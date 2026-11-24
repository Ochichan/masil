use masil_relay::{RelayConfig, RelayState, build_router};
use std::sync::Arc;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    let config = RelayConfig::from_env();
    let listen = config.listen.clone();
    let state = Arc::new(RelayState::new(config));
    let app = build_router(state);

    let listener = TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|e| panic!("masil-relay failed to bind {listen}: {e}"));
    eprintln!("masil-relay listening on {listen}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap_or_else(|e| panic!("masil-relay server error: {e}"));
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    eprintln!("masil-relay shutting down");
}
