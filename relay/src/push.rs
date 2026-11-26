//! Optional minimal-metadata APNs and FCM push notification bridge.
//! Never accepts or forwards prompt/code contents; only host/agent identity,
//! event kind, and deep-link coordinates cross the push path.

use crate::broker::{RelayState, extract_bearer};
use crate::config::{ApnsConfig, FcmConfig};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use masil_wire::rpc::{LiveActivityPayload, PushNotificationRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize)]
struct ApnsClaims<'a> {
    iss: &'a str,
    iat: u64,
}

#[derive(Serialize)]
struct FcmJwtClaims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

#[derive(Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn event_label(kind: &str) -> &'static str {
    match kind {
        "approval_requested" => "Approval requested",
        "critical_risk_permission" => "Critical approval requested",
        "question_asked" => "Question waiting",
        "blocked" => "Needs input",
        "loop_paused" | "circuit_breaker_tripped" => "Loop paused",
        "hard_conflict" | "same_directory_race" => "Worktree conflict",
        "bs_flagged" => "Audit warning",
        "turn_completed" | "returned_idle" | "turn_finished" => "Turn finished",
        "operation_unknown" => "Outcome unknown",
        "missed_schedule" => "Missed schedule",
        _ => "Attention needed",
    }
}

/// Builds the strictly whitelisted APNs `liveactivity` JSON envelope and topic
/// header (`<bundle_id>.push-type.liveactivity`) per D-09.
pub fn build_apns_live_activity_payload(
    bundle_id: &str,
    payload: &LiveActivityPayload,
    is_start: bool,
    timestamp_secs: u64,
) -> (String, Value) {
    let clean = payload.sanitized();
    let topic = format!("{bundle_id}.push-type.liveactivity");
    let mut content_state = json!({
        "workingCount": clean.working_count,
        "blockedCount": clean.blocked_count,
        "doneCount": clean.done_count,
        "alertCount": clean.alert_count,
        "deepLink": clean.deep_link,
        "updatedAt": if clean.updated_at > 0 { clean.updated_at } else { timestamp_secs },
    });
    if let Some(obj) = content_state.as_object_mut() {
        if let Some(alias) = clean.top_agent_alias {
            obj.insert("topAgentAlias".into(), json!(alias));
        }
        if let Some(kind) = clean.top_event_kind {
            obj.insert("topEventKind".into(), json!(kind.as_str()));
        }
        if let Some(summary) = clean.top_summary_line {
            obj.insert("topSummaryLine".into(), json!(summary));
        }
    }
    let mut aps = json!({
        "timestamp": timestamp_secs,
        "event": if is_start { "start" } else { "update" },
        "content-state": content_state,
    });
    if is_start && let Some(obj) = aps.as_object_mut() {
        obj.insert("attributes-type".into(), json!("MasilFleetAttributes"));
        obj.insert(
            "attributes".into(),
            json!({
                "hostId": clean.host_id,
                "hostName": clean.host_name,
            }),
        );
    }
    (topic, json!({ "aps": aps }))
}

/// Builds a silent data-only FCM payload for the Android Ongoing Fleet Pulse bar (D-09).
pub fn build_fcm_ongoing_payload(device_token: &str, payload: &LiveActivityPayload) -> Value {
    let clean = payload.sanitized();
    json!({
        "message": {
            "token": device_token,
            "data": {
                "kind": "live_pulse",
                "host_id": clean.host_id,
                "host_name": clean.host_name,
                "working_count": clean.working_count.to_string(),
                "blocked_count": clean.blocked_count.to_string(),
                "done_count": clean.done_count.to_string(),
                "alert_count": clean.alert_count.to_string(),
                "top_agent_alias": clean.top_agent_alias.unwrap_or_default(),
                "top_event_kind": clean.top_event_kind.map(|k| k.as_str()).unwrap_or(""),
                "top_summary_line": clean.top_summary_line.unwrap_or_default(),
                "deep_link": clean.deep_link,
                "updated_at": clean.updated_at.to_string(),
            },
            "android": { "priority": "normal" }
        }
    })
}

pub async fn handle_push(
    headers: HeaderMap,
    State(state): State<Arc<RelayState>>,
    Json(req): Json<PushNotificationRequest>,
) -> impl IntoResponse {
    let Some(token) = extract_bearer(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok": false, "error": "missing_host_token"})),
        );
    };
    if !state.verify_host_token(&req.host_id, &token).await {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "error": "invalid_host_token"})),
        );
    }
    if let Some(ref la) = req.live_activity
        && let Err(reason) = la.validate()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": format!("invalid_live_activity: {reason}")})),
        );
    }
    (StatusCode::OK, Json(dispatch_push(&state, &req).await))
}

pub async fn dispatch_push(state: &RelayState, req: &PushNotificationRequest) -> Value {
    if req.tokens.is_empty() {
        return json!({
            "ok": true,
            "sent": 0,
            "skipped": 0,
            "configured": state.config.apns.is_some() || state.config.fcm.is_some(),
        });
    }

    if let Some(ref la) = req.live_activity
        && let Err(reason) = la.validate()
    {
        return json!({
            "ok": false,
            "sent": 0,
            "skipped": req.tokens.len(),
            "errors": [{"error": format!("invalid_live_activity: {reason}")}],
            "configured": state.config.apns.is_some() || state.config.fcm.is_some(),
        });
    }

    let title = format!("masil · {}", req.host_name);
    let body = format!("{}: {}", req.agent_name, event_label(&req.event_kind));
    let deep_link = format!("masil://host/{}/agent/{}", req.host_id, req.agent_id);

    let mut sent = 0usize;
    let mut skipped = 0usize;
    let mut errors: Vec<Value> = Vec::new();

    for device in &req.tokens {
        let ptype = device.push_type.as_deref().unwrap_or("alert");
        match (device.platform.as_str(), ptype) {
            ("ios", "liveactivity" | "liveactivity_start") => {
                if let (Some(apns), Some(la)) = (&state.config.apns, &req.live_activity) {
                    let is_start = ptype == "liveactivity_start";
                    match send_apns_live_activity(&state.http, apns, &device.token, la, is_start)
                        .await
                    {
                        Ok(()) => sent += 1,
                        Err(err) => errors
                            .push(json!({"platform": "ios", "push_type": ptype, "error": err})),
                    }
                } else {
                    skipped += 1;
                }
            }
            ("android", "android_ongoing") => {
                if let (Some(fcm), Some(la)) = (&state.config.fcm, &req.live_activity) {
                    match send_fcm_ongoing(&state.http, fcm, &device.token, la).await {
                        Ok(()) => sent += 1,
                        Err(err) => errors
                            .push(json!({"platform": "android", "push_type": ptype, "error": err})),
                    }
                } else {
                    skipped += 1;
                }
            }
            ("ios", _) => {
                if let Some(ref apns) = state.config.apns {
                    match send_apns(
                        &state.http,
                        apns,
                        &device.token,
                        &title,
                        &body,
                        &deep_link,
                        req,
                    )
                    .await
                    {
                        Ok(()) => sent += 1,
                        Err(err) => errors.push(json!({"platform": "ios", "error": err})),
                    }
                } else {
                    skipped += 1;
                }
            }
            ("android", _) => {
                if let Some(ref fcm) = state.config.fcm {
                    match send_fcm(
                        &state.http,
                        fcm,
                        &device.token,
                        &title,
                        &body,
                        &deep_link,
                        req,
                    )
                    .await
                    {
                        Ok(()) => sent += 1,
                        Err(err) => errors.push(json!({"platform": "android", "error": err})),
                    }
                } else {
                    skipped += 1;
                }
            }
            _ => skipped += 1,
        }
    }

    json!({
        "ok": errors.is_empty(),
        "sent": sent,
        "skipped": skipped,
        "errors": errors,
        "configured": state.config.apns.is_some() || state.config.fcm.is_some(),
    })
}

async fn send_apns(
    http: &reqwest::Client,
    cfg: &ApnsConfig,
    device_token: &str,
    title: &str,
    body: &str,
    deep_link: &str,
    req: &PushNotificationRequest,
) -> Result<(), String> {
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(cfg.key_id.clone());
    let key = EncodingKey::from_ec_pem(cfg.key_pem.as_bytes())
        .map_err(|e| format!("invalid apns key: {e}"))?;
    let jwt = jsonwebtoken::encode(
        &header,
        &ApnsClaims {
            iss: &cfg.team_id,
            iat: now_secs(),
        },
        &key,
    )
    .map_err(|e| format!("apns jwt encode failed: {e}"))?;

    let host = if cfg.sandbox {
        "https://api.sandbox.push.apple.com"
    } else {
        "https://api.push.apple.com"
    };
    let url = format!("{host}/3/device/{device_token}");
    let payload = json!({
        "aps": {
            "alert": { "title": title, "body": body },
            "sound": "default",
            "thread-id": req.host_id,
        },
        "masil": {
            "host_id": req.host_id,
            "agent_id": req.agent_id,
            "event_kind": req.event_kind,
            "deep_link": deep_link,
        }
    });

    let resp = http
        .post(url)
        .header("authorization", format!("bearer {jwt}"))
        .header("apns-topic", &cfg.bundle_id)
        .header("apns-push-type", "alert")
        .header("apns-priority", "10")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("apns request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("apns status {}", resp.status()));
    }
    Ok(())
}

async fn send_fcm(
    http: &reqwest::Client,
    cfg: &FcmConfig,
    device_token: &str,
    title: &str,
    body: &str,
    deep_link: &str,
    req: &PushNotificationRequest,
) -> Result<(), String> {
    let now = now_secs();
    let header = Header::new(Algorithm::RS256);
    let key = EncodingKey::from_rsa_pem(cfg.private_key_pem.as_bytes())
        .map_err(|e| format!("invalid fcm key: {e}"))?;
    let assertion = jsonwebtoken::encode(
        &header,
        &FcmJwtClaims {
            iss: &cfg.client_email,
            scope: "https://www.googleapis.com/auth/firebase.messaging",
            aud: "https://oauth2.googleapis.com/token",
            iat: now,
            exp: now + 3000,
        },
        &key,
    )
    .map_err(|e| format!("fcm jwt encode failed: {e}"))?;

    let oauth = http
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("fcm oauth failed: {e}"))?;
    if !oauth.status().is_success() {
        return Err(format!("fcm oauth status {}", oauth.status()));
    }
    let token_resp: OAuthTokenResponse = oauth
        .json()
        .await
        .map_err(|e| format!("fcm oauth parse failed: {e}"))?;

    let url = format!(
        "https://fcm.googleapis.com/v1/projects/{}/messages:send",
        cfg.project_id
    );
    let payload = json!({
        "message": {
            "token": device_token,
            "notification": { "title": title, "body": body },
            "data": {
                "host_id": req.host_id,
                "agent_id": req.agent_id,
                "event_kind": req.event_kind,
                "deep_link": deep_link,
            },
            "android": { "priority": "high" }
        }
    });
    let resp = http
        .post(url)
        .bearer_auth(&token_resp.access_token)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("fcm send failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("fcm status {}", resp.status()));
    }
    Ok(())
}

async fn send_apns_live_activity(
    http: &reqwest::Client,
    cfg: &ApnsConfig,
    activity_token: &str,
    live_activity: &LiveActivityPayload,
    is_start: bool,
) -> Result<(), String> {
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(cfg.key_id.clone());
    let key = EncodingKey::from_ec_pem(cfg.key_pem.as_bytes())
        .map_err(|e| format!("invalid apns key: {e}"))?;
    let now = now_secs();
    let jwt = jsonwebtoken::encode(
        &header,
        &ApnsClaims {
            iss: &cfg.team_id,
            iat: now,
        },
        &key,
    )
    .map_err(|e| format!("apns jwt encode failed: {e}"))?;

    let host = if cfg.sandbox {
        "https://api.sandbox.push.apple.com"
    } else {
        "https://api.push.apple.com"
    };
    let url = format!("{host}/3/device/{activity_token}");
    let (topic, payload) =
        build_apns_live_activity_payload(&cfg.bundle_id, live_activity, is_start, now);

    let resp = http
        .post(url)
        .header("authorization", format!("bearer {jwt}"))
        .header("apns-topic", topic)
        .header("apns-push-type", "liveactivity")
        .header("apns-priority", "10")
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("apns liveactivity request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("apns liveactivity status {}", resp.status()));
    }
    Ok(())
}

async fn send_fcm_ongoing(
    http: &reqwest::Client,
    cfg: &FcmConfig,
    device_token: &str,
    live_activity: &LiveActivityPayload,
) -> Result<(), String> {
    let now = now_secs();
    let header = Header::new(Algorithm::RS256);
    let key = EncodingKey::from_rsa_pem(cfg.private_key_pem.as_bytes())
        .map_err(|e| format!("invalid fcm key: {e}"))?;
    let assertion = jsonwebtoken::encode(
        &header,
        &FcmJwtClaims {
            iss: &cfg.client_email,
            scope: "https://www.googleapis.com/auth/firebase.messaging",
            aud: "https://oauth2.googleapis.com/token",
            iat: now,
            exp: now + 3000,
        },
        &key,
    )
    .map_err(|e| format!("fcm jwt encode failed: {e}"))?;

    let oauth = http
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("fcm oauth failed: {e}"))?;
    if !oauth.status().is_success() {
        return Err(format!("fcm oauth status {}", oauth.status()));
    }
    let token_resp: OAuthTokenResponse = oauth
        .json()
        .await
        .map_err(|e| format!("fcm oauth parse failed: {e}"))?;

    let url = format!(
        "https://fcm.googleapis.com/v1/projects/{}/messages:send",
        cfg.project_id
    );
    let payload = build_fcm_ongoing_payload(device_token, live_activity);
    let resp = http
        .post(url)
        .bearer_auth(&token_resp.access_token)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("fcm ongoing send failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("fcm ongoing status {}", resp.status()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use masil_wire::rpc::LiveActivityEventKind;

    #[test]
    fn apns_live_activity_payload_matches_apple_contract_and_caps_fields() {
        let la = LiveActivityPayload {
            host_id: "mac-studio".into(),
            host_name: "Mac Studio".into(),
            working_count: 3,
            blocked_count: 1,
            done_count: 2,
            alert_count: 1,
            top_agent_alias: Some("builder-auth".into()),
            top_event_kind: Some(LiveActivityEventKind::ApprovalRequested),
            top_summary_line: Some("Run cargo test".into()),
            deep_link: "masil://host/mac-studio/agent/%251".into(),
            updated_at: 1_790_000_123,
        };
        assert!(la.validate().is_ok());

        let (topic, body) = build_apns_live_activity_payload("dev.masil.app", &la, false, 100);
        assert_eq!(topic, "dev.masil.app.push-type.liveactivity");
        assert_eq!(body["aps"]["event"], "update");
        assert_eq!(body["aps"]["content-state"]["workingCount"], 3);
        assert_eq!(body["aps"]["content-state"]["blockedCount"], 1);
        assert_eq!(
            body["aps"]["content-state"]["topAgentAlias"],
            "builder-auth"
        );
        assert_eq!(
            body["aps"]["content-state"]["topEventKind"],
            "approval_requested"
        );
        assert_eq!(
            body["aps"]["content-state"]["topSummaryLine"],
            "Run cargo test"
        );

        let (_, start_body) = build_apns_live_activity_payload("dev.masil.app", &la, true, 100);
        assert_eq!(start_body["aps"]["event"], "start");
        assert_eq!(start_body["aps"]["attributes-type"], "MasilFleetAttributes");
        assert_eq!(start_body["aps"]["attributes"]["hostId"], "mac-studio");
    }

    #[test]
    fn fcm_ongoing_payload_is_data_only_without_alert_banner() {
        let la = LiveActivityPayload {
            host_id: "mac-studio".into(),
            host_name: "Mac Studio".into(),
            working_count: 2,
            blocked_count: 0,
            done_count: 4,
            alert_count: 0,
            top_agent_alias: None,
            top_event_kind: Some(LiveActivityEventKind::TurnFinished),
            top_summary_line: None,
            deep_link: "masil://host/mac-studio".into(),
            updated_at: 1_790_000_200,
        };
        let body = build_fcm_ongoing_payload("fcm-tok-1", &la);
        assert!(body["message"].get("notification").is_none());
        assert_eq!(body["message"]["data"]["kind"], "live_pulse");
        assert_eq!(body["message"]["data"]["working_count"], "2");
        assert_eq!(body["message"]["data"]["top_summary_line"], "");
    }
}
