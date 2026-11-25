use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use masil_wire::frames::{
    BinaryFrame, BinaryOpcode, ResizeMode, TerminalResizePayload, TerminalSnapshotMeta,
};
use masil_wire::noise::{
    HandshakeParams, InitiatorHandshake, Keypair, NoiseError, ResponderHandshake,
};
use masil_wire::pairing::PairingOffer;
use masil_wire::record::{MAX_CHUNK_DATA, RecordError, RecordKind, open_record, seal_record};
use serde_json::{Value, json};
use std::path::PathBuf;

#[test]
fn noise_nk_roundtrip_and_replay_rejection() {
    let responder_static = Keypair::from_secret_bytes([0x11; 32]);
    let initiator_ephemeral = Keypair::from_secret_bytes([0x22; 32]);
    let responder_ephemeral = Keypair::from_secret_bytes([0x33; 32]);

    let (init_hs, msg1) = InitiatorHandshake::start_with_ephemeral(
        responder_static.public_bytes(),
        initiator_ephemeral,
        HandshakeParams::new(true),
    )
    .unwrap();
    let (mut resp_tx, msg2) = ResponderHandshake::accept_with_ephemeral(
        &responder_static,
        responder_ephemeral,
        &msg1,
        true,
    )
    .unwrap();
    let mut init_tx = init_hs.finish(&msg2).unwrap();

    assert_eq!(init_tx.handshake_hash, resp_tx.handshake_hash);
    assert!(init_tx.params.binary_ciphertext());
    assert!(resp_tx.params.binary_ciphertext());

    let wire1 = seal_record(&mut init_tx, RecordKind::Text, br#"{"v":1,"type":"ping"}"#).unwrap();
    let (kind, plain1) = open_record(&mut resp_tx, &wire1).unwrap();
    assert_eq!(kind, RecordKind::Text);
    assert_eq!(plain1, br#"{"v":1,"type":"ping"}"#);

    // Replaying wire1 must fail AEAD authentication because nonce advanced.
    assert!(matches!(
        open_record(&mut resp_tx, &wire1),
        Err(RecordError::Noise(NoiseError::CryptoFailure))
    ));

    // Responder -> Initiator large multi-chunk binary payload.
    let big_payload = vec![0xAB; MAX_CHUNK_DATA * 2 + 123];
    let wire2 = seal_record(&mut resp_tx, RecordKind::Binary, &big_payload).unwrap();
    let (kind2, plain2) = open_record(&mut init_tx, &wire2).unwrap();
    assert_eq!(kind2, RecordKind::Binary);
    assert_eq!(plain2, big_payload);
}

#[test]
fn rejects_tampered_and_truncated_records() {
    let rs = Keypair::generate();
    let (init_hs, msg1) =
        InitiatorHandshake::start(rs.public_bytes(), HandshakeParams::new(true)).unwrap();
    let (mut resp_tx, msg2) = ResponderHandshake::accept(&rs, &msg1, true).unwrap();
    let mut init_tx = init_hs.finish(&msg2).unwrap();

    let mut wire = seal_record(&mut init_tx, RecordKind::Text, b"hello").unwrap();
    let last = wire.len() - 1;
    wire[last] ^= 0xFF;
    assert!(matches!(
        open_record(&mut resp_tx, &wire),
        Err(RecordError::Noise(NoiseError::CryptoFailure))
    ));
    // Nonce must NOT have advanced on failed decryption.
    assert_eq!(resp_tx.recv.nonce(), 0);
    // Truncated record rejected cleanly.
    assert!(matches!(
        open_record(&mut resp_tx, &wire[..4]),
        Err(RecordError::TruncatedFrame)
    ));
}

#[test]
fn binary_frame_and_snapshot_roundtrip() {
    let meta = TerminalSnapshotMeta {
        cols: 180,
        rows: 50,
        cursor_x: 12,
        cursor_y: 48,
        desktop_cols: 180,
        desktop_rows: 50,
        fit_active: false,
    };
    let payload = meta.encode_with_ansi(b"\x1b[32mhello\x1b[0m").unwrap();
    let frame = BinaryFrame::new(BinaryOpcode::TerminalSnapshot, 7, payload);
    let decoded = BinaryFrame::decode(&frame.encode()).unwrap();
    assert_eq!(decoded.opcode, BinaryOpcode::TerminalSnapshot);
    assert_eq!(decoded.channel_id, 7);
    let (decoded_meta, ansi) = TerminalSnapshotMeta::decode_with_ansi(&decoded.payload).unwrap();
    assert_eq!(decoded_meta, meta);
    assert_eq!(ansi, b"\x1b[32mhello\x1b[0m");

    let resize = TerminalResizePayload {
        cols: 58,
        rows: 36,
        mode: ResizeMode::Fit,
    };
    let rjson = serde_json::to_vec(&resize).unwrap();
    let rframe = BinaryFrame::new(BinaryOpcode::TerminalResize, 7, rjson);
    assert_eq!(BinaryFrame::decode(&rframe.encode()).unwrap(), rframe);
}

#[test]
fn pairing_uri_roundtrip() {
    let offer = PairingOffer {
        version: 1,
        host_id: "macbook-pro".into(),
        host_pub: [0x42; 32],
        pair_token: "tok_abc-123".into(),
        relay_url: Some("wss://relay.example.com/v1".into()),
        direct_addr: Some("100.64.0.10:7767".into()),
        host_name: "Ochi MacBook".into(),
    };
    let uri = offer.to_uri().unwrap();
    let parsed = PairingOffer::parse_uri(&uri).unwrap();
    assert_eq!(parsed, offer);
}

#[test]
fn cross_language_vectors_match_checked_in_fixture() {
    let responder_static = Keypair::from_secret_bytes([
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e,
        0x1f, 0x20,
    ]);
    let initiator_ephemeral = Keypair::from_secret_bytes([
        0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f,
        0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e,
        0x3f, 0x40,
    ]);
    let responder_ephemeral = Keypair::from_secret_bytes([
        0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f,
        0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e,
        0x5f, 0x60,
    ]);

    let (init_hs, msg1) = InitiatorHandshake::start_with_ephemeral(
        responder_static.public_bytes(),
        initiator_ephemeral.clone(),
        HandshakeParams::new(true),
    )
    .unwrap();
    let (mut resp_tx, msg2) = ResponderHandshake::accept_with_ephemeral(
        &responder_static,
        responder_ephemeral.clone(),
        &msg1,
        true,
    )
    .unwrap();
    let mut init_tx = init_hs.finish(&msg2).unwrap();

    let text_plain = br#"{"v":1,"id":"1","type":"agent.list.request","params":{}}"#;
    let text_wire = seal_record(&mut init_tx, RecordKind::Text, text_plain).unwrap();
    let (k1, d1) = open_record(&mut resp_tx, &text_wire).unwrap();
    assert_eq!(k1, RecordKind::Text);
    assert_eq!(d1, text_plain);

    let bin_plain = BinaryFrame::new(
        BinaryOpcode::TerminalOutput,
        3,
        b"\x1b[1mhi\x1b[0m".to_vec(),
    )
    .encode();
    let bin_wire = seal_record(&mut resp_tx, RecordKind::Binary, &bin_plain).unwrap();
    let (k2, d2) = open_record(&mut init_tx, &bin_wire).unwrap();
    assert_eq!(k2, RecordKind::Binary);
    assert_eq!(d2, bin_plain);

    let vector = json!({
        "protocol_name": "Noise_NK_25519_ChaChaPoly_SHA256",
        "prologue": "masil-remote-e2ee/1",
        "responder_static_secret_b64": STANDARD.encode(responder_static.secret_bytes()),
        "responder_static_public_b64": STANDARD.encode(responder_static.public_bytes()),
        "initiator_ephemeral_secret_b64": STANDARD.encode(initiator_ephemeral.secret_bytes()),
        "responder_ephemeral_secret_b64": STANDARD.encode(responder_ephemeral.secret_bytes()),
        "msg1_b64": STANDARD.encode(&msg1),
        "msg2_b64": STANDARD.encode(&msg2),
        "handshake_hash_b64": STANDARD.encode(init_tx.handshake_hash),
        "initiator_to_responder_text_plain_b64": STANDARD.encode(text_plain),
        "initiator_to_responder_text_wire_b64": STANDARD.encode(&text_wire),
        "responder_to_initiator_binary_plain_b64": STANDARD.encode(&bin_plain),
        "responder_to_initiator_binary_wire_b64": STANDARD.encode(&bin_wire),
    });

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors.json");
    if std::env::var("MASIL_UPDATE_VECTORS").as_deref() == Ok("1") {
        std::fs::write(&path, serde_json::to_string_pretty(&vector).unwrap() + "\n").unwrap();
    }
    let checked_in: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("tests/vectors.json exists"))
            .unwrap();
    assert_eq!(vector, checked_in);
}

#[test]
fn timeline_plugin_card_roundtrip() {
    let item = masil_wire::rpc::TimelineItem::PluginCard {
        id: "plugin:repo-pulse:status".into(),
        seq: 42,
        ts_ms: 1_700_000_000_000,
        plugin_id: "repo-pulse".into(),
        card_id: "status".into(),
        title: "Repo Pulse".into(),
        subtitle: Some("main · 2 dirty".into()),
        blocks: vec![json!({
            "type": "metric_row",
            "items": [{"label": "Branch", "value": "main", "tone": "accent"}]
        })],
    };
    let encoded = serde_json::to_string(&item).unwrap();
    assert!(encoded.contains("\"kind\":\"plugin_card\""));
    let decoded: masil_wire::rpc::TimelineItem = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, item);
}
