//! The read-only core bridge used by the resident coordinator and waits.
//!
//! The core owns the socket and the authoritative object lifetime. This
//! module deliberately keeps the transport thin: it validates one framed
//! request/reply exchange, then exposes a validated push-only watch stream.
//! Callers decide whether a lost stream means polling fallback or an
//! `observation_lost` result.

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout;

pub(crate) const MAX_WATCH_PANES: usize = 64;
const REQUEST_FRAME: usize = 8 * 1024;
const RESPONSE_FRAME: usize = 64 * 1024;
const FRAME_TIMEOUT: Duration = Duration::from_secs(3);

/// A bridge endpoint copied from the native server's format variables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub(crate) path: PathBuf,
    pub(crate) core_boot_id: String,
}

/// Errors are intentionally classified by the point at which they happened.
/// A connection/setup failure can use polling; a failure after a watch ACK
/// means the stream's observation cannot be trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Unavailable(String),
    Rejected { code: String, message: String },
    BootMismatch,
    Lost(String),
    Gap,
    Protocol(String),
}

impl Error {
    /// Setup failures for which a waiter preserves its historical polling
    /// behavior. `observation_capacity_exceeded` is a bridge-side limit too:
    /// no push stream can be established for this pane.
    pub(crate) fn falls_back_to_polling(&self) -> bool {
        match self {
            Self::Unavailable(_) => true,
            Self::Rejected { code, .. } => matches!(
                code.as_str(),
                "connection_limit"
                    | "coordinator_required"
                    | "observation_capacity_exceeded"
                    | "server_exiting"
            ),
            _ => false,
        }
    }

    /// Once hello succeeded, transport failure cannot be confused with a
    /// refused or capacity-limited connection. The caller had a bridge and
    /// lost it while establishing its fenced stream.
    fn after_hello(self) -> Self {
        match self {
            Self::Unavailable(reason) => Self::Lost(reason),
            error => error,
        }
    }

    pub(crate) fn is_target_gone(&self) -> bool {
        matches!(self, Self::Rejected { code, .. } if code == "target_gone")
    }

    pub(crate) fn is_server_exiting(&self) -> bool {
        matches!(self, Self::Rejected { code, .. } if code == "server_exiting")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(formatter, "bridge unavailable: {reason}"),
            Self::Rejected { code, message } => {
                write!(formatter, "bridge rejected {code}: {message}")
            }
            Self::BootMismatch => formatter.write_str("bridge core boot mismatch"),
            Self::Lost(reason) => write!(formatter, "bridge stream lost: {reason}"),
            Self::Gap => formatter.write_str("bridge stream requires resynchronization"),
            Self::Protocol(reason) => write!(formatter, "invalid bridge protocol: {reason}"),
        }
    }
}

struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StrictVisitor;

        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = Strict;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("JSON without duplicate object keys")
            }

            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Strict, E> {
                Ok(Strict(value.into()))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Strict, E> {
                Ok(Strict(value.into()))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Strict, E> {
                Ok(Strict(value.into()))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Strict, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| Strict(Value::Number(number)))
                    .ok_or_else(|| E::custom("non-finite number"))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Strict, E> {
                Ok(Strict(value.into()))
            }

            fn visit_none<E: de::Error>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }

            fn visit_unit<E: de::Error>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Strict, A::Error> {
                let mut values = Vec::new();
                while let Some(Strict(value)) = access.next_element()? {
                    if values.len() == MAX_WATCH_PANES {
                        return Err(de::Error::custom("array limit"));
                    }
                    values.push(value);
                }
                Ok(Strict(Value::Array(values)))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Strict, A::Error> {
                let mut values = Map::new();
                while let Some(key) = access.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom("duplicate key"));
                    }
                    if values.len() == 32 {
                        return Err(de::Error::custom("object limit"));
                    }
                    let Strict(value) = access.next_value()?;
                    values.insert(key, value);
                }
                Ok(Strict(Value::Object(values)))
            }
        }

        deserializer.deserialize_any(StrictVisitor)
    }
}

fn bounded(value: &Value, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    match value {
        Value::Array(values) => values.iter().all(|value| bounded(value, depth + 1)),
        Value::Object(values) => values.values().all(|value| bounded(value, depth + 1)),
        _ => true,
    }
}

fn decode_value(body: &[u8]) -> Result<Value, Error> {
    let Strict(value) = serde_json::from_slice(body)
        .map_err(|error| Error::Protocol(format!("invalid JSON: {error}")))?;
    if !bounded(&value, 1) {
        return Err(Error::Protocol("response nesting limit".into()));
    }
    Ok(value)
}

fn exact_object<'a>(
    value: &'a Value,
    fields: &[&str],
    description: &str,
) -> Result<&'a Map<String, Value>, Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::Protocol(format!("{description} must be an object")))?;
    if object.len() != fields.len() || fields.iter().any(|field| !object.contains_key(*field)) {
        return Err(Error::Protocol(format!("invalid {description} schema")));
    }
    Ok(object)
}

fn decimal(value: Option<&Value>, field: &str) -> Result<u64, Error> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Protocol(format!("{field} must be a decimal string")))?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::Protocol(format!("{field} must be a decimal string")));
    }
    value
        .parse::<u64>()
        .map_err(|_| Error::Protocol(format!("{field} is out of range")))
}

fn identifier(value: Option<&Value>, field: &str) -> Result<String, Error> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Protocol(format!("{field} must be a string")))?;
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        return Err(Error::Protocol(format!("invalid {field}")));
    }
    Ok(value.to_owned())
}

fn object_id(value: &str, sigil: char, field: &str) -> Result<(), Error> {
    let Some(number) = value.strip_prefix(sigil) else {
        return Err(Error::Protocol(format!("invalid {field}")));
    };
    if number.is_empty()
        || number.len() > 10
        || !number.bytes().all(|byte| byte.is_ascii_digit())
        || number.parse::<u32>().is_err()
    {
        return Err(Error::Protocol(format!("invalid {field}")));
    }
    Ok(())
}

fn watch_error(value: &Value, request_id: &str) -> Result<Error, Error> {
    let error = exact_object(
        value,
        &["v", "kind", "request_id", "code", "message"],
        "error",
    )?;
    if error.get("v").and_then(Value::as_u64) != Some(1)
        || error.get("kind").and_then(Value::as_str) != Some("error")
        || error.get("request_id").and_then(Value::as_str) != Some(request_id)
    {
        return Err(Error::Protocol("invalid error envelope".into()));
    }
    Ok(Error::Rejected {
        code: identifier(error.get("code"), "error code")?,
        message: identifier(error.get("message"), "error message")?,
    })
}

async fn write_request(stream: &mut UnixStream, value: &Value) -> Result<(), Error> {
    let body = serde_json::to_vec(value)
        .map_err(|error| Error::Protocol(format!("encoding request: {error}")))?;
    if body.is_empty() || body.len() > REQUEST_FRAME {
        return Err(Error::Protocol("request exceeds core frame limit".into()));
    }
    timeout(FRAME_TIMEOUT, async {
        stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
        stream.write_all(&body).await
    })
    .await
    .map_err(|_| Error::Unavailable("writing request timed out".into()))?
    .map_err(|error| Error::Unavailable(format!("writing request: {error}")))
}

fn frame_length(header: [u8; 4]) -> Result<usize, Error> {
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > RESPONSE_FRAME {
        return Err(Error::Protocol("response exceeds frame limit".into()));
    }
    Ok(length)
}

async fn read_reply(stream: &mut UnixStream) -> Result<Value, Error> {
    let body = timeout(FRAME_TIMEOUT, async {
        let mut header = [0; 4];
        stream
            .read_exact(&mut header)
            .await
            .map_err(|error| Error::Unavailable(format!("reading reply: {error}")))?;
        let length = frame_length(header)?;
        let mut body = vec![0; length];
        stream
            .read_exact(&mut body)
            .await
            .map_err(|error| Error::Unavailable(format!("reading reply: {error}")))?;
        Ok::<_, Error>(body)
    })
    .await
    .map_err(|_| Error::Unavailable("reading reply timed out".into()))??;
    decode_value(&body)
}

async fn read_stream(stream: &mut UnixStream) -> Result<Value, Error> {
    let mut header = [0; 4];
    stream
        .read_exact(&mut header[..1])
        .await
        .map_err(|error| Error::Lost(format!("reading stream: {error}")))?;
    let body = timeout(FRAME_TIMEOUT, async {
        stream
            .read_exact(&mut header[1..])
            .await
            .map_err(|error| Error::Lost(format!("reading stream: {error}")))?;
        let length = frame_length(header)?;
        let mut body = vec![0; length];
        stream
            .read_exact(&mut body)
            .await
            .map_err(|error| Error::Lost(format!("reading stream: {error}")))?;
        Ok::<_, Error>(body)
    })
    .await
    .map_err(|_| Error::Lost("stream frame timed out".into()))??;
    decode_value(&body)
}

fn validate_hello(value: &Value, request_id: &str, expected_boot: &str) -> Result<(), Error> {
    if value.get("kind").and_then(Value::as_str) == Some("error") {
        return Err(watch_error(value, request_id)?);
    }
    let hello = exact_object(
        value,
        &[
            "v",
            "kind",
            "request_id",
            "core_boot_id",
            "negotiated_version",
            "capabilities",
            "limits",
        ],
        "hello",
    )?;
    if hello.get("v").and_then(Value::as_u64) != Some(1)
        || hello.get("kind").and_then(Value::as_str) != Some("hello")
        || hello.get("request_id").and_then(Value::as_str) != Some(request_id)
    {
        return Err(Error::Protocol("invalid hello envelope".into()));
    }
    if identifier(hello.get("core_boot_id"), "core_boot_id")? != expected_boot {
        return Err(Error::BootMismatch);
    }
    let version_value = hello
        .get("negotiated_version")
        .ok_or_else(|| Error::Protocol("missing negotiated version".into()))?;
    let version = exact_object(version_value, &["major", "minor"], "negotiated version")?;
    if version.get("major").and_then(Value::as_u64) != Some(1)
        || version
            .get("minor")
            .and_then(Value::as_u64)
            .is_none_or(|minor| minor < 2)
        || hello
            .get("capabilities")
            .and_then(|value| value.get("watch"))
            .and_then(Value::as_bool)
            != Some(true)
        || hello
            .get("capabilities")
            .and_then(|value| value.get("lifecycle"))
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Err(Error::Protocol(
            "bridge lacks watch lifecycle support".into(),
        ));
    }
    Ok(())
}

/// An established bridge connection before a watch starts.
pub(crate) struct Client {
    stream: UnixStream,
    boot_id: String,
}

impl Client {
    pub(crate) async fn connect(endpoint: &Endpoint, request_id: &str) -> Result<Self, Error> {
        let mut stream = timeout(FRAME_TIMEOUT, UnixStream::connect(&endpoint.path))
            .await
            .map_err(|_| Error::Unavailable("connecting timed out".into()))?
            .map_err(|error| Error::Unavailable(format!("connecting: {error}")))?;
        write_request(
            &mut stream,
            &json!({"v": 1, "kind": "hello", "request_id": request_id}),
        )
        .await?;
        let hello = read_reply(&mut stream).await?;
        validate_hello(&hello, request_id, &endpoint.core_boot_id)?;
        Ok(Self {
            stream,
            boot_id: endpoint.core_boot_id.clone(),
        })
    }

    /// Opens a push-only watch. A lifecycle watch may have an empty explicit
    /// pane scope; every other scope needs one to 64 unique `%N` IDs.
    pub(crate) async fn watch(
        mut self,
        request_id: &str,
        pane_ids: Vec<String>,
        lifecycle: bool,
    ) -> Result<Watch, Error> {
        if pane_ids.len() > MAX_WATCH_PANES || (pane_ids.is_empty() && !lifecycle) {
            return Err(Error::Protocol("invalid watch pane scope".into()));
        }
        let mut scope = HashSet::new();
        for pane_id in &pane_ids {
            object_id(pane_id, '%', "pane_id")?;
            if !scope.insert(pane_id.clone()) {
                return Err(Error::Protocol("duplicate watch pane ID".into()));
            }
        }
        let request = json!({
            "v": 1,
            "kind": "watch",
            "request_id": request_id,
            "expected_core_boot_id": self.boot_id,
            "pane_ids": pane_ids,
            "clients": false,
        });
        // The core's strict schema rejects an omitted/invalid lifecycle on
        // the empty global scope, and rejects a null lifecycle field. Add it
        // only to the lifecycle form.
        let mut request = request;
        if lifecycle {
            request
                .as_object_mut()
                .expect("bridge watch request is an object")
                .insert("lifecycle".into(), Value::String("all".into()));
        }
        write_request(&mut self.stream, &request)
            .await
            .map_err(Error::after_hello)?;
        let ack = read_reply(&mut self.stream)
            .await
            .map_err(Error::after_hello)?;
        let state = parse_watch_ack(&ack, request_id, &self.boot_id, scope, lifecycle)?;
        Ok(Watch {
            stream: self.stream,
            boot_id: self.boot_id,
            state,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WatchState {
    stream_epoch: String,
    last_sequence: u64,
    scope: HashSet<String>,
    lifecycle: bool,
}

fn parse_watch_ack(
    value: &Value,
    request_id: &str,
    boot_id: &str,
    scope: HashSet<String>,
    lifecycle: bool,
) -> Result<WatchState, Error> {
    if value.get("kind").and_then(Value::as_str) == Some("error") {
        return Err(watch_error(value, request_id)?);
    }
    let ack = exact_object(
        value,
        &[
            "v",
            "kind",
            "request_id",
            "core_boot_id",
            "stream_epoch",
            "fence_seq",
            "scope_revision",
            "complete",
            "panes",
        ],
        "watch acknowledgement",
    )?;
    if ack.get("v").and_then(Value::as_u64) != Some(1)
        || ack.get("kind").and_then(Value::as_str) != Some("watch")
        || ack.get("request_id").and_then(Value::as_str) != Some(request_id)
        || ack.get("core_boot_id").and_then(Value::as_str) != Some(boot_id)
        || ack.get("complete").and_then(Value::as_bool) != Some(true)
    {
        return Err(Error::Protocol("invalid watch acknowledgement".into()));
    }
    let stream_epoch = decimal(ack.get("stream_epoch"), "stream_epoch")?.to_string();
    let fence_seq = decimal(ack.get("fence_seq"), "fence_seq")?;
    decimal(ack.get("scope_revision"), "scope_revision")?;
    let panes = ack
        .get("panes")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol("watch acknowledgement panes must be an array".into()))?;
    if panes.len() != scope.len() {
        return Err(Error::Protocol(
            "watch acknowledgement does not contain the scope".into(),
        ));
    }
    let mut baseline = HashSet::new();
    for pane in panes {
        let pane = exact_object(
            pane,
            &[
                "pane_id",
                "pty_generation",
                "screen_generation",
                "width",
                "height",
                "dead",
            ],
            "watch pane",
        )?;
        let pane_id = identifier(pane.get("pane_id"), "pane_id")?;
        object_id(&pane_id, '%', "pane_id")?;
        if !scope.contains(&pane_id) || !baseline.insert(pane_id) {
            return Err(Error::Protocol(
                "watch acknowledgement pane is outside scope".into(),
            ));
        }
        decimal(pane.get("pty_generation"), "pty_generation")?;
        decimal(pane.get("screen_generation"), "screen_generation")?;
        if pane.get("width").and_then(Value::as_u64).is_none()
            || pane.get("height").and_then(Value::as_u64).is_none()
            || pane.get("dead").and_then(Value::as_bool).is_none()
        {
            return Err(Error::Protocol("invalid watch pane metadata".into()));
        }
    }
    Ok(WatchState {
        stream_epoch,
        last_sequence: fence_seq,
        scope,
        lifecycle,
    })
}

/// A reason attached to a pane event. `Created` is lifecycle-only; screen
/// events are delivered only to the explicit pane scopes that own a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneReason {
    ScreenDirty,
    Resized,
    PtyChanged,
    Exited,
    Removed,
    Created,
}

/// A validated bridge event. Generations are checked at the wire boundary;
/// callers need only the identity and reason to schedule an authoritative
/// native pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Pane { pane_id: String, reason: PaneReason },
    WindowRemoved { window_id: String },
    SessionRemoved { session_id: String },
    /// The native server is tearing down sessions. This terminal event is
    /// delivered to every watch, independent of its pane or lifecycle scope.
    ServerExiting,
}

fn parse_pane_reason(value: &str) -> Option<PaneReason> {
    Some(match value {
        "screen_dirty" => PaneReason::ScreenDirty,
        "resized" => PaneReason::Resized,
        "pty_changed" => PaneReason::PtyChanged,
        "exited" => PaneReason::Exited,
        "removed" => PaneReason::Removed,
        "created" => PaneReason::Created,
        _ => return None,
    })
}

fn parse_event(value: &Value, boot_id: &str, state: &WatchState) -> Result<(Event, u64), Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::Protocol("event must be an object".into()))?;
    let reason = object
        .get("reason")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Protocol("event reason must be a string".into()))?;
    let fields: &[&str] = match reason {
        "server_exiting" => &[
            "v",
            "kind",
            "core_boot_id",
            "stream_epoch",
            "event_seq",
            "reason",
        ],
        "window_removed" => &[
            "v",
            "kind",
            "core_boot_id",
            "stream_epoch",
            "event_seq",
            "reason",
            "window_id",
        ],
        "session_removed" => &[
            "v",
            "kind",
            "core_boot_id",
            "stream_epoch",
            "event_seq",
            "reason",
            "session_id",
        ],
        _ => &[
            "v",
            "kind",
            "core_boot_id",
            "stream_epoch",
            "event_seq",
            "reason",
            "pane_id",
            "pty_generation",
            "screen_generation",
        ],
    };
    let event = exact_object(value, fields, "watch event")?;
    if event.get("v").and_then(Value::as_u64) != Some(1)
        || event.get("kind").and_then(Value::as_str) != Some("event")
        || event.get("core_boot_id").and_then(Value::as_str) != Some(boot_id)
        || event.get("stream_epoch").and_then(Value::as_str) != Some(state.stream_epoch.as_str())
    {
        return Err(Error::Protocol("event stream identity mismatch".into()));
    }
    let sequence = decimal(event.get("event_seq"), "event_seq")?;
    if sequence <= state.last_sequence {
        return Err(Error::Protocol("event sequence is not increasing".into()));
    }
    let event = match reason {
        "server_exiting" => Event::ServerExiting,
        "window_removed" => {
            if !state.lifecycle {
                return Err(Error::Protocol(
                    "window lifecycle event outside scope".into(),
                ));
            }
            let window_id = identifier(event.get("window_id"), "window_id")?;
            object_id(&window_id, '@', "window_id")?;
            Event::WindowRemoved { window_id }
        }
        "session_removed" => {
            if !state.lifecycle {
                return Err(Error::Protocol(
                    "session lifecycle event outside scope".into(),
                ));
            }
            let session_id = identifier(event.get("session_id"), "session_id")?;
            object_id(&session_id, '$', "session_id")?;
            Event::SessionRemoved { session_id }
        }
        _ => {
            let reason = parse_pane_reason(reason)
                .ok_or_else(|| Error::Protocol("unknown pane event reason".into()))?;
            let pane_id = identifier(event.get("pane_id"), "pane_id")?;
            object_id(&pane_id, '%', "pane_id")?;
            decimal(event.get("pty_generation"), "pty_generation")?;
            decimal(event.get("screen_generation"), "screen_generation")?;
            let allowed = match reason {
                PaneReason::Created => state.lifecycle,
                PaneReason::ScreenDirty | PaneReason::Resized => state.scope.contains(&pane_id),
                PaneReason::PtyChanged | PaneReason::Exited | PaneReason::Removed => {
                    state.lifecycle || state.scope.contains(&pane_id)
                }
            };
            if !allowed {
                return Err(Error::Protocol("pane event outside watch scope".into()));
            }
            Event::Pane { pane_id, reason }
        }
    };
    Ok((event, sequence))
}

fn validate_gap(value: &Value, boot_id: &str, state: &WatchState) -> Result<(), Error> {
    let gap = exact_object(
        value,
        &[
            "v",
            "kind",
            "core_boot_id",
            "stream_epoch",
            "after_seq",
            "first_available_seq",
            "last_seq",
            "code",
        ],
        "watch gap",
    )?;
    if gap.get("v").and_then(Value::as_u64) != Some(1)
        || gap.get("kind").and_then(Value::as_str) != Some("gap")
        || gap.get("core_boot_id").and_then(Value::as_str) != Some(boot_id)
        || gap.get("stream_epoch").and_then(Value::as_str) != Some(state.stream_epoch.as_str())
        || gap.get("code").and_then(Value::as_str) != Some("resync_required")
    {
        return Err(Error::Protocol("invalid watch gap".into()));
    }
    let after = decimal(gap.get("after_seq"), "after_seq")?;
    let first = decimal(gap.get("first_available_seq"), "first_available_seq")?;
    let last = decimal(gap.get("last_seq"), "last_seq")?;
    if after < state.last_sequence || first <= after || first - after <= 1 || last < first {
        return Err(Error::Protocol("invalid watch gap sequence".into()));
    }
    Ok(())
}

/// A push-only watch stream. A gap or EOF never silently reconnects: its
/// caller must either resync against the native server or report loss.
pub(crate) struct Watch {
    stream: UnixStream,
    boot_id: String,
    state: WatchState,
}

impl Watch {
    pub(crate) async fn next(&mut self) -> Result<Event, Error> {
        let value = read_stream(&mut self.stream).await?;
        match value.get("kind").and_then(Value::as_str) {
            Some("event") => {
                let (event, sequence) = parse_event(&value, &self.boot_id, &self.state)?;
                self.state.last_sequence = sequence;
                Ok(event)
            }
            Some("gap") => {
                validate_gap(&value, &self.boot_id, &self.state)?;
                Err(Error::Gap)
            }
            _ => Err(Error::Protocol("unexpected watch stream frame".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(scope: &[&str], lifecycle: bool) -> WatchState {
        WatchState {
            stream_epoch: "7".into(),
            last_sequence: 3,
            scope: scope.iter().map(|pane| (*pane).to_owned()).collect(),
            lifecycle,
        }
    }

    #[test]
    fn framed_json_rejects_duplicate_keys_and_deep_values() {
        assert!(decode_value(br#"{"v":1,"v":1}"#).is_err());
        let deep = br#"[[[[[[[[[[0]]]]]]]]]]"#;
        assert!(decode_value(deep).is_err());
        assert_eq!(decode_value(br#"{"v":1}"#).unwrap()["v"], 1);
    }

    #[test]
    fn frame_lengths_are_big_endian_and_bounded_before_allocation() {
        assert_eq!(frame_length([0, 0, 1, 0]).unwrap(), 256);
        assert!(frame_length([0, 0, 0, 0]).is_err());
        assert!(frame_length((RESPONSE_FRAME as u32 + 1).to_be_bytes()).is_err());
    }

    #[test]
    fn pane_events_require_the_scope_and_increasing_sequence() {
        let event = json!({
            "v": 1,
            "kind": "event",
            "core_boot_id": "boot",
            "stream_epoch": "7",
            "event_seq": "4",
            "reason": "screen_dirty",
            "pane_id": "%2",
            "pty_generation": "1",
            "screen_generation": "9",
        });
        assert_eq!(
            parse_event(&event, "boot", &state(&["%2"], false))
                .unwrap()
                .0,
            Event::Pane {
                pane_id: "%2".into(),
                reason: PaneReason::ScreenDirty,
            }
        );
        assert!(parse_event(&event, "boot", &state(&["%1"], false)).is_err());
        let mut old = event;
        old["event_seq"] = json!("3");
        assert!(parse_event(&old, "boot", &state(&["%2"], false)).is_err());
    }

    #[test]
    fn lifecycle_events_are_only_valid_on_a_lifecycle_stream() {
        let event = json!({
            "v": 1,
            "kind": "event",
            "core_boot_id": "boot",
            "stream_epoch": "7",
            "event_seq": "4",
            "reason": "window_removed",
            "window_id": "@9",
        });
        assert_eq!(
            parse_event(&event, "boot", &state(&[], true)).unwrap().0,
            Event::WindowRemoved {
                window_id: "@9".into(),
            }
        );
        assert!(parse_event(&event, "boot", &state(&[], false)).is_err());
    }

    #[test]
    fn server_exiting_is_a_global_terminal_event() {
        let event = json!({
            "v": 1,
            "kind": "event",
            "core_boot_id": "boot",
            "stream_epoch": "7",
            "event_seq": "4",
            "reason": "server_exiting",
        });
        assert_eq!(
            parse_event(&event, "boot", &state(&[], false)).unwrap().0,
            Event::ServerExiting
        );
    }

    #[test]
    fn target_gone_and_capacity_are_distinguished_from_loss() {
        let gone = Error::Rejected {
            code: "target_gone".into(),
            message: "gone".into(),
        };
        assert!(gone.is_target_gone());
        assert!(!gone.falls_back_to_polling());
        let capacity = Error::Rejected {
            code: "observation_capacity_exceeded".into(),
            message: "full".into(),
        };
        assert!(capacity.falls_back_to_polling());
        let reserved = Error::Rejected {
            code: "coordinator_required".into(),
            message: "reserved".into(),
        };
        assert!(reserved.falls_back_to_polling());
        let exiting = Error::Rejected {
            code: "server_exiting".into(),
            message: "server is exiting".into(),
        };
        assert!(exiting.is_server_exiting());
        assert!(exiting.falls_back_to_polling());
        assert!(!Error::Lost("EOF".into()).falls_back_to_polling());
        assert_eq!(
            Error::Unavailable("EOF".into()).after_hello(),
            Error::Lost("EOF".into())
        );
    }
}
