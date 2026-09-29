//! Client for conflated, complete agent projections, not event history.
use crate::{
    Output, StreamError, decimal, exchange, object_with_fields, pane_id, receive_stream,
    write_ndjson,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::time::Duration;

type Scope = BTreeMap<String, (String, String, String)>;

fn label(value: &Value) -> Result<String, String> {
    let value = value.as_str().ok_or("identity must be a string")?;
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Err("invalid observation identity".into());
    }
    Ok(value.into())
}

fn choice(value: &Value, values: &[&str]) -> Result<(), String> {
    if !value.as_str().is_some_and(|s| values.contains(&s)) {
        return Err("invalid projection state".into());
    }
    Ok(())
}

pub(crate) fn validate(value: &Value) -> Result<(String, u64, Scope), String> {
    let envelope = object_with_fields(
        value,
        &[
            "v",
            "kind",
            "request_id",
            "epoch",
            "revision",
            "complete",
            "observations",
        ],
        "agent projection",
    )?;
    if envelope.get("v") != Some(&json!(1))
        || value["kind"] != "agents_snapshot"
        || value["request_id"] != "watch-agents"
        || value["complete"] != true
    {
        return Err("invalid agent projection envelope".into());
    }
    let epoch = value["epoch"]
        .as_str()
        .filter(|s| s.len() == 32 && s.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or("invalid daemon epoch")?
        .to_owned();
    let revision = decimal(value.get("revision"), "revision")?;
    let rows = value["observations"]
        .as_array()
        .filter(|rows| !rows.is_empty() && rows.len() <= 64)
        .ok_or("invalid projection observation count")?;
    let mut scope = Scope::new();
    for row in rows {
        object_with_fields(
            row,
            &[
                "id",
                "source_id",
                "session_id",
                "pane_id",
                "native",
                "core",
                "binding",
                "frontend_verified",
                "capabilities",
                "attention",
            ],
            "observation",
        )?;
        let id = label(&row["id"])?;
        let source = label(&row["source_id"])?;
        let session = label(&row["session_id"])?;
        let pane = row["pane_id"].as_str().ok_or("missing pane ID")?;
        if pane != format!("%{}", pane_id(pane)?)
            || scope.insert(id, (source, session, pane.into())).is_some()
        {
            return Err("invalid or duplicate projection target".into());
        }
        choice(&row["binding"], &["explicit_unverified", "invalidated"])?;
        if row["frontend_verified"] != false {
            return Err("unsupported frontend verification".into());
        }
        let native = &row["native"];
        object_with_fields(
            native,
            &[
                "exists",
                "activity",
                "last_activity",
                "attention",
                "permission_count",
                "question_count",
                "freshness",
                "observed_at_ms",
            ],
            "native state",
        )?;
        if !(native["exists"].is_null() || native["exists"].is_boolean())
            || !native["observed_at_ms"].is_u64()
        {
            return Err("invalid native evidence".into());
        }
        choice(
            &native["activity"],
            &["working", "idle", "retrying", "unknown"],
        )?;
        if !native["last_activity"].is_null() {
            choice(
                &native["last_activity"],
                &["working", "idle", "retrying", "unknown"],
            )?;
        }
        choice(
            &native["attention"],
            &["approval", "question", "none", "unknown"],
        )?;
        choice(&native["freshness"], &["fresh", "syncing", "stale"])?;
        let permission_count = native["permission_count"]
            .as_u64()
            .filter(|n| *n <= 256)
            .ok_or("invalid permission count")?;
        let question_count = native["question_count"]
            .as_u64()
            .filter(|n| *n <= 256)
            .ok_or("invalid question count")?;
        if permission_count + question_count > 256 {
            return Err("pending request count exceeds limit".into());
        }
        let core = &row["core"];
        object_with_fields(
            core,
            &["process", "pty_generation", "freshness"],
            "core state",
        )?;
        choice(
            &core["process"],
            &["running", "exited", "removed", "unknown"],
        )?;
        choice(&core["freshness"], &["connecting", "fresh", "stale", "gap"])?;
        if !core["pty_generation"].is_null() {
            decimal(core.get("pty_generation"), "pty_generation")?;
        }
        let attention = &row["attention"];
        object_with_fields(
            attention,
            &["revision", "acknowledged", "pending", "available"],
            "attention state",
        )?;
        decimal(attention.get("revision"), "attention revision")?;
        for field in ["acknowledged", "pending", "available"] {
            if !attention[field].is_boolean() {
                return Err("invalid attention flag".into());
            }
        }
        let capabilities = &row["capabilities"];
        object_with_fields(
            capabilities,
            &[
                "read",
                "input",
                "approval",
                "completion",
                "child_aggregation",
            ],
            "capabilities",
        )?;
        if capabilities["read"] != true
            || ["input", "approval", "completion", "child_aggregation"]
                .iter()
                .any(|name| capabilities[name] != false)
        {
            return Err("unsupported projection capability".into());
        }
    }
    Ok((epoch, revision, scope))
}

pub fn run(socket: &str, args: &[String]) -> Result<i32, String> {
    let count = match args {
        [] => None,
        [flag, value]
            if flag == "--count"
                && !value.is_empty()
                && value.bytes().all(|c| c.is_ascii_digit()) =>
        {
            Some(
                value
                    .parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or("count must be a positive integer")?,
            )
        }
        _ => {
            return Err(
                "watch-agents accepts only --count N (including the initial snapshot)".into(),
            );
        }
    };
    let mut stream = UnixStream::connect(socket).map_err(|_| "manager socket is unavailable")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    let mut value = exchange(
        &mut stream,
        json!({"v":1,"kind":"watch-agents","request_id":"watch-agents"}),
    )?;
    if value["kind"] == "error" {
        println!("{value}");
        return Ok(5);
    }
    let (epoch, mut revision, scope) = validate(&value)?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut received = 0;
    loop {
        if matches!(write_ndjson(&mut output, &value)?, Output::Closed) {
            return Ok(0);
        }
        received += 1;
        if count == Some(received) {
            return Ok(0);
        }
        value = match receive_stream(&mut stream) {
            Ok(value) => value,
            Err(StreamError::Lost(reason)) => {
                eprintln!("masil-agent: agent observation lost: {reason}");
                return Ok(3);
            }
            Err(StreamError::Malformed(reason)) => return Err(reason),
        };
        let (next_epoch, next_revision, next_scope) = validate(&value)?;
        if next_epoch != epoch || next_scope != scope || next_revision <= revision {
            return Err("agent projection identity changed or revision did not advance".into());
        }
        revision = next_revision;
    }
}
