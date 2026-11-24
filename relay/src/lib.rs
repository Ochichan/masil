//! Zero-knowledge WebSocket relay and optional minimal-metadata APNs/FCM push
//! bridge for `masil-agent remote` hosts and `masil` mobile clients.

pub mod broker;
pub mod config;
pub mod push;

use axum::Router;
use axum::routing::{get, post};
use std::sync::Arc;

pub use broker::RelayState;
pub use config::RelayConfig;

pub fn build_router(state: Arc<RelayState>) -> Router {
    Router::new()
        .route("/healthz", get(broker::healthz))
        .route("/v1/host/{host_id}", get(broker::host_ws))
        .route("/v1/client/{host_id}", get(broker::client_ws))
        .route("/v1/push", post(push::handle_push))
        .with_state(state)
}
