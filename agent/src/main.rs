//! Observer CLI and opt-in terminal management UI. Queries never start a daemon;
//! `agent coordinator start` and later mutating commands start the coordinator.
mod agent_stream;
mod attention;
mod changes;
mod checkpoint;
mod coordinator;
mod daemon;
mod detection;
mod doctor;
mod finder;
mod ipc;
mod layout;
mod managed;
mod native_ui;
mod notify;
mod observation;
mod opencode;
mod process;
mod providers;
mod schedule;
mod session;
mod ui;
mod worktree;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const MAX_FRAME: usize = 65_536;
const STREAM_FRAME_TIMEOUT: Duration = Duration::from_secs(3);

struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Strict;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON without duplicate object keys")
            }
            fn visit_bool<E: de::Error>(self, x: bool) -> Result<Strict, E> {
                Ok(Strict(x.into()))
            }
            fn visit_i64<E: de::Error>(self, x: i64) -> Result<Strict, E> {
                Ok(Strict(x.into()))
            }
            fn visit_u64<E: de::Error>(self, x: u64) -> Result<Strict, E> {
                Ok(Strict(x.into()))
            }
            fn visit_f64<E: de::Error>(self, x: f64) -> Result<Strict, E> {
                serde_json::Number::from_f64(x)
                    .map(|n| Strict(Value::Number(n)))
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E: de::Error>(self, x: &str) -> Result<Strict, E> {
                Ok(Strict(x.into()))
            }
            fn visit_none<E: de::Error>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Strict, E> {
                Ok(Strict(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
                let mut values = Vec::new();
                while let Some(Strict(value)) = a.next_element()? {
                    if values.len() == 64 {
                        return Err(de::Error::custom("array limit"));
                    }
                    values.push(value);
                }
                Ok(Strict(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Strict, A::Error> {
                let mut values = Map::new();
                while let Some(key) = a.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom("duplicate key"));
                    }
                    if values.len() == 32 {
                        return Err(de::Error::custom("object limit"));
                    }
                    let Strict(value) = a.next_value()?;
                    values.insert(key, value);
                }
                Ok(Strict(Value::Object(values)))
            }
        }
        d.deserialize_any(V)
    }
}

fn bounded(value: &Value, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    match value {
        Value::Array(a) => a.iter().all(|v| bounded(v, depth + 1)),
        Value::Object(m) => m.values().all(|v| bounded(v, depth + 1)),
        _ => true,
    }
}

fn receive(stream: &mut UnixStream, request_id: &str) -> Result<Value, String> {
    let mut header = [0; 4];
    stream.read_exact(&mut header).map_err(|e| e.to_string())?;
    let n = u32::from_be_bytes(header) as usize;
    if n == 0 || n > MAX_FRAME {
        return Err("response exceeds frame limit".into());
    }
    let mut body = vec![0; n];
    stream.read_exact(&mut body).map_err(|e| e.to_string())?;
    let Strict(value) = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    if !bounded(&value, 1) || value["v"] != 1 || value["request_id"] != request_id {
        return Err("invalid response envelope".into());
    }
    Ok(value)
}

fn exchange(stream: &mut UnixStream, value: Value) -> Result<Value, String> {
    let body = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
    if body.len() > 8192 {
        return Err("request exceeds core frame limit".into());
    }
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .map_err(|e| e.to_string())?;
    stream.write_all(&body).map_err(|e| e.to_string())?;
    receive(
        stream,
        value["request_id"].as_str().ok_or("missing request ID")?,
    )
}

enum StreamError {
    Lost(String),
    Malformed(String),
}

enum Output {
    Written,
    Closed,
}

fn stream_read_exact(
    stream: &mut UnixStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<(), StreamError> {
    let mut offset = 0;
    while offset < buffer.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(StreamError::Malformed("stream frame timed out".into()));
        }
        // Darwin rejects subsecond SO_RCVTIMEO values on Unix sockets on some
        // supported releases. Use a whole-second ceiling while retaining the
        // monotonic deadline across all reads in the frame.
        let timeout =
            Duration::from_secs(remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0));
        if let Err(error) = stream.set_read_timeout(Some(timeout)) {
            // Darwin returns EINVAL after the peer has closed even while a
            // complete frame remains buffered. Reads cannot block in that
            // state, so drain the frame and let a later read report EOF.
            if error.kind() != io::ErrorKind::InvalidInput {
                return Err(StreamError::Malformed(format!(
                    "setting stream frame deadline: {error}"
                )));
            }
        }
        let result = stream.read(&mut buffer[offset..]);
        if Instant::now() >= deadline {
            return Err(StreamError::Malformed("stream frame timed out".into()));
        }
        match result {
            Ok(0) => {
                return Err(StreamError::Lost(
                    "unexpected EOF during stream frame".into(),
                ));
            }
            Ok(length) => {
                offset += length;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(StreamError::Malformed("stream frame timed out".into()));
            }
            Err(error) => return Err(StreamError::Lost(format!("stream read failed: {error}"))),
        }
    }
    Ok(())
}

fn receive_stream(stream: &mut UnixStream) -> Result<Value, StreamError> {
    if let Err(error) = stream.set_read_timeout(None)
        && error.kind() != io::ErrorKind::InvalidInput
    {
        return Err(StreamError::Malformed(format!(
            "clearing stream timeout: {error}"
        )));
    }
    let mut header = [0; 4];
    loop {
        match stream.read(&mut header[..1]) {
            Ok(0) => return Err(StreamError::Lost("unexpected EOF on watch stream".into())),
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(StreamError::Lost(format!("stream read failed: {error}"))),
        }
    }
    let deadline = Instant::now() + STREAM_FRAME_TIMEOUT;
    stream_read_exact(stream, &mut header[1..], deadline)?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(StreamError::Malformed(
            "response exceeds frame limit".into(),
        ));
    }
    let mut body = vec![0; length];
    stream_read_exact(stream, &mut body, deadline)?;
    let Strict(value) =
        serde_json::from_slice(&body).map_err(|error| StreamError::Malformed(error.to_string()))?;
    if !bounded(&value, 1) {
        return Err(StreamError::Malformed("response nesting limit".into()));
    }
    Ok(value)
}

fn send_frame(stream: &mut UnixStream, value: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if body.len() > 8192 {
        return Err("request exceeds core frame limit".into());
    }
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .map_err(|error| error.to_string())?;
    stream.write_all(&body).map_err(|error| error.to_string())
}

fn write_ndjson(output: &mut impl Write, value: &Value) -> Result<Output, String> {
    let mut line = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    line.push(b'\n');
    match output.write_all(&line).and_then(|()| output.flush()) {
        Ok(()) => Ok(Output::Written),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(Output::Closed),
        Err(error) => Err(format!("stdout: {error}")),
    }
}

fn object_with_fields<'a>(
    value: &'a Value,
    fields: &[&str],
    description: &str,
) -> Result<&'a Map<String, Value>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{description} must be an object"))?;
    if object.len() != fields.len() || fields.iter().any(|field| !object.contains_key(*field)) {
        return Err(format!("invalid {description} schema"));
    }
    Ok(object)
}

fn decimal(value: Option<&Value>, field: &str) -> Result<u64, String> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field} must be a decimal string"))?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("{field} must be a decimal string"));
    }
    text.parse::<u64>()
        .map_err(|_| format!("{field} is out of range"))
}

fn identifier(value: Option<&Value>, field: &str) -> Result<String, String> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field} must be a string"))?;
    if text.is_empty() || text.len() > 128 {
        return Err(format!("invalid {field}"));
    }
    Ok(text.to_owned())
}

fn pane_id(text: &str) -> Result<u32, String> {
    if text.len() < 2
        || text.len() > 11
        || !text.starts_with('%')
        || !text[1..].bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("invalid pane ID: {text}"));
    }
    text[1..]
        .parse::<u32>()
        .map_err(|_| format!("invalid pane ID: {text}"))
}

fn validate_pane_metadata(value: &Value, scope: &HashSet<u32>) -> Result<u32, String> {
    let pane = object_with_fields(
        value,
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
    let pane_text = pane
        .get("pane_id")
        .and_then(Value::as_str)
        .ok_or("watch pane_id must be a string")?;
    let id = pane_id(pane_text)?;
    if !scope.contains(&id) {
        return Err("watch baseline contains a pane outside its scope".into());
    }
    decimal(pane.get("pty_generation"), "pty_generation")?;
    decimal(pane.get("screen_generation"), "screen_generation")?;
    for field in ["width", "height"] {
        if pane.get(field).and_then(Value::as_u64).is_none() {
            return Err(format!("watch pane {field} must be an unsigned integer"));
        }
    }
    if pane.get("dead").and_then(Value::as_bool).is_none() {
        return Err("watch pane dead must be a boolean".into());
    }
    Ok(id)
}

fn stream_envelope<'a>(
    value: &'a Value,
    fields: &[&str],
    description: &str,
) -> Result<&'a Map<String, Value>, String> {
    let object = object_with_fields(value, fields, description)?;
    if object.get("v").and_then(Value::as_u64) != Some(1) {
        return Err(format!("invalid {description} version"));
    }
    Ok(object)
}

fn stream_value(stream: &mut UnixStream) -> Result<Option<Value>, String> {
    match receive_stream(stream) {
        Ok(value) => Ok(Some(value)),
        Err(StreamError::Malformed(error)) => Err(error),
        Err(StreamError::Lost(error)) => {
            eprintln!("masil-agent: observation lost: {error}");
            Ok(None)
        }
    }
}

fn watch(
    stream: &mut UnixStream,
    hello: &Value,
    pane_arguments: &[String],
    count: Option<u64>,
) -> Result<i32, String> {
    let boot_id = identifier(hello.get("core_boot_id"), "core_boot_id")?;
    let mut scope = HashSet::new();
    for pane in pane_arguments {
        if !scope.insert(pane_id(pane)?) {
            return Err(format!("duplicate pane ID: {pane}"));
        }
    }
    let request = json!({
        "v": 1,
        "kind": "watch",
        "request_id": "watch",
        "expected_core_boot_id": boot_id,
        "pane_ids": pane_arguments,
    });
    send_frame(stream, &request)?;

    let ack = match stream_value(stream) {
        Ok(Some(value)) => value,
        Ok(None) => return Ok(3),
        Err(error) => return Err(error),
    };
    if ack.get("kind").and_then(Value::as_str) == Some("error") {
        if ack.get("v").and_then(Value::as_u64) != Some(1)
            || ack.get("request_id").and_then(Value::as_str) != Some("watch")
        {
            return Err("invalid watch error envelope".into());
        }
        let stdout = io::stdout();
        let mut output = stdout.lock();
        return match write_ndjson(&mut output, &ack)? {
            Output::Written => Ok(5),
            Output::Closed => Ok(0),
        };
    }
    let ack_object = stream_envelope(
        &ack,
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
        "watch ACK",
    )?;
    if ack_object.get("kind").and_then(Value::as_str) != Some("watch")
        || ack_object.get("request_id").and_then(Value::as_str) != Some("watch")
        || ack_object.get("core_boot_id").and_then(Value::as_str) != Some(boot_id.as_str())
        || ack_object.get("complete").and_then(Value::as_bool) != Some(true)
    {
        return Err("invalid watch ACK envelope".into());
    }
    let stream_epoch = decimal(ack_object.get("stream_epoch"), "stream_epoch")?;
    let mut last_sequence = decimal(ack_object.get("fence_seq"), "fence_seq")?;
    decimal(ack_object.get("scope_revision"), "scope_revision")?;
    let panes = ack_object
        .get("panes")
        .and_then(Value::as_array)
        .ok_or("watch ACK panes must be an array")?;
    if panes.len() != scope.len() {
        return Err("watch ACK does not contain the complete scope".into());
    }
    let mut baseline_scope = HashSet::new();
    for pane in panes {
        if !baseline_scope.insert(validate_pane_metadata(pane, &scope)?) {
            return Err("watch ACK contains a duplicate pane".into());
        }
    }

    let stdout = io::stdout();
    let mut output = stdout.lock();
    if matches!(write_ndjson(&mut output, &ack)?, Output::Closed) {
        return Ok(0);
    }
    if count == Some(0) {
        return Ok(0);
    }

    let mut received = 0u64;
    loop {
        let value = match stream_value(stream) {
            Ok(Some(value)) => value,
            Ok(None) => return Ok(3),
            Err(error) => return Err(error),
        };
        match value.get("kind").and_then(Value::as_str) {
            Some("event") => {
                let event = stream_envelope(
                    &value,
                    &[
                        "v",
                        "kind",
                        "core_boot_id",
                        "stream_epoch",
                        "event_seq",
                        "pane_id",
                        "pty_generation",
                        "screen_generation",
                        "reason",
                    ],
                    "event",
                )?;
                if event.get("core_boot_id").and_then(Value::as_str) != Some(boot_id.as_str())
                    || decimal(event.get("stream_epoch"), "stream_epoch")? != stream_epoch
                {
                    return Err("event stream identity mismatch".into());
                }
                let sequence = decimal(event.get("event_seq"), "event_seq")?;
                if sequence <= last_sequence {
                    return Err("event_seq is not strictly increasing".into());
                }
                let event_pane = event
                    .get("pane_id")
                    .and_then(Value::as_str)
                    .ok_or("event pane_id must be a string")?;
                if !scope.contains(&pane_id(event_pane)?) {
                    return Err("event pane is outside the watch scope".into());
                }
                decimal(event.get("pty_generation"), "pty_generation")?;
                decimal(event.get("screen_generation"), "screen_generation")?;
                if !matches!(
                    event.get("reason").and_then(Value::as_str),
                    Some("screen_dirty" | "resized" | "pty_changed" | "exited" | "removed")
                ) {
                    return Err("invalid event reason".into());
                }
                last_sequence = sequence;
                if matches!(write_ndjson(&mut output, &value)?, Output::Closed) {
                    return Ok(0);
                }
                received += 1;
                if count == Some(received) {
                    return Ok(0);
                }
            }
            Some("gap") => {
                let gap = stream_envelope(
                    &value,
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
                    "gap",
                )?;
                if gap.get("core_boot_id").and_then(Value::as_str) != Some(boot_id.as_str())
                    || decimal(gap.get("stream_epoch"), "stream_epoch")? != stream_epoch
                    || gap.get("code").and_then(Value::as_str) != Some("resync_required")
                {
                    return Err("invalid gap stream identity".into());
                }
                let after = decimal(gap.get("after_seq"), "after_seq")?;
                let first = decimal(gap.get("first_available_seq"), "first_available_seq")?;
                let last = decimal(gap.get("last_seq"), "last_seq")?;
                if after < last_sequence || first <= after || first - after <= 1 || last < first {
                    return Err("invalid gap sequence metadata".into());
                }
                if matches!(write_ndjson(&mut output, &value)?, Output::Closed) {
                    return Ok(0);
                }
                eprintln!("masil-agent: observation lost: resync required");
                return Ok(3);
            }
            _ => return Err("unexpected watch stream frame".into()),
        }
    }
}

fn parse_watch_arguments(args: &[String]) -> Result<(Option<u64>, Vec<String>), String> {
    let (count, panes) = if args.first().map(String::as_str) == Some("--count") {
        if args.len() < 3 {
            return Err("watch --count requires a value and pane IDs".into());
        }
        if args[1].is_empty() || !args[1].bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("watch count must be an unsigned integer".into());
        }
        let count = args[1]
            .parse::<u64>()
            .map_err(|_| "watch count must be an unsigned integer".to_string())?;
        (Some(count), &args[2..])
    } else {
        (None, args)
    };
    if panes.is_empty() || panes.len() > 64 {
        return Err("watch requires 1..64 pane IDs".into());
    }
    let mut ids = HashSet::new();
    for pane in panes {
        if !ids.insert(pane_id(pane)?) {
            return Err(format!("duplicate pane ID: {pane}"));
        }
    }
    Ok((count, panes.to_vec()))
}

fn serve_command(socket: &str, args: &[String]) -> Result<i32, String> {
    let mut core = None;
    let mut config_path = None;
    if args.len() != 4 {
        return Err("serve requires --core CORE_SOCKET --config FILE".into());
    }
    for pair in args.as_chunks::<2>().0 {
        match pair[0].as_str() {
            "--core" if core.is_none() => core = Some(pair[1].as_str()),
            "--config" if config_path.is_none() => config_path = Some(pair[1].as_str()),
            _ => return Err("invalid or repeated serve option".into()),
        }
    }
    let core = core.ok_or("missing --core")?;
    let file = std::fs::File::open(config_path.ok_or("missing --config")?)
        .map_err(|_| "cannot open observation config")?;
    let mut body = Vec::new();
    file.take(65_537)
        .read_to_end(&mut body)
        .map_err(|_| "cannot read observation config")?;
    if body.len() > 65_536 {
        return Err("observation config exceeds 64 KiB".into());
    }
    let mut config: observation::Config =
        serde_json::from_slice(&body).map_err(|_| "invalid observation config schema")?;
    observation::validate(&config)?;
    for source in &mut config.sources {
        let path = std::path::Path::new(&source.directory);
        if !path.is_dir() {
            return Err("observation directory does not exist".into());
        }
        source.directory = path
            .canonicalize()
            .map_err(|_| "cannot resolve observation directory")?
            .into_os_string()
            .into_string()
            .map_err(|_| "observation directory is not UTF-8")?;
    }
    observation::validate(&config)?;
    daemon::serve(
        std::path::Path::new(socket),
        std::path::Path::new(core),
        config,
    )
}

fn manager_query(socket: &str, command: &str, args: &[String]) -> Result<i32, String> {
    let mut request = json!({"v":1,"kind":command,"request_id":command});
    match command {
        "inspect" if args.len() == 1 => request["id"] = args[0].clone().into(),
        "attention" if args == ["--all"] => request["all"] = true.into(),
        "ack" if args.len() == 5 => {
            request["id"] = args[0].clone().into();
            for pair in args[1..].as_chunks::<2>().0 {
                let field = match pair[0].as_str() {
                    "--epoch" => "epoch",
                    "--revision" => "revision",
                    _ => return Err("ack requires ID --epoch E --revision R".into()),
                };
                if request.get(field).is_some() {
                    return Err("repeated ack option".into());
                }
                request[field] = pair[1].clone().into();
            }
            if request.get("epoch").is_none() || request.get("revision").is_none() {
                return Err("ack requires ID --epoch E --revision R".into());
            }
        }
        "status" | "agents" | "stop" | "attention" if args.is_empty() => {}
        _ => return Err("incorrect manager query arguments".into()),
    }
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error)
            if command == "status"
                && matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
        {
            let absent = error.kind() == io::ErrorKind::NotFound;
            println!(
                "{}",
                json!({"v":1,"kind":"status","request_id":"status",
                "status":if absent {"not_started"} else {"unreachable"}})
            );
            return Ok(if absent { 0 } else { 3 });
        }
        Err(_) => return Err("manager socket is unavailable".into()),
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    let response = exchange(&mut stream, request)?;
    if response["kind"] != command && response["kind"] != "error" {
        return Err("unexpected manager response kind".into());
    }
    println!(
        "{}",
        serde_json::to_string(&response).map_err(|e| e.to_string())?
    );
    Ok(if response["kind"] == "error" { 5 } else { 0 })
}

fn execute() -> Result<i32, String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args == ["--help"] {
        println!(
            "masil-agent 0.1.0 — native agents and terminal desk\n\nDiagnostics:\n  masil-agent doctor [--socket MASIL_SOCKET] [--versions] [--bundle DIR]\n\nNative management:\n  masil-agent agent --help\n  masil-agent agent [--socket MASIL_SOCKET] list|ui|sidebar\n\nCore socket:\n  masil-agent --socket PATH hello|inventory|snapshot %N|stats\n  masil-agent --socket PATH watch [--count N] %0 [%1 ...]\n\nManager socket:\n  masil-agent --socket PATH serve --core CORE_SOCKET --config FILE\n  masil-agent --socket PATH status|agents|inspect ID|stop\n  masil-agent --socket PATH attention [--all]\n  masil-agent --socket PATH ack ID --epoch E --revision R\n  masil-agent --socket PATH watch-agents [--count N]\n  masil-agent --socket PATH ui [--core-native CORE_SOCKET] [--client CLIENT] [--lang en|ko] [--theme dark|light|terminal]\n  masil-agent --socket PATH sidebar --core-native CORE_SOCKET [--client CLIENT] [--target %N] [--lang en|ko]\n\nmasil server:\n  masil-agent settings [--socket MASIL_SOCKET] [--set KEY VALUE]... [--layer on|off] [--reset] [--get] | --reload-config\n  masil-agent session [--socket MASIL_SOCKET] [--client CLIENT] save|list|preview [NAME]|restore [NAME]|menu|autosave\n  masil-agent layout [--socket MASIL_SOCKET] list|show NAME|plan NAME [--session NAME]|apply NAME [--session NAME] [--yes]\n\nThe sidebar changes the shared window layout. Expand uses native pane zoom. No default tmux bindings are changed.\nAcknowledgements are shared only for this daemon lifetime and never approve provider requests.\n\nExplicit core bridge required: MASIL_BRIDGE_SOCKET=/private/path/observe.sock masil ...\nProvider associations are unverified TUI bindings. Observer connections do not submit prompts or approve requests. Native prompt delivery is not provider acceptance. Queries never start a daemon; `agent coordinator start` and later mutating commands start the coordinator."
        );
        return Ok(0);
    }
    if args == ["--version"] {
        println!("masil-agent 0.1.0");
        return Ok(0);
    }
    if args == ["--ui-design"] {
        println!("{}", ui::DESIGN_CONTRACT);
        return Ok(0);
    }
    if args[0] == "settings" {
        return ui::settings::run(&args[1..]);
    }
    if args[0] == "session" {
        return session::run(&args[1..]);
    }
    if args[0] == "doctor" {
        return doctor::run(&args[1..]);
    }
    if args[0] == "layout" {
        return layout::run(&args[1..]);
    }
    if args[0] == "agent" {
        return match managed::run(&args[1..]) {
            Ok(code) => Ok(code),
            Err(message) => {
                let (class, code) = managed::failure::classify(&message);
                eprintln!("masil-agent: error[{code}]: {message}");
                Ok(class.exit_code())
            }
        };
    }
    if args[0] == "checkpoint-auto" {
        return checkpoint::auto(&args[1..]);
    }
    if args[0] == "worktree" {
        return match worktree::run(&args[1..]) {
            Ok(code) => Ok(code),
            Err(message) => {
                let (class, code) = managed::failure::classify(&message);
                eprintln!("masil-agent: error[{code}]: {message}");
                Ok(class.exit_code())
            }
        };
    }
    if args[0] == "agentd" {
        return coordinator::run(&args[1..]);
    }
    if args.len() < 3 || args[0] != "--socket" {
        return Err("expected --socket PATH command".into());
    }
    let command = args[2].as_str();
    if command == "ui" || command == "sidebar" {
        return ui::run(&args[1], &args[3..], command == "sidebar");
    }
    if command == "serve" {
        return serve_command(&args[1], &args[3..]);
    }
    if command == "watch-agents" {
        return agent_stream::run(&args[1], &args[3..]);
    }
    if matches!(
        command,
        "status" | "agents" | "inspect" | "stop" | "attention" | "ack"
    ) {
        return manager_query(&args[1], command, &args[3..]);
    }
    if !matches!(
        command,
        "hello" | "inventory" | "snapshot" | "stats" | "watch"
    ) {
        return Err("unsupported command; see --help".into());
    }
    let watch_arguments = if command == "watch" {
        Some(parse_watch_arguments(&args[3..])?)
    } else {
        None
    };
    let expected_len = if command == "snapshot" { 4 } else { 3 };
    if command != "watch" && args.len() != expected_len {
        return Err("incorrect arguments; see --help".into());
    }
    let mut stream =
        UnixStream::connect(&args[1]).map_err(|e| format!("bridge unavailable: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    let hello = exchange(
        &mut stream,
        json!({"v":1,"kind":"hello","request_id":"hello"}),
    )?;
    if hello["kind"] == "error" || command == "hello" {
        println!(
            "{}",
            serde_json::to_string_pretty(&hello).map_err(|e| e.to_string())?
        );
        return Ok(if hello["kind"] == "error" { 5 } else { 0 });
    }
    if let Some((count, panes)) = watch_arguments {
        return watch(&mut stream, &hello, &panes, count);
    }
    let mut request = json!({"v":1,"kind":command,"request_id":"query"});
    if command == "snapshot" {
        request["pane_id"] = args[3].clone().into();
    }
    // Stream inventory pages individually, preserving the core's revision fence.
    loop {
        let response = exchange(&mut stream, request.clone())?;
        println!(
            "{}",
            serde_json::to_string(&response).map_err(|e| e.to_string())?
        );
        if response["kind"] == "error" {
            return Ok(5);
        }
        if command != "inventory" || response["next_cursor"].is_null() {
            break;
        }
        request["cursor"] = response["next_cursor"].clone();
        request["revision"] = response["revision"].clone();
    }
    Ok(0)
}

fn main() {
    match execute() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("masil-agent: {error}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn stream_eof_after_deadline_is_timeout() {
        let (mut stream, peer) = UnixStream::pair().unwrap();
        let (start_tx, start_rx) = mpsc::channel();
        let closer = thread::spawn(move || {
            start_rx.recv().unwrap();
            thread::sleep(Duration::from_millis(150));
            drop(peer);
        });

        let started = Instant::now();
        start_tx.send(()).unwrap();
        let mut byte = [0];
        let result = stream_read_exact(&mut stream, &mut byte, started + Duration::from_millis(50));

        closer.join().unwrap();
        assert!(matches!(
            result,
            Err(StreamError::Malformed(ref message)) if message == "stream frame timed out"
        ));
    }

    #[test]
    fn stream_eof_before_deadline_is_lost() {
        let (mut stream, peer) = UnixStream::pair().unwrap();
        drop(peer);
        let mut byte = [0];

        let result = stream_read_exact(
            &mut stream,
            &mut byte,
            Instant::now() + Duration::from_secs(1),
        );

        assert!(matches!(result, Err(StreamError::Lost(_))));
    }

    #[test]
    fn rejects_nested_duplicate_fields() {
        assert!(serde_json::from_str::<Strict>(r#"{"a":{"x":1,"x":2}}"#).is_err());
    }
    #[test]
    fn accepts_unicode_and_enforces_depth() {
        let Strict(value) = serde_json::from_str(r#"{"text":"한글\n中文","null":null}"#).unwrap();
        assert!(bounded(&value, 1));
        let mut deep = Value::Null;
        for _ in 0..9 {
            deep = json!({"nested":deep});
        }
        assert!(!bounded(&deep, 1));
    }
}
