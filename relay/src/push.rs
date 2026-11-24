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
use masil_wire::rpc::PushNotificationRequest;
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
        "question_asked" => "Question waiting",
        "blocked" => "Needs input",
        "turn_completed" | "returned_idle" => "Turn finished",
        "operation_unknown" => "Outcome unknown",
        "missed_schedule" => "Missed schedule",
        _ => "Attention needed",
    }
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

    let title = format!("masil · {}", req.host_name);
    let body = format!("{}: {}", req.agent_name, event_label(&req.event_kind));
    let deep_link = format!("masil://host/{}/agent/{}", req.host_id, req.agent_id);

    let mut sent = 0usize;
    let mut skipped = 0usize;
    let mut errors: Vec<Value> = Vec::new();

    for device in &req.tokens {
        match device.platform.as_str() {
            "ios" => {
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
            "android" => {
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
