//! Bounded manager IPC. The UI consumes complete snapshots, never terminal bytes.
use crate::{MAX_FRAME, Strict, agent_stream, bounded};
use serde_json::{Value, json};
use std::sync::Arc;
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::{Notify, watch},
};

const DEADLINE: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub(super) enum Update {
    Disconnected(String),
    Snapshot { value: Value, boot: Option<String> },
}

async fn send(stream: &mut UnixStream, request: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(request).map_err(|e| e.to_string())?;
    if body.len() > 8192 {
        return Err("request too large".into());
    }
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream.write_all(&body).await.map_err(|e| e.to_string())
}

async fn read(stream: &mut UnixStream) -> Result<Value, String> {
    // An idle stream has no deadline. Once any frame byte arrives, its entire
    // remaining header/body must arrive inside one deadline.
    let mut header = [0; 4];
    stream
        .read_exact(&mut header[..1])
        .await
        .map_err(|_| "observer disconnected".to_string())?;
    tokio::time::timeout(DEADLINE, async {
        stream
            .read_exact(&mut header[1..])
            .await
            .map_err(|e| e.to_string())?;
        let size = u32::from_be_bytes(header) as usize;
        if size == 0 || size > MAX_FRAME {
            return Err("invalid observer frame size".into());
        }
        let mut bytes = vec![0; size];
        stream
            .read_exact(&mut bytes)
            .await
            .map_err(|e| e.to_string())?;
        let Strict(value) = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if !bounded(&value, 1) {
            return Err("invalid observer frame depth".into());
        }
        Ok(value)
    })
    .await
    .map_err(|_| "observer frame timed out".to_string())?
}

pub(super) async fn query(socket: &Path, request: Value) -> Result<Value, String> {
    tokio::time::timeout(DEADLINE, async {
        let mut stream = UnixStream::connect(socket)
            .await
            .map_err(|_| "observer socket unavailable".to_string())?;
        send(&mut stream, &request).await?;
        let value = read(&mut stream).await?;
        if value["v"] != 1 || value["request_id"] != request["request_id"] {
            return Err("invalid observer response".into());
        }
        if value["kind"] == "error" {
            return Err(format!(
                "{}: {}",
                value["code"].as_str().unwrap_or("error"),
                value["message"].as_str().unwrap_or("request rejected")
            ));
        }
        if value["kind"] != request["kind"] {
            return Err("unexpected observer response".into());
        }
        Ok(value)
    })
    .await
    .map_err(|_| "observer request timed out".to_string())?
}

pub(super) fn status_identity(status: &Value) -> Result<(String, Option<String>), String> {
    let epoch = status["epoch"]
        .as_str()
        .filter(|s| s.len() == 32 && s.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or("invalid observer epoch")?
        .to_owned();
    if status["status"] != "running" {
        return Err("observer is not running".into());
    }
    let boot = if status["core"]["freshness"] == "fresh" {
        status["core"]["core_boot_id"]
            .as_str()
            .filter(|s| {
                s.len() == 36
                    && s.bytes().enumerate().all(|(i, c)| {
                        if [8, 13, 18, 23].contains(&i) {
                            c == b'-'
                        } else {
                            c.is_ascii_hexdigit()
                        }
                    })
            })
            .map(str::to_owned)
    } else {
        None
    };
    Ok((epoch, boot))
}

async fn connected(socket: &Path, tx: &watch::Sender<Update>) -> Result<(), String> {
    let status = query(socket, json!({"v":1,"kind":"status","request_id":"status"})).await?;
    let (epoch, boot) = status_identity(&status)?;
    let mut stream = tokio::time::timeout(DEADLINE, UnixStream::connect(socket))
        .await
        .map_err(|_| "observer connection timed out")?
        .map_err(|e| e.to_string())?;
    tokio::time::timeout(
        DEADLINE,
        send(
            &mut stream,
            &json!({"v":1,"kind":"watch-agents","request_id":"watch-agents"}),
        ),
    )
    .await
    .map_err(|_| "observer subscription timed out")??;
    let mut previous = None;
    loop {
        let value = if previous.is_none() {
            tokio::time::timeout(DEADLINE, read(&mut stream))
                .await
                .map_err(|_| "observer baseline timed out")??
        } else {
            read(&mut stream).await?
        };
        let (frame_epoch, revision, scope) = agent_stream::validate(&value)?;
        if frame_epoch != epoch {
            return Err("observer restarted; reconnecting".into());
        }
        if let Some((old_revision, old_scope)) = &previous
            && (revision <= *old_revision || scope != *old_scope)
        {
            return Err("observer projection identity changed".into());
        }
        if boot.is_none()
            && value["observations"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| row["core"]["freshness"] == "fresh"))
        {
            return Err("Core connected; refreshing its identity".into());
        }
        // Bind one boot to the entire stream. Never combine an old row with a
        // boot learned from a later status after the core has restarted.
        tx.send_replace(Update::Snapshot {
            value,
            boot: boot.clone(),
        });
        previous = Some((revision, scope));
    }
}

pub(super) async fn watch(
    socket: std::path::PathBuf,
    tx: watch::Sender<Update>,
    retry: Arc<Notify>,
) {
    let mut delay = Duration::from_millis(500);
    loop {
        let result = tokio::select! {
            result = connected(&socket, &tx) => result,
            _ = retry.notified() => { delay = Duration::from_millis(500); continue; },
            _ = tx.closed() => return,
        };
        tx.send_replace(Update::Disconnected(
            result
                .err()
                .unwrap_or_else(|| "observer disconnected".into()),
        ));
        tokio::select! {
            _ = tokio::time::sleep(delay) => {},
            _ = retry.notified() => { delay = Duration::from_millis(500); },
            _ = tx.closed() => return,
        }
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn malformed_duplicate_and_oversized_frames_fail_closed() {
        for body in [br#"{"v":1,"v":1}"#.as_slice(), b"[]"] {
            let (mut a, mut b) = UnixStream::pair().unwrap();
            a.write_all(&(body.len() as u32).to_be_bytes())
                .await
                .unwrap();
            a.write_all(body).await.unwrap();
            let value = read(&mut b).await;
            if body[0] == b'{' {
                assert!(value.is_err());
            } else {
                assert!(agent_stream::validate(&value.unwrap()).is_err());
            }
        }
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(&65537u32.to_be_bytes()).await.unwrap();
        assert!(read(&mut b).await.is_err());
    }
    #[test]
    fn boot_is_available_only_with_fresh_evidence() {
        let mut status = json!({"status":"running","epoch":"a".repeat(32),"core":{"freshness":"fresh","core_boot_id":"01234567-89ab-cdef-0123-456789abcdef"}});
        assert!(status_identity(&status).unwrap().1.is_some());
        status["core"]["freshness"] = "stale".into();
        assert!(status_identity(&status).unwrap().1.is_none());
        status["epoch"] = "bad".into();
        assert!(status_identity(&status).is_err());
    }
}
