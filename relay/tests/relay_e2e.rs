use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::{SinkExt, StreamExt};
use masil_relay::{RelayConfig, RelayState, build_router};
use masil_wire::noise::{HandshakeParams, InitiatorHandshake, Keypair, ResponderHandshake};
use masil_wire::record::{RecordKind, open_record, seal_record};
use masil_wire::rpc::{
    HandshakeEnvelope, LiveActivityEventKind, LiveActivityPayload, PushDeviceToken,
    PushNotificationRequest, RelayControlMessage,
};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

async fn spawn_relay() -> String {
    let state = Arc::new(RelayState::new(RelayConfig::default()));
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("{addr}")
}

#[tokio::test]
async fn e2ee_noise_nk_roundtrip_through_relay_and_hijack_rejected() {
    let addr = spawn_relay().await;
    let host_token = "host-secret-token-1234567890";
    let host_keypair = Keypair::generate();

    // 1. Host registers.
    let host_url = format!("ws://{addr}/v1/host/mac-dev?token={host_token}");
    let (mut host_ws, _) = connect_async(&host_url).await.unwrap();

    // 2. Another host with wrong token cannot hijack `mac-dev`.
    let bad_url = format!("ws://{addr}/v1/host/mac-dev?token=wrong-token-1234567890");
    assert!(connect_async(&bad_url).await.is_err());

    // 3. Mobile client connects.
    let client_url = format!("ws://{addr}/v1/client/mac-dev");
    let (mut client_ws, _) = connect_async(&client_url).await.unwrap();

    // Host receives SessionOpen.
    let open_raw = host_ws.next().await.unwrap().unwrap().into_text().unwrap();
    let session_id = match serde_json::from_str::<RelayControlMessage>(&open_raw).unwrap() {
        RelayControlMessage::SessionOpen { session_id } => session_id,
        other => panic!("expected SessionOpen, got {other:?}"),
    };

    // 4. Client initiates Noise NK handshake.
    let (init_hs, msg1) =
        InitiatorHandshake::start(host_keypair.public_bytes(), HandshakeParams::new(true)).unwrap();
    let hello = serde_json::to_string(&HandshakeEnvelope::E2eeHello {
        v: 1,
        message: STANDARD.encode(&msg1),
    })
    .unwrap();
    client_ws.send(Message::Text(hello.into())).await.unwrap();

    // Host receives framed E2eeHello, responds with E2eeReady.
    let host_in = host_ws.next().await.unwrap().unwrap().into_text().unwrap();
    let hello_text = match serde_json::from_str::<RelayControlMessage>(&host_in).unwrap() {
        RelayControlMessage::Frame {
            session_id: sid,
            kind,
            data,
        } => {
            assert_eq!(sid, session_id);
            assert_eq!(kind, "text");
            data
        }
        other => panic!("expected Frame, got {other:?}"),
    };
    let msg1_decoded = match serde_json::from_str::<HandshakeEnvelope>(&hello_text).unwrap() {
        HandshakeEnvelope::E2eeHello { v: 1, message } => STANDARD.decode(message).unwrap(),
        other => panic!("expected E2eeHello, got {other:?}"),
    };
    let (mut host_tx, msg2) =
        ResponderHandshake::accept(&host_keypair, &msg1_decoded, true).unwrap();
    let ready_text = serde_json::to_string(&HandshakeEnvelope::E2eeReady {
        v: 1,
        message: STANDARD.encode(&msg2),
    })
    .unwrap();
    let relay_reply = serde_json::to_string(&RelayControlMessage::Frame {
        session_id: session_id.clone(),
        kind: "text".into(),
        data: ready_text,
    })
    .unwrap();
    host_ws
        .send(Message::Text(relay_reply.into()))
        .await
        .unwrap();

    // Client finishes handshake.
    let client_in = client_ws
        .next()
        .await
        .unwrap()
        .unwrap()
        .into_text()
        .unwrap();
    let msg2_decoded = match serde_json::from_str::<HandshakeEnvelope>(&client_in).unwrap() {
        HandshakeEnvelope::E2eeReady { v: 1, message } => STANDARD.decode(message).unwrap(),
        other => panic!("expected E2eeReady, got {other:?}"),
    };
    let mut client_tx = init_hs.finish(&msg2_decoded).unwrap();
    assert_eq!(client_tx.handshake_hash, host_tx.handshake_hash);

    // 5. Encrypted binary record flows client -> host through relay.
    let wire = seal_record(
        &mut client_tx,
        RecordKind::Text,
        br#"{"v":1,"type":"ping"}"#,
    )
    .unwrap();
    client_ws.send(Message::Binary(wire.into())).await.unwrap();

    let host_frame = host_ws.next().await.unwrap().unwrap().into_text().unwrap();
    let cipher_bytes = match serde_json::from_str::<RelayControlMessage>(&host_frame).unwrap() {
        RelayControlMessage::Frame { kind, data, .. } => {
            assert_eq!(kind, "binary");
            STANDARD.decode(data).unwrap()
        }
        other => panic!("expected binary Frame, got {other:?}"),
    };
    let (rk, pt) = open_record(&mut host_tx, &cipher_bytes).unwrap();
    assert_eq!(rk, RecordKind::Text);
    assert_eq!(pt, br#"{"v":1,"type":"ping"}"#);

    // 6. Push endpoint validates host token and reports unconfigured cleanly.
    let http = reqwest::Client::new();
    let push_resp = http
        .post(format!("http://{addr}/v1/push"))
        .bearer_auth(host_token)
        .json(&PushNotificationRequest {
            host_id: "mac-dev".into(),
            host_name: "Mac".into(),
            agent_id: "%0".into(),
            agent_name: "builder".into(),
            event_kind: "approval_requested".into(),
            occurred_ms: 1790000000000,
            tokens: vec![PushDeviceToken {
                platform: "ios".into(),
                token: "dummy-apns-token".into(),
                push_type: None,
            }],
            live_activity: None,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(push_resp.status(), 200);
    let body: serde_json::Value = push_resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["sent"], 0);
    assert_eq!(body["skipped"], 1);
    assert_eq!(body["configured"], false);

    // 7. Live Activity minimal-metadata push accepts whitelisted fields and rejects
    //    any smuggled prompt/code fields (D-09 zero-code-leak schema assertion).
    let live_resp = http
        .post(format!("http://{addr}/v1/push"))
        .bearer_auth(host_token)
        .json(&PushNotificationRequest {
            host_id: "mac-dev".into(),
            host_name: "Mac".into(),
            agent_id: "%0".into(),
            agent_name: "builder".into(),
            event_kind: "approval_requested".into(),
            occurred_ms: 1790000000100,
            tokens: vec![
                PushDeviceToken {
                    platform: "ios".into(),
                    token: "live-act-token".into(),
                    push_type: Some("liveactivity".into()),
                },
                PushDeviceToken {
                    platform: "android".into(),
                    token: "fcm-ongoing-token".into(),
                    push_type: Some("android_ongoing".into()),
                },
            ],
            live_activity: Some(LiveActivityPayload {
                host_id: "mac-dev".into(),
                host_name: "Mac".into(),
                working_count: 2,
                blocked_count: 1,
                done_count: 0,
                alert_count: 1,
                top_agent_alias: Some("builder".into()),
                top_event_kind: Some(LiveActivityEventKind::ApprovalRequested),
                top_summary_line: None,
                deep_link: "masil://host/mac-dev/agent/%250".into(),
                updated_at: 1790000000,
            }),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(live_resp.status(), 200);
    let live_body: serde_json::Value = live_resp.json().await.unwrap();
    assert_eq!(live_body["ok"], true);
    assert_eq!(live_body["skipped"], 2);

    // Smuggling prompt/code into live_activity must be rejected with 422.
    let smuggled_resp = http
        .post(format!("http://{addr}/v1/push"))
        .bearer_auth(host_token)
        .json(&serde_json::json!({
            "host_id": "mac-dev",
            "host_name": "Mac",
            "agent_id": "%0",
            "agent_name": "builder",
            "event_kind": "approval_requested",
            "occurred_ms": 1790000000200i64,
            "tokens": [],
            "live_activity": {
                "host_id": "mac-dev",
                "host_name": "Mac",
                "working_count": 1,
                "blocked_count": 0,
                "done_count": 0,
                "alert_count": 0,
                "deep_link": "masil://host/mac-dev",
                "raw_prompt": "sk-secret-must-never-cross-relay"
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(smuggled_resp.status(), 422);

    // Oversized summary (>60 chars) must be rejected with 400.
    let bad_summary_resp = http
        .post(format!("http://{addr}/v1/push"))
        .bearer_auth(host_token)
        .json(&PushNotificationRequest {
            host_id: "mac-dev".into(),
            host_name: "Mac".into(),
            agent_id: "%0".into(),
            agent_name: "builder".into(),
            event_kind: "approval_requested".into(),
            occurred_ms: 1790000000300,
            tokens: vec![],
            live_activity: Some(LiveActivityPayload {
                host_id: "mac-dev".into(),
                host_name: "Mac".into(),
                working_count: 1,
                blocked_count: 0,
                done_count: 0,
                alert_count: 0,
                top_agent_alias: Some("builder".into()),
                top_event_kind: Some(LiveActivityEventKind::TurnFinished),
                top_summary_line: Some("a".repeat(65)),
                deep_link: "masil://host/mac-dev".into(),
                updated_at: 1790000000,
            }),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(bad_summary_resp.status(), 400);
}
