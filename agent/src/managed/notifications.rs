//! `agent notify ...` (P7a, docs/notifications.md): switches the
//! coordinator's notifications for this server, shows how they went, and
//! sends a test through the routes.

use super::Manager;
use super::operations::Store;
use crate::notify::Settings;
use serde_json::{Value, json};

fn usage() -> String {
    "usage: masil-agent agent notify enable | disable | status | test".into()
}

pub(super) async fn command(manager: &Manager, args: &[String]) -> Result<Value, String> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["enable"] => enable(manager).await,
        ["disable"] => disable(manager).await,
        ["status"] => status(manager).await,
        ["test"] => test(manager).await,
        _ => Err(usage()),
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| error.to_string())?
}

/// Notifications on: the inbox too (they read it), the environment kept,
/// and a coordinator running. Nothing recorded before now is sent.
async fn enable(manager: &Manager) -> Result<Value, String> {
    let socket = manager.native.socket.clone();
    blocking(move || crate::coordinator::check_state(&socket).map(drop)).await?;
    let settings = Settings::load()?;
    // The coordinator's own environment is narrower than this command's.
    let env = crate::notify::kept_environment();
    let inbox_was_on = manager.operation_store().await?.inbox_enabled()?;
    if !inbox_was_on {
        super::inbox::set_enabled(manager, true).await?;
    }
    manager.operation_store().await?.set_notify(true, &env)?;
    let socket = manager.native.socket.clone();
    let coordinator = blocking(move || {
        Ok(match crate::coordinator::ensure(&socket) {
            Ok(_) => {
                crate::coordinator::reload(&socket);
                "running".to_owned()
            }
            Err(error) => format!("error: {error}"),
        })
    })
    .await?;
    let mut value = json!({
        "notify": "on",
        "inbox": if inbox_was_on { "on" } else { "switched_on" },
        "routes": settings.routes(),
        "events": settings.events,
        "coordinator": coordinator,
    });
    if std::env::var_os("MASIL_AGENT_RUN").is_some() {
        value["warning"] = json!(
            "run inside an agent pane: the environment kept for OS notifications and the hook is this pane's"
        );
    }
    Ok(value)
}

async fn disable(manager: &Manager) -> Result<Value, String> {
    manager.operation_store().await?.set_notify(false, &[])?;
    let socket = manager.native.socket.clone();
    blocking(move || {
        crate::coordinator::reload(&socket);
        Ok(())
    })
    .await?;
    Ok(json!({"notify": "off"}))
}

/// Read-only: creates no store and starts nothing.
async fn status(manager: &Manager) -> Result<Value, String> {
    let socket = manager.native.socket.clone();
    let stored = blocking(move || Store::notify_readonly(&socket)).await?;
    let mut value = stored.unwrap_or_else(|| {
        json!({"enabled": false, "inbox_enabled": false, "cursor": 0, "interrupted": 0, "recent": []})
    });
    match Settings::load() {
        Ok(settings) => {
            value["routes"] = json!(settings.routes());
            value["events"] = json!(settings.events);
            value["detail"] = json!(settings.detail);
        }
        Err(error) => value["settings_error"] = json!(error),
    }
    if value["enabled"] == true && value["inbox_enabled"] != true {
        value["warning"] =
            json!("notifications are on but the inbox is off; nothing is recorded to notify");
    }
    let socket = manager.native.socket.clone();
    value["coordinator"] = blocking(move || crate::coordinator::status(&socket))
        .await
        .map(|status| status["state"].clone())
        .unwrap_or(Value::Null);
    Ok(value)
}

/// One test message through each route, sent by the coordinator in the
/// environment real notifications use. Not recorded.
async fn test(manager: &Manager) -> Result<Value, String> {
    let socket = manager.native.socket.clone();
    blocking(move || crate::coordinator::notify_test(&socket)).await
}
