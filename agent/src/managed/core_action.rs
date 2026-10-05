//! The coordinator-only client for the core guarded-action ledger.
//!
//! The resident watch uses ordinary bridge connections. This module owns the
//! single coordinator-role connection, keeps a reader running while no call
//! is pending, and serializes ledger requests so ticket order is explicit.

use super::operations::{Admission, NewOperation, Record, Store, Ticket, UNKNOWN};
use super::{Agent, Manager, nonce};
use crate::bridge::Endpoint;
use crate::observation::now_ms;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixStream, unix::OwnedReadHalf, unix::OwnedWriteHalf};
use tokio::sync::{Notify, mpsc, oneshot};

pub(crate) const ACTION_PROTOCOL: u64 = 1;
const REQUEST_FRAME: usize = 8 * 1024;
const RESPONSE_FRAME: usize = 64 * 1024;
const FRAME_TIMEOUT: Duration = Duration::from_secs(3);
const BUSY_ATTEMPTS: usize = 5;
const INTERRUPT_INTER_KEY_PAUSE: Duration = Duration::from_millis(300);

/// A boot-local ticket assigned by the coordinator connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct DispatchTicket {
    pub(crate) epoch: u64,
    pub(crate) seq: u64,
}

impl DispatchTicket {
    fn value(self) -> Value {
        json!({"epoch": self.epoch.to_string(), "seq": self.seq.to_string()})
    }

    fn parse(value: Option<&Value>) -> Result<Self, Error> {
        let value = value
            .ok_or_else(|| Error::Protocol("dispatch_ticket is missing".into()))?
            .as_object()
            .ok_or_else(|| Error::Protocol("dispatch_ticket must be an object".into()))?;
        if value.len() != 2 || !value.contains_key("epoch") || !value.contains_key("seq") {
            return Err(Error::Protocol("invalid dispatch_ticket schema".into()));
        }
        let epoch = decimal(value.get("epoch"), "dispatch_ticket.epoch")?;
        let seq = decimal(value.get("seq"), "dispatch_ticket.seq")?;
        if epoch == 0 {
            return Err(Error::Protocol("dispatch_ticket epoch is zero".into()));
        }
        Ok(Self { epoch, seq })
    }
}

/// The five operation-key components the C ledger retains for replay checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OperationKey {
    pub(crate) environment_id: String,
    pub(crate) principal: String,
    pub(crate) namespace_epoch: String,
    pub(crate) namespace_nonce: String,
    pub(crate) operation_id: String,
}

impl OperationKey {
    pub(crate) fn value(&self) -> Value {
        json!({
            "environment_id": self.environment_id,
            "principal": self.principal,
            "namespace_epoch": self.namespace_epoch,
            "namespace_nonce": self.namespace_nonce,
            "operation_id": self.operation_id,
        })
    }

    /// C stores a length-prefixed concatenation of these five fields and
    /// exposes only this digest from ledger_list. Keep the encoding here in
    /// lockstep so a later ticket of the same durable interrupt can recover
    /// its full key even if that individual SQLite intent was not committed.
    fn ledger_digest(&self) -> String {
        let fields = [
            &self.environment_id,
            &self.principal,
            &self.namespace_epoch,
            &self.namespace_nonce,
            &self.operation_id,
        ];
        let mut encoded = Vec::new();
        for field in fields {
            encoded.push(field.len() as u8);
            encoded.extend_from_slice(field.as_bytes());
        }
        format!("{:x}", Sha256::digest(encoded))
    }
}

/// Native state the core validates immediately before applying an action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Preconditions {
    pub(crate) core_boot_id: String,
    pub(crate) pane_id: String,
    pub(crate) pty_generation: String,
    pub(crate) foreground_pgid: String,
    pub(crate) current_command: String,
    pub(crate) meta_digest: String,
    pub(crate) tracked_digest: Option<String>,
    pub(crate) expected_output_generation: Option<u64>,
    pub(crate) expected_title: Option<String>,
    pub(crate) expected_progress: Option<String>,
}

impl Preconditions {
    fn target(&self) -> Value {
        json!({
            "core_boot_id": self.core_boot_id,
            "pane_id": self.pane_id,
            "pty_generation": self.pty_generation,
        })
    }

    fn value(&self) -> Value {
        let mut value = json!({
            "expected_foreground_pgid": self.foreground_pgid,
            "expected_current_command": self.current_command,
            "meta_digest": self.meta_digest,
            "pane_dead": false,
            "input_off": false,
            "synchronize": false,
            "mode": "none",
        });
        if let Some(digest) = &self.tracked_digest {
            value["tracked_digest"] = json!(digest);
        }
        if let Some(generation) = self.expected_output_generation {
            value["expected_output_generation"] = json!(generation.to_string());
        }
        if let Some(title) = &self.expected_title {
            value["expected_title"] = json!(title);
        }
        if let Some(progress) = &self.expected_progress {
            value["expected_progress"] = json!(progress);
        }
        value
    }
}

/// One guarded effect sent through the coordinator connection.
#[derive(Clone, Debug)]
pub(crate) struct GuardedAction {
    pub(crate) operation_key: OperationKey,
    pub(crate) ticket: DispatchTicket,
    pub(crate) payload_digest: String,
    pub(crate) retain: bool,
    pub(crate) preconditions: Preconditions,
    pub(crate) action: Value,
}

/// Results from the core ledger. The error variants intentionally retain the
/// C protocol names so callers can report a precise outcome without guessing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LedgerResult {
    Applied {
        ticket: DispatchTicket,
        result_digest: String,
        queued_bytes: Option<u64>,
    },
    RejectedBeforeEffect {
        ticket: DispatchTicket,
        reason: String,
        result_digest: String,
    },
    NotApplied {
        ticket: DispatchTicket,
        result_digest: String,
    },
    LedgerFull,
    IdempotencyConflict,
    ReceiptNotRetained,
    TicketGap,
    EpochClosed,
    EpochUnknown,
}

impl LedgerResult {
    pub(crate) fn ticket(&self) -> Option<DispatchTicket> {
        match self {
            Self::Applied { ticket, .. }
            | Self::RejectedBeforeEffect { ticket, .. }
            | Self::NotApplied { ticket, .. } => Some(*ticket),
            Self::LedgerFull
            | Self::IdempotencyConflict
            | Self::ReceiptNotRetained
            | Self::TicketGap
            | Self::EpochClosed
            | Self::EpochUnknown => None,
        }
    }

    pub(crate) fn result_digest(&self) -> Option<&str> {
        match self {
            Self::Applied { result_digest, .. }
            | Self::RejectedBeforeEffect { result_digest, .. }
            | Self::NotApplied { result_digest, .. } => Some(result_digest),
            Self::LedgerFull
            | Self::IdempotencyConflict
            | Self::ReceiptNotRetained
            | Self::TicketGap
            | Self::EpochClosed
            | Self::EpochUnknown => None,
        }
    }

    pub(crate) fn result_text(&self) -> &'static str {
        match self {
            Self::Applied { .. } => "applied",
            Self::RejectedBeforeEffect { .. } => "rejected_before_effect",
            Self::NotApplied { .. } => "not_applied",
            Self::LedgerFull => "ledger_full",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::ReceiptNotRetained => "receipt_not_retained",
            Self::TicketGap => "ticket_gap",
            Self::EpochClosed => "epoch_closed",
            Self::EpochUnknown => "epoch_unknown",
        }
    }
}

fn consumes_ticket(result: &LedgerResult) -> bool {
    result.ticket().is_some() || matches!(result, LedgerResult::LedgerFull)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReconcileDecision {
    Retire,
    OutcomeUnknown,
}

fn reconcile_decision(has_sqlite_receipt: bool, _result: &LedgerResult) -> ReconcileDecision {
    if has_sqlite_receipt {
        ReconcileDecision::Retire
    } else {
        ReconcileDecision::OutcomeUnknown
    }
}

/// A retained ledger entry returned by `ledger_list`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LedgerEntry {
    pub(crate) ticket: DispatchTicket,
    pub(crate) operation_id: String,
    pub(crate) operation_key_digest: String,
    /// The whole key, so an entry without a SQLite intent can be retired.
    pub(crate) operation_key: OperationKey,
    pub(crate) payload_digest: String,
    pub(crate) result: LedgerResult,
}

/// An epoch that may still own retained ledger slots. The core returns the
/// active epoch as well as every closed epoch whose slots have not all been
/// retired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LedgerEpoch {
    pub(crate) epoch: u64,
    pub(crate) watermark: u64,
    pub(crate) retained: u64,
}

/// The difference between an unavailable bridge before a request and a
/// connection that vanished after an effect may have reached the core.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    /// The request cannot fit the core's receive frame. No bytes were
    /// written, so the live bridge connection remains usable.
    InvalidArgument(String),
    Unavailable(String),
    Lost(String),
    /// The bridge explicitly refused the request before assigning its
    /// sequence. A caller may safely retry the same ticket later.
    Busy(String),
    Rejected {
        code: String,
        message: String,
    },
    Protocol(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidArgument(reason) => formatter.write_str(reason),
            Self::Unavailable(reason) => write!(formatter, "bridge unavailable: {reason}"),
            Self::Lost(reason) => write!(formatter, "bridge connection lost: {reason}"),
            Self::Busy(reason) => write!(formatter, "bridge unavailable: {reason}"),
            Self::Rejected { code, message } => {
                write!(formatter, "bridge rejected {code}: {message}")
            }
            Self::Protocol(reason) => write!(formatter, "invalid action bridge protocol: {reason}"),
        }
    }
}

impl Error {
    fn rejected_code(&self) -> Option<&str> {
        match self {
            Self::Rejected { code, .. } => Some(code),
            Self::InvalidArgument(_)
            | Self::Unavailable(_)
            | Self::Lost(_)
            | Self::Busy(_)
            | Self::Protocol(_) => None,
        }
    }
}

struct WireRequest {
    value: Value,
    request_id: String,
    reply: oneshot::Sender<Result<Value, Error>>,
}

// `call` numbers requests monotonically, so this is the largest request ID
// it can place in a guarded-action frame. Validate against it before a
// durable admission records the ticket, rather than discovering the core's
// 8 KiB receive limit in the writer after a record is dispatching.
const MAX_REQUEST_ID: &str = "core-action-18446744073709551615";

fn guarded_action_fields(request: &GuardedAction) -> Value {
    json!({
        "operation_key": request.operation_key.value(),
        "dispatch_ticket": request.ticket.value(),
        "payload_digest": request.payload_digest,
        "retain": request.retain,
        "target": request.preconditions.target(),
        "preconditions": request.preconditions.value(),
        "action": request.action,
    })
}

/// Check the full bridge request, including its protocol envelope, before a
/// caller records a dispatch ticket. The C bridge's receive limit is 8 KiB.
pub(crate) fn validate_guarded_action(request: &GuardedAction) -> Result<(), String> {
    let mut value = guarded_action_fields(request)
        .as_object()
        .cloned()
        .ok_or("invalid_argument: guarded action is not an object")?;
    value.insert("v".into(), json!(1));
    value.insert("kind".into(), json!("guarded_action"));
    value.insert("request_id".into(), json!(MAX_REQUEST_ID));
    let body = serde_json::to_vec(&Value::Object(value))
        .map_err(|error| format!("invalid_argument: encoding guarded action: {error}"))?;
    if body.is_empty() || body.len() > REQUEST_FRAME {
        return Err("invalid_argument: guarded action exceeds the core 8 KiB frame limit".into());
    }
    Ok(())
}

/// A live coordinator-role connection. The writer waits for one matching
/// reply at a time, while the reader owns the receive half continuously so
/// the bridge cannot close an idle peer for being unable to write to it.
pub(crate) struct Client {
    requests: mpsc::Sender<WireRequest>,
    core_boot_id: String,
    epoch: u64,
    input: bool,
    next_seq: u64,
    request_number: u64,
    broken: bool,
    _reader: tokio::task::JoinHandle<()>,
    _writer: tokio::task::JoinHandle<()>,
}

impl Client {
    pub(crate) async fn connect(endpoint: &Endpoint) -> Result<Self, Error> {
        let mut stream = tokio::time::timeout(FRAME_TIMEOUT, UnixStream::connect(&endpoint.path))
            .await
            .map_err(|_| Error::Unavailable("connecting timed out".into()))?
            .map_err(|error| Error::Unavailable(format!("connecting: {error}")))?;
        let hello_id = "core-action-hello";
        write_value(
            &mut stream,
            &json!({
                "v": 1,
                "kind": "hello",
                "request_id": hello_id,
                "role": "coordinator",
            }),
        )
        .await?;
        let hello = read_value(&mut stream, false).await?;
        let (epoch, input) = parse_hello(&hello, hello_id, endpoint)?;
        let (read, write) = stream.into_split();
        let (requests, received_requests) = mpsc::channel(1);
        let (frames, received_frames) = mpsc::unbounded_channel();
        let reader = tokio::spawn(reader_task(read, frames));
        let writer = tokio::spawn(writer_task(write, received_requests, received_frames));
        Ok(Self {
            requests,
            core_boot_id: endpoint.core_boot_id.clone(),
            epoch,
            input,
            next_seq: 0,
            request_number: 0,
            broken: false,
            _reader: reader,
            _writer: writer,
        })
    }

    pub(crate) const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) const fn supports_input(&self) -> bool {
        self.input
    }

    pub(crate) fn core_boot_id(&self) -> &str {
        &self.core_boot_id
    }

    pub(crate) const fn next_ticket(&self) -> DispatchTicket {
        DispatchTicket {
            epoch: self.epoch,
            seq: self.next_seq,
        }
    }

    pub(crate) fn is_broken(&self) -> bool {
        self.broken
    }

    pub(crate) async fn guarded_action(
        &mut self,
        request: GuardedAction,
    ) -> Result<LedgerResult, Error> {
        validate_guarded_action(&request).map_err(Error::InvalidArgument)?;
        if request.ticket != self.next_ticket() {
            return Err(Error::Protocol(
                "guarded action ticket is out of order".into(),
            ));
        }
        let ticket = request.ticket;
        let fields = guarded_action_fields(&request);
        let reply = self.call_with_busy("guarded_action", fields).await?;
        let result = match parse_ledger_reply(&reply, "guarded_action", ticket) {
            Ok(result) => result,
            Err(error) => {
                self.broken = true;
                return Err(error);
            }
        };
        // `ledger_full` is the one error that C1a explicitly consumes.
        if consumes_ticket(&result) {
            self.next_seq = self
                .next_seq
                .checked_add(1)
                .ok_or_else(|| Error::Protocol("dispatch sequence exhausted".into()))?;
        }
        // These answers prove this connection's local sequence disagrees
        // with the core. Do not assign another effect from its stale ticket
        // stream; the next coordinator connection reconciles first.
        if matches!(
            result,
            LedgerResult::IdempotencyConflict
                | LedgerResult::ReceiptNotRetained
                | LedgerResult::TicketGap
                | LedgerResult::EpochClosed
                | LedgerResult::EpochUnknown
        ) {
            self.broken = true;
        }
        Ok(result)
    }

    /// Stages one prompt body on this coordinator connection.  The core owns
    /// the bytes until the following `input_commit`; no pane input happens in
    /// this phase.
    pub(crate) async fn stage_input(
        &mut self,
        staging_id: &str,
        operation_key: &OperationKey,
        bytes: &[u8],
    ) -> Result<(), Error> {
        if !self.supports_input() {
            return Err(Error::Unavailable(
                "the core does not support staged input".into(),
            ));
        }
        if !valid_staging_id(staging_id) {
            return Err(Error::InvalidArgument(
                "invalid_argument: staging ID is invalid".into(),
            ));
        }
        if bytes.is_empty() || bytes.len() > 32_768 {
            return Err(Error::InvalidArgument(
                "invalid_argument: staged input must contain 1–32768 bytes".into(),
            ));
        }
        let digest = format!("{:x}", Sha256::digest(bytes));
        let reply = self
            .call_with_busy(
                "input_begin",
                json!({
                    "staging_id": staging_id,
                    "operation_key": operation_key.value(),
                    "total_len": bytes.len(),
                    "sha256": digest,
                    "core_boot_id": self.core_boot_id,
                }),
            )
            .await?;
        parse_input_begin(&reply, staging_id)?;
        for (index, chunk) in bytes.chunks(4_608).enumerate() {
            let offset = index * 4_608;
            let reply = self
                .call_with_busy(
                    "input_chunk",
                    json!({
                        "staging_id": staging_id,
                        "offset": offset,
                        "data_b64": base64(chunk),
                    }),
                )
                .await?;
            parse_input_chunk(&reply, staging_id, offset + chunk.len())?;
        }
        Ok(())
    }

    pub(crate) async fn retire_receipt(
        &mut self,
        operation_key: &OperationKey,
        ticket: DispatchTicket,
        result_digest: &str,
        store_revision: u64,
    ) -> Result<(), Error> {
        let reply = self
            .call_with_busy(
                "retire_receipt",
                json!({
                    "core_boot_id": self.core_boot_id,
                    "operation_key": operation_key.value(),
                    "dispatch_ticket": ticket.value(),
                    "result_digest": result_digest,
                    "store_revision": store_revision.to_string(),
                }),
            )
            .await?;
        let request_id = response_id(&reply)
            .ok_or_else(|| Error::Protocol("retire response has no request_id".into()))?;
        if let Some(error) = parse_error(&reply, &request_id)? {
            return Err(error);
        }
        let object = reply
            .as_object()
            .ok_or_else(|| Error::Protocol("retire response is not an object".into()))?;
        if object.get("v").and_then(Value::as_u64) != Some(1)
            || object.get("kind").and_then(Value::as_str) != Some("retire_receipt")
            || DispatchTicket::parse(object.get("dispatch_ticket"))? != ticket
        {
            return Err(Error::Protocol("invalid retire response".into()));
        }
        if object.get("retired").and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        if matches!(
            parse_ledger_reply(&reply, "retire_receipt", ticket)?,
            LedgerResult::NotApplied { .. }
        ) {
            return Ok(());
        }
        Err(Error::Protocol("invalid retire response".into()))
    }

    pub(crate) async fn ledger_query(
        &mut self,
        ticket: DispatchTicket,
    ) -> Result<LedgerResult, Error> {
        let reply = self
            .call_with_busy(
                "ledger_query",
                json!({
                    "core_boot_id": self.core_boot_id,
                    "dispatch_ticket": ticket.value(),
                }),
            )
            .await?;
        parse_ledger_reply(&reply, "ledger_query", ticket)
    }

    pub(crate) async fn ledger_list(&mut self, epoch: u64) -> Result<Vec<LedgerEntry>, Error> {
        let mut after: Option<u64> = None;
        let mut entries = Vec::new();
        loop {
            let mut request = json!({
                "core_boot_id": self.core_boot_id,
                "epoch": epoch.to_string(),
            });
            if let Some(after) = after {
                request["after_seq"] = json!(after.to_string());
            }
            let reply = self.call_with_busy("ledger_list", request).await?;
            let page = parse_ledger_list(&reply, epoch)?;
            let next = page.next_seq;
            entries.extend(page.entries);
            if !page.truncated {
                return Ok(entries);
            }
            if after.is_some_and(|previous| next <= previous) {
                return Err(Error::Protocol(
                    "ledger list pagination did not advance".into(),
                ));
            }
            after = Some(next);
        }
    }

    pub(crate) async fn ledger_epochs(&mut self) -> Result<Vec<LedgerEpoch>, Error> {
        let mut after: Option<u64> = None;
        let mut epochs: Vec<LedgerEpoch> = Vec::new();
        loop {
            let mut request = json!({"core_boot_id": self.core_boot_id});
            if let Some(after) = after {
                request["after_epoch"] = json!(after.to_string());
            }
            let reply = self.call_with_busy("ledger_epochs", request).await?;
            let page = parse_ledger_epochs(&reply)?;
            let next = page.next_epoch;
            if epochs
                .last()
                .zip(page.epochs.first())
                .is_some_and(|(before, after)| after.epoch <= before.epoch)
            {
                return Err(Error::Protocol(
                    "ledger epochs pages are not ordered".into(),
                ));
            }
            epochs.extend(page.epochs);
            if !page.truncated {
                return Ok(epochs);
            }
            if after.is_some_and(|previous| next <= previous) {
                return Err(Error::Protocol(
                    "ledger epochs pagination did not advance".into(),
                ));
            }
            after = Some(next);
        }
    }

    async fn call(&mut self, kind: &str, fields: Value) -> Result<Value, Error> {
        if self.broken {
            return Err(Error::Lost(
                "the coordinator bridge connection is closed".into(),
            ));
        }
        self.request_number = self.request_number.saturating_add(1);
        let request_id = format!("core-action-{}", self.request_number);
        let mut value = fields
            .as_object()
            .cloned()
            .ok_or_else(|| Error::Protocol("action request is not an object".into()))?;
        value.insert("v".into(), json!(1));
        value.insert("kind".into(), json!(kind));
        value.insert("request_id".into(), json!(&request_id));
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(WireRequest {
                value: Value::Object(value),
                request_id,
                reply,
            })
            .await
            .map_err(|_| Error::Lost("the coordinator bridge writer ended".into()))?;
        answer
            .await
            .map_err(|_| Error::Lost("the coordinator bridge writer ended".into()))?
    }

    /// Retry only a bridge response-capacity refusal. The core has not
    /// reserved a ticket for `bridge_busy`, so every ledger method can safely
    /// repeat its exact request on this connection.
    async fn call_with_busy(&mut self, kind: &str, fields: Value) -> Result<Value, Error> {
        let mut busy = BusyRetry::new();
        loop {
            let reply = match self.call(kind, fields.clone()).await {
                Ok(reply) => reply,
                Err(error) => {
                    self.broken = true;
                    return Err(error);
                }
            };
            let request_id = match response_id(&reply) {
                Some(request_id) => request_id,
                None => {
                    self.broken = true;
                    return Err(Error::Protocol("action response has no request_id".into()));
                }
            };
            match parse_error(&reply, &request_id) {
                Ok(Some(Error::Rejected { code, message })) if code == "bridge_busy" => {
                    let Some(delay) = busy.next_delay() else {
                        return Err(Error::Busy(format!(
                            "the core bridge stayed busy for {} attempts: {message}",
                            busy.attempts()
                        )));
                    };
                    tokio::time::sleep(delay).await;
                }
                Ok(_) => return Ok(reply),
                Err(error) => {
                    self.broken = true;
                    return Err(error);
                }
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self._reader.abort();
        self._writer.abort();
    }
}

async fn reader_task(mut read: OwnedReadHalf, frames: mpsc::UnboundedSender<Result<Value, Error>>) {
    loop {
        let frame = read_stream_value(&mut read).await;
        let done = frame.is_err();
        if frames.send(frame).is_err() || done {
            return;
        }
    }
}

async fn writer_task(
    mut write: OwnedWriteHalf,
    mut requests: mpsc::Receiver<WireRequest>,
    mut frames: mpsc::UnboundedReceiver<Result<Value, Error>>,
) {
    while let Some(request) = requests.recv().await {
        let response = match write_value(&mut write, &request.value).await {
            Ok(()) => match tokio::time::timeout(FRAME_TIMEOUT, frames.recv()).await {
                Ok(Some(Ok(value))) => {
                    if response_id(&value).as_deref() == Some(request.request_id.as_str()) {
                        Ok(value)
                    } else {
                        Err(Error::Protocol(
                            "action response request_id mismatch".into(),
                        ))
                    }
                }
                Ok(Some(Err(error))) => Err(error),
                Ok(None) => Err(Error::Lost("the coordinator bridge reader ended".into())),
                Err(_) => Err(Error::Lost("reading action response timed out".into())),
            },
            Err(error) => Err(error),
        };
        let failed = response.is_err();
        let _ = request.reply.send(response);
        if failed {
            while let Ok(request) = requests.try_recv() {
                let _ = request.reply.send(Err(Error::Lost(
                    "the coordinator bridge connection ended".into(),
                )));
            }
            return;
        }
    }
}

async fn write_value<W>(stream: &mut W, value: &Value) -> Result<(), Error>
where
    W: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(value)
        .map_err(|error| Error::Protocol(format!("encoding action request: {error}")))?;
    if body.is_empty() || body.len() > REQUEST_FRAME {
        return Err(Error::Protocol(
            "action request exceeds core frame limit".into(),
        ));
    }
    tokio::time::timeout(FRAME_TIMEOUT, async {
        stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
        stream.write_all(&body).await
    })
    .await
    .map_err(|_| Error::Unavailable("writing action request timed out".into()))?
    .map_err(|error| Error::Unavailable(format!("writing action request: {error}")))
}

async fn read_value<R>(stream: &mut R, lost: bool) -> Result<Value, Error>
where
    R: AsyncRead + Unpin,
{
    let body = tokio::time::timeout(FRAME_TIMEOUT, async {
        let mut header = [0; 4];
        stream.read_exact(&mut header).await?;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > RESPONSE_FRAME {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "action response exceeds frame limit",
            ));
        }
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await?;
        Ok::<_, std::io::Error>(body)
    })
    .await
    .map_err(|_| transport_error(lost, "reading action response timed out"))?
    .map_err(|error| transport_error(lost, &format!("reading action response: {error}")))?;
    serde_json::from_slice(&body)
        .map_err(|error| Error::Protocol(format!("invalid action response JSON: {error}")))
}

/// The dedicated reader intentionally has no idle timeout. A coordinator can
/// sit for minutes between actions, but it still must consume an immediate
/// core reply before the bridge's five-second write deadline expires.
async fn read_stream_value<R>(stream: &mut R) -> Result<Value, Error>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0; 4];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|error| Error::Lost(format!("reading action stream: {error}")))?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > RESPONSE_FRAME {
        return Err(Error::Protocol(
            "action stream response exceeds frame limit".into(),
        ));
    }
    let mut body = vec![0; length];
    stream
        .read_exact(&mut body)
        .await
        .map_err(|error| Error::Lost(format!("reading action stream: {error}")))?;
    serde_json::from_slice(&body)
        .map_err(|error| Error::Protocol(format!("invalid action stream JSON: {error}")))
}

fn transport_error(lost: bool, reason: &str) -> Error {
    if lost {
        Error::Lost(reason.into())
    } else {
        Error::Unavailable(reason.into())
    }
}

fn decimal(value: Option<&Value>, field: &str) -> Result<u64, Error> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Protocol(format!("{field} must be a decimal string")))?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::Protocol(format!("{field} must be a decimal string")));
    }
    text.parse()
        .map_err(|_| Error::Protocol(format!("{field} is out of range")))
}

fn decimal_or_number(value: Option<&Value>, field: &str) -> Result<u64, Error> {
    match value {
        Some(Value::Number(value)) => value
            .as_u64()
            .ok_or_else(|| Error::Protocol(format!("{field} is out of range"))),
        value => decimal(value, field),
    }
}

fn busy_backoff(attempt: usize) -> Duration {
    let multiplier = 1_u64 << attempt.saturating_sub(1).min(3);
    Duration::from_millis(50 * multiplier)
}

/// The same request is retried only when the bridge explicitly says it did
/// not reserve a response slot. Guarded actions therefore never mutate the
/// client's next sequence number while they wait.
struct BusyRetry {
    attempts: usize,
}

impl BusyRetry {
    fn new() -> Self {
        Self { attempts: 0 }
    }

    fn attempts(&self) -> usize {
        self.attempts
    }

    fn next_delay(&mut self) -> Option<Duration> {
        self.attempts += 1;
        (self.attempts < BUSY_ATTEMPTS).then(|| busy_backoff(self.attempts))
    }
}

fn mentions_boot_mismatch(error: &str) -> bool {
    error.contains("boot_mismatch")
}

fn response_id(value: &Value) -> Option<String> {
    value
        .as_object()?
        .get("request_id")?
        .as_str()
        .map(str::to_owned)
}

fn valid_staging_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[((first & 0x03) << 4 | second >> 4) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((second & 0x0f) << 2 | third >> 6) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    encoded
}

fn parse_input_begin(value: &Value, staging_id: &str) -> Result<(), Error> {
    let request_id = response_id(value)
        .ok_or_else(|| Error::Protocol("input_begin response has no request_id".into()))?;
    if let Some(error) = parse_error(value, &request_id)? {
        return Err(error);
    }
    let object = value
        .as_object()
        .ok_or_else(|| Error::Protocol("input_begin response is not an object".into()))?;
    if object.get("v").and_then(Value::as_u64) != Some(1)
        || object.get("kind").and_then(Value::as_str) != Some("input_begin")
        || object.get("staging_id").and_then(Value::as_str) != Some(staging_id)
    {
        return Err(Error::Protocol("invalid input_begin response".into()));
    }
    Ok(())
}

fn parse_input_chunk(value: &Value, staging_id: &str, received: usize) -> Result<(), Error> {
    let request_id = response_id(value)
        .ok_or_else(|| Error::Protocol("input_chunk response has no request_id".into()))?;
    if let Some(error) = parse_error(value, &request_id)? {
        return Err(error);
    }
    let object = value
        .as_object()
        .ok_or_else(|| Error::Protocol("input_chunk response is not an object".into()))?;
    let actual = decimal_or_number(object.get("received"), "input_chunk.received")?;
    if object.get("v").and_then(Value::as_u64) != Some(1)
        || object.get("kind").and_then(Value::as_str) != Some("input_chunk")
        || object.get("staging_id").and_then(Value::as_str) != Some(staging_id)
        || actual != received as u64
    {
        return Err(Error::Protocol("invalid input_chunk response".into()));
    }
    Ok(())
}

fn parse_hello(value: &Value, request_id: &str, endpoint: &Endpoint) -> Result<(u64, bool), Error> {
    if let Some(error) = parse_error(value, request_id)? {
        if matches!(&error, Error::Rejected { code, .. } if code == "epoch_exhausted") {
            return Err(Error::Unavailable(
                "the core has no coordinator action epoch available".into(),
            ));
        }
        return Err(error);
    }
    let object = value
        .as_object()
        .ok_or_else(|| Error::Protocol("hello is not an object".into()))?;
    if object.get("v").and_then(Value::as_u64) != Some(1)
        || object.get("kind").and_then(Value::as_str) != Some("hello")
        || object.get("request_id").and_then(Value::as_str) != Some(request_id)
        || object.get("core_boot_id").and_then(Value::as_str)
            != Some(endpoint.core_boot_id.as_str())
    {
        return Err(Error::Protocol("invalid action hello envelope".into()));
    }
    let capabilities = object
        .get("capabilities")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::Protocol("action hello has no capabilities".into()))?;
    if capabilities.get("actions").and_then(Value::as_bool) != Some(true) {
        return Err(Error::Unavailable(
            "the core does not support actions".into(),
        ));
    }
    Ok((
        decimal(capabilities.get("dispatch_epoch"), "dispatch_epoch")?,
        capabilities.get("input").and_then(Value::as_bool) == Some(true),
    ))
}

fn parse_error(value: &Value, request_id: &str) -> Result<Option<Error>, Error> {
    if value.get("kind").and_then(Value::as_str) != Some("error") {
        return Ok(None);
    }
    let object = value
        .as_object()
        .ok_or_else(|| Error::Protocol("error is not an object".into()))?;
    if object.get("v").and_then(Value::as_u64) != Some(1)
        || object.get("request_id").and_then(Value::as_str) != Some(request_id)
    {
        return Err(Error::Protocol("invalid action error envelope".into()));
    }
    let code = object
        .get("code")
        .and_then(Value::as_str)
        .filter(|code| !code.is_empty())
        .ok_or_else(|| Error::Protocol("action error has no code".into()))?;
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .ok_or_else(|| Error::Protocol("action error has no message".into()))?;
    Ok(Some(Error::Rejected {
        code: code.into(),
        message: message.into(),
    }))
}

fn parse_ledger_reply(
    value: &Value,
    kind: &str,
    ticket: DispatchTicket,
) -> Result<LedgerResult, Error> {
    let request_id = response_id(value)
        .ok_or_else(|| Error::Protocol("action response has no request_id".into()))?;
    if let Some(error) = parse_error(value, &request_id)? {
        return match error {
            Error::Rejected { code, message } => Ok(match code.as_str() {
                "ledger_full" => LedgerResult::LedgerFull,
                "idempotency_conflict" => LedgerResult::IdempotencyConflict,
                "receipt_not_retained" => LedgerResult::ReceiptNotRetained,
                "ticket_gap" => LedgerResult::TicketGap,
                "epoch_closed" => LedgerResult::EpochClosed,
                "epoch_unknown" => LedgerResult::EpochUnknown,
                _ => return Err(Error::Rejected { code, message }),
            }),
            _ => Err(error),
        };
    }
    let object = value
        .as_object()
        .ok_or_else(|| Error::Protocol("ledger response is not an object".into()))?;
    if object.get("v").and_then(Value::as_u64) != Some(1)
        || object.get("kind").and_then(Value::as_str) != Some(kind)
        || DispatchTicket::parse(object.get("dispatch_ticket"))? != ticket
    {
        return Err(Error::Protocol("invalid ledger response envelope".into()));
    }
    let result = object
        .get("result")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Protocol("ledger response has no result".into()))?;
    let digest = object
        .get("result_digest")
        .and_then(Value::as_str)
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| Error::Protocol("ledger response has invalid result_digest".into()))?
        .to_owned();
    if result == "applied" {
        let queued_bytes = match object.get("queued_bytes") {
            None | Some(Value::Null) => None,
            Some(value) => Some(decimal_or_number(Some(value), "queued_bytes")?),
        };
        return Ok(LedgerResult::Applied {
            ticket,
            result_digest: digest,
            queued_bytes,
        });
    }
    if result == "not_applied" {
        return Ok(LedgerResult::NotApplied {
            ticket,
            result_digest: digest,
        });
    }
    if let Some(reason) = result.strip_prefix("rejected_before_effect:") {
        if reason.is_empty() {
            return Err(Error::Protocol("ledger rejection has no reason".into()));
        }
        return Ok(LedgerResult::RejectedBeforeEffect {
            ticket,
            reason: reason.into(),
            result_digest: digest,
        });
    }
    Err(Error::Protocol("unknown ledger result".into()))
}

struct LedgerPage {
    entries: Vec<LedgerEntry>,
    next_seq: u64,
    truncated: bool,
}

struct EpochPage {
    epochs: Vec<LedgerEpoch>,
    next_epoch: u64,
    truncated: bool,
}

fn parse_ledger_epochs(value: &Value) -> Result<EpochPage, Error> {
    let request_id = response_id(value)
        .ok_or_else(|| Error::Protocol("ledger epochs has no request_id".into()))?;
    if let Some(error) = parse_error(value, &request_id)? {
        return Err(error);
    }
    let object = value
        .as_object()
        .ok_or_else(|| Error::Protocol("ledger epochs is not an object".into()))?;
    if object.get("v").and_then(Value::as_u64) != Some(1)
        || object.get("kind").and_then(Value::as_str) != Some("ledger_epochs")
    {
        return Err(Error::Protocol("invalid ledger epochs envelope".into()));
    }
    let epochs = object
        .get("epochs")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol("ledger epochs must be an array".into()))?;
    if epochs.len() > 2049 {
        return Err(Error::Protocol("ledger epochs exceeds capacity".into()));
    }
    let mut previous = None;
    let epochs = epochs
        .iter()
        .map(|epoch| {
            let epoch = epoch
                .as_object()
                .ok_or_else(|| Error::Protocol("ledger epoch is not an object".into()))?;
            if epoch.len() != 3
                || !epoch.contains_key("epoch")
                || !epoch.contains_key("watermark")
                || !epoch.contains_key("retained")
            {
                return Err(Error::Protocol("invalid ledger epoch schema".into()));
            }
            let parsed = LedgerEpoch {
                epoch: decimal(epoch.get("epoch"), "ledger_epochs.epoch")?,
                watermark: decimal(epoch.get("watermark"), "ledger_epochs.watermark")?,
                retained: decimal_or_number(epoch.get("retained"), "ledger_epochs.retained")?,
            };
            if parsed.epoch == 0 || previous.is_some_and(|before| parsed.epoch <= before) {
                return Err(Error::Protocol("ledger epochs are not ordered".into()));
            }
            previous = Some(parsed.epoch);
            Ok(parsed)
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let next_epoch = decimal(object.get("next_epoch"), "ledger_epochs.next_epoch")?;
    let truncated = object
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or_else(|| Error::Protocol("ledger epochs truncated must be a boolean".into()))?;
    if truncated && epochs.last().is_some_and(|epoch| next_epoch != epoch.epoch) {
        return Err(Error::Protocol(
            "ledger epochs next_epoch does not match the page".into(),
        ));
    }
    Ok(EpochPage {
        epochs,
        next_epoch,
        truncated,
    })
}

fn parse_ledger_list(value: &Value, epoch: u64) -> Result<LedgerPage, Error> {
    let request_id = response_id(value)
        .ok_or_else(|| Error::Protocol("ledger list has no request_id".into()))?;
    if let Some(error) = parse_error(value, &request_id)? {
        return Err(error);
    }
    let object = value
        .as_object()
        .ok_or_else(|| Error::Protocol("ledger list is not an object".into()))?;
    if object.get("v").and_then(Value::as_u64) != Some(1)
        || object.get("kind").and_then(Value::as_str) != Some("ledger_list")
        || decimal(object.get("epoch"), "ledger_list.epoch")? != epoch
    {
        return Err(Error::Protocol("invalid ledger list envelope".into()));
    }
    let entries = object
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol("ledger list entries must be an array".into()))?;
    if entries.len() > 2048 {
        return Err(Error::Protocol("ledger list exceeds slot count".into()));
    }
    let entries = entries
        .iter()
        .map(|entry| {
            let object = entry
                .as_object()
                .ok_or_else(|| Error::Protocol("ledger entry is not an object".into()))?;
            let ticket = DispatchTicket::parse(object.get("dispatch_ticket"))?;
            if ticket.epoch != epoch {
                return Err(Error::Protocol("ledger entry has another epoch".into()));
            }
            let operation_id = object
                .get("operation_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| Error::Protocol("ledger entry has no operation_id".into()))?
                .to_owned();
            let operation_key_digest = object
                .get("operation_key_digest")
                .and_then(Value::as_str)
                .filter(|value| {
                    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .ok_or_else(|| {
                    Error::Protocol("ledger entry has invalid operation_key_digest".into())
                })?
                .to_owned();
            let key = object
                .get("operation_key")
                .and_then(Value::as_object)
                .ok_or_else(|| Error::Protocol("ledger entry has no operation_key".into()))?;
            let component = |name: &str| {
                key.get(name)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        Error::Protocol(format!("ledger entry operation_key has no {name}"))
                    })
            };
            let operation_key = OperationKey {
                environment_id: component("environment_id")?,
                principal: component("principal")?,
                namespace_epoch: component("namespace_epoch")?,
                namespace_nonce: component("namespace_nonce")?,
                operation_id: component("operation_id")?,
            };
            let payload_digest = object
                .get("payload_digest")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty() && value.len() <= 64)
                .ok_or_else(|| Error::Protocol("ledger entry has invalid payload_digest".into()))?
                .to_owned();
            let result = parse_ledger_reply(
                &json!({
                    "v": 1,
                    "kind": "ledger_query",
                    "request_id": "ledger-list-entry",
                    "dispatch_ticket": ticket.value(),
                    "result": object.get("result").cloned().unwrap_or(Value::Null),
                    "result_digest": object.get("result_digest").cloned().unwrap_or(Value::Null),
                    "queued_bytes": object.get("queued_bytes").cloned().unwrap_or(Value::Null),
                }),
                "ledger_query",
                ticket,
            )?;
            Ok(LedgerEntry {
                ticket,
                operation_id,
                operation_key_digest,
                operation_key,
                payload_digest,
                result,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let next_seq = decimal(object.get("next_seq"), "ledger_list.next_seq")?;
    let truncated = object
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or_else(|| Error::Protocol("ledger list truncated must be a boolean".into()))?;
    Ok(LedgerPage {
        entries,
        next_seq,
        truncated,
    })
}

/// A per-pane flag shared with the resident pass. It does not make external
/// hook writes impossible; those are still caught by `tracked_digest` and
/// the coordinator's same-run retry.
#[derive(Default)]
pub(crate) struct PaneActionLocks {
    held: Mutex<HashSet<String>>,
    changed: Notify,
}

impl PaneActionLocks {
    pub(crate) async fn lock(self: &Arc<Self>, pane: &str) -> PaneActionGuard {
        loop {
            let notified = self.changed.notified();
            if self
                .held
                .lock()
                .map(|mut held| held.insert(pane.to_owned()))
                .unwrap_or(false)
            {
                return PaneActionGuard {
                    locks: self.clone(),
                    pane: pane.to_owned(),
                };
            }
            notified.await;
        }
    }

    pub(crate) fn is_locked(&self, pane: &str) -> bool {
        self.held
            .lock()
            .map(|held| held.contains(pane))
            .unwrap_or(false)
    }
}

pub(crate) struct PaneActionGuard {
    locks: Arc<PaneActionLocks>,
    pane: String,
}

impl Drop for PaneActionGuard {
    fn drop(&mut self) {
        if let Ok(mut held) = self.locks.held.lock() {
            held.remove(&self.pane);
        }
        self.locks.changed.notify_waiters();
    }
}

#[derive(Default)]
struct ActionStatus {
    bridge: String,
    detail: Option<String>,
    epoch: Option<u64>,
}

/// The coordinator-side owner of the core action connection. Its serial lock
/// is deliberately broader than a pane lock: core tickets are one ordered
/// sequence for the connection, while pane locks only protect resident
/// TRACKED writes during an individual action.
pub(crate) struct CoordinatorActions {
    socket: PathBuf,
    locks: Arc<PaneActionLocks>,
    serial: tokio::sync::Mutex<()>,
    client: tokio::sync::Mutex<Option<Client>>,
    stopping: AtomicBool,
    active: AtomicUsize,
    idle: Notify,
    status: Mutex<ActionStatus>,
}

impl CoordinatorActions {
    pub(crate) fn new(socket: PathBuf, locks: Arc<PaneActionLocks>) -> Self {
        Self {
            socket,
            locks,
            serial: tokio::sync::Mutex::new(()),
            client: tokio::sync::Mutex::new(None),
            stopping: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            idle: Notify::new(),
            status: Mutex::new(ActionStatus {
                bridge: "not_connected".into(),
                detail: None,
                epoch: None,
            }),
        }
    }

    /// Starts the dedicated connection early when this boot selected the core
    /// path. Failure is retained as status, not made fatal to unrelated
    /// coordinator duties; a later action reports `bridge_unavailable`.
    pub(crate) async fn start(&self) {
        let Ok(manager) = Manager::new(self.socket.clone(), None) else {
            return;
        };
        if !matches!(
            manager.action_path().await,
            Ok(super::ActionPath::CoreLedger)
        ) {
            return;
        }
        if let Err(error) = self.connect(&manager).await {
            self.unavailable(&error);
        }
    }

    pub(crate) fn status(&self) -> Value {
        match self.status.lock() {
            Ok(status) => json!({
                "state": status.bridge,
                "detail": status.detail,
                "epoch": status.epoch,
            }),
            Err(_) => json!({"state": "unavailable", "detail": "action status unavailable"}),
        }
    }

    /// Stop accepts no new work, waits for an action that already sent a
    /// request, then drops the bridge connection. Dropping it before the
    /// reply would fence the epoch while its outcome was still unknown.
    pub(crate) async fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        // An action owns this lock through its reply. Taking it after setting
        // `stopping` both fences a waiter that has not started yet and waits
        // for a request already in flight before its Client is dropped.
        let _serial = self.serial.lock().await;
        while self.active.load(Ordering::SeqCst) != 0 {
            self.idle.notified().await;
        }
        self.client.lock().await.take();
    }

    pub(crate) async fn execute(&self, params: &Value) -> Result<Value, String> {
        let _serial = self.serial.lock().await;
        if self.stopping.load(Ordering::SeqCst) {
            return Err("coordinator_unavailable: the coordinator is stopping".into());
        }
        let _active = ActiveAction::new(self);
        let action = ControlAction::parse(params)?;
        let _pane = self.locks.lock(action.pane()).await;
        let manager = Manager::new(self.socket.clone(), None)?;
        let connect_needed = {
            let client = self.client.lock().await;
            client.is_none()
        };
        let connected = if connect_needed {
            self.connect(&manager).await
        } else {
            Ok(())
        };
        if let Err(error) = connected {
            self.unavailable(&error);
            return Err(format!("bridge_unavailable: {error}"));
        }
        let mut client = self.client.lock().await;
        let Some(core) = client.as_mut() else {
            return Err("bridge_unavailable: coordinator action connection is unavailable".into());
        };
        let outcome = match action {
            ControlAction::Keys {
                pane,
                expected,
                keys,
                retry,
            } => {
                self.keys(&manager, core, &pane, &expected, &keys, retry)
                    .await
            }
            ControlAction::Interrupt {
                pane,
                expected,
                operation,
                keys,
            } => {
                self.interrupt(
                    &manager,
                    core,
                    InterruptAction {
                        pane: &pane,
                        expected: &expected,
                        operation: operation.as_deref(),
                        keys: &keys,
                    },
                )
                .await
            }
            ControlAction::Close {
                pane,
                expected,
                operation,
            } => {
                self.close(&manager, core, &pane, &expected, operation.as_deref())
                    .await
            }
            ControlAction::Prompt {
                pane,
                expected,
                text,
                operation,
                queued,
                needs_bracket,
                checkpoint,
            } => {
                self.prompt(
                    &manager,
                    core,
                    &pane,
                    &expected,
                    &text,
                    operation,
                    queued,
                    needs_bracket,
                    checkpoint,
                )
                .await
            }
        };
        let boot_mismatch = outcome
            .as_ref()
            .err()
            .is_some_and(|error| mentions_boot_mismatch(error));
        let old_boot = core.core_boot_id().to_owned();
        if core.is_broken() || boot_mismatch {
            client.take();
            self.unavailable(if boot_mismatch {
                "the native server restarted while a core action was in flight"
            } else {
                "the coordinator bridge connection ended during an action"
            });
        }
        drop(client);
        if boot_mismatch
            && let Err(error) = self.mark_boot_outcomes_unknown(&manager, &old_boot).await
        {
            self.unavailable(&error);
        }
        outcome
    }

    async fn connect(&self, manager: &Manager) -> Result<(), String> {
        match self.connect_once(manager).await {
            Ok(()) => Ok(()),
            Err(_) => {
                // SIGUSR1 recreates a missing native bridge socket. Send it
                // once, wait well below the half-second budget, then make
                // one new connection attempt before exposing
                // bridge_unavailable to the caller.
                self.nudge_native_bridge(manager).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                self.connect_once(manager).await
            }
        }
    }

    async fn nudge_native_bridge(&self, manager: &Manager) {
        let Ok(pid) = manager.command(&["display-message", "-p", "#{pid}"]).await else {
            return;
        };
        let Ok(pid) = pid.trim().parse::<libc::pid_t>() else {
            return;
        };
        if pid <= 1 {
            return;
        }
        // SAFETY: the native server supplied this positive process ID for
        // the socket this action is already addressing.
        unsafe {
            libc::kill(pid, libc::SIGUSR1);
        }
    }

    async fn connect_once(&self, manager: &Manager) -> Result<(), String> {
        let endpoint = manager
            .bridge_endpoint()
            .await?
            .ok_or("the native bridge socket is not available")?;
        let mut core = match Client::connect(&endpoint).await {
            Ok(core) => core,
            Err(error) => {
                if error.rejected_code() == Some("boot_mismatch") {
                    self.mark_boot_outcomes_unknown(manager, &endpoint.core_boot_id)
                        .await?;
                }
                return Err(error.to_string());
            }
        };
        if let Err(error) = self.reconcile(manager, &mut core).await {
            if mentions_boot_mismatch(&error) {
                self.mark_boot_outcomes_unknown(manager, core.core_boot_id())
                    .await?;
            }
            return Err(error);
        }
        if let Ok(mut status) = self.status.lock() {
            status.bridge = "connected".into();
            status.detail = None;
            status.epoch = Some(core.epoch());
        }
        *self.client.lock().await = Some(core);
        Ok(())
    }

    fn unavailable(&self, error: &str) {
        if let Ok(mut status) = self.status.lock() {
            status.bridge = "bridge_unavailable".into();
            status.detail = Some(error.into());
            status.epoch = None;
        }
    }

    /// Once a core boot changes, no ledger query can establish whether a
    /// request sent to that old boot reached its effect. Keep every such
    /// durable intent visible for an operator instead of treating the new
    /// boot's empty ledger as a no-op.
    async fn mark_boot_outcomes_unknown(
        &self,
        manager: &Manager,
        old_boot: &str,
    ) -> Result<(), String> {
        let mut store = manager.operation_store().await?;
        for record in store.list(false, 4096)? {
            if record.boot != old_boot
                || record.state != super::operations::DISPATCHING
                || core_intents(&record).is_empty()
            {
                continue;
            }
            let evidence = json!({"path": "core_ledger", "reconciled": "boot_mismatch"});
            let record = store.finish(
                &ticket_for(&record),
                UNKNOWN,
                "core_ledger_reconcile",
                Some(&evidence),
                now_ms(),
            )?;
            store.note_prompt_result(&record)?;
        }
        Ok(())
    }

    async fn keys(
        &self,
        manager: &Manager,
        core: &mut Client,
        pane: &str,
        expected: &Expected,
        keys: &[String],
        retry: Retry,
    ) -> Result<Value, String> {
        validate_keys(keys)?;
        let current = manager.get_readonly(pane).await?;
        expected.matches(&current, matches!(retry, Retry::Never))?;
        let first = self
            .send_keys(core, &current, keys, KeyDispatch::ephemeral(&current, true))
            .await;
        let result = match first {
            Ok(LedgerResult::RejectedBeforeEffect { reason, .. })
                if reason == "tracked_changed" && matches!(retry, Retry::SameRun) =>
            {
                let fresh = manager.get_readonly(pane).await?;
                expected.matches(&fresh, false)?;
                self.send_keys(core, &fresh, keys, KeyDispatch::ephemeral(&fresh, true))
                    .await
                    .map_err(|error| match error {
                        Error::InvalidArgument(error) => error,
                        error => format!("outcome_unknown: {error}"),
                    })?
            }
            Ok(result) => result,
            Err(Error::InvalidArgument(error)) => return Err(error),
            Err(Error::Busy(error)) => return Err(format!("bridge_unavailable: {error}")),
            Err(error) => return Err(format!("outcome_unknown: {error}")),
        };
        match result {
            LedgerResult::Applied { .. } => Ok(json!({
                "stage": "keys_delivered",
                "pane_id": current.pane_id,
                "run": current.run,
                "provider_accepted": false,
                "path": "core_ledger",
            })),
            LedgerResult::RejectedBeforeEffect { reason, .. } => {
                Err(format!("rejected_before_effect:{reason}"))
            }
            other => Err(other.result_text().into()),
        }
    }

    async fn interrupt(
        &self,
        manager: &Manager,
        core: &mut Client,
        action: InterruptAction<'_>,
    ) -> Result<Value, String> {
        let current = manager.get_readonly(action.pane).await?;
        // The caller owns screen judgment. The coordinator only verifies the
        // run identity before it commits the durable intent. In particular,
        // a resident TRACKED write is not a reason to discard a caller's
        // already-judged interrupt plan.
        action.expected.matches(&current, false)?;
        validate_keys(action.keys)?;
        if action.keys.is_empty() {
            return Err("invalid_action: interrupt keys must not be empty".into());
        }
        self.durable_interrupt(
            manager,
            core,
            current,
            action.operation,
            action.keys,
            action
                .expected
                .tracked_digest
                .as_deref()
                .ok_or("invalid_action: interrupt expected tracked_digest is required")?,
        )
        .await
    }

    async fn close(
        &self,
        manager: &Manager,
        core: &mut Client,
        pane: &str,
        expected: &Expected,
        operation: Option<&str>,
    ) -> Result<Value, String> {
        let current = manager.get_readonly(pane).await?;
        // Like the native close, a TRACKED rewrite within the same run does
        // not stop a close.
        expected.matches(&current, false)?;
        self.durable_kill(manager, core, current, operation).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn prompt(
        &self,
        manager: &Manager,
        core: &mut Client,
        pane: &str,
        expected: &Expected,
        text: &str,
        operation: Option<u64>,
        queued: Option<(i64, i64)>,
        needs_bracket: bool,
        checkpoint: bool,
    ) -> Result<Value, String> {
        if !core.supports_input() {
            return Err("bridge_unavailable: the core does not support staged input".into());
        }
        let current = manager.get_readonly(pane).await?;
        // The caller supplied its freshly read idle screen snapshot. The
        // coordinator confirms identity before durable admission, then gives
        // those exact guards to core for the commit boundary.
        expected.matches(&current, false)?;

        let mut store = manager.operation_store().await?;
        let next = store.prompt_next(&current.run)?;
        let slot = operation.unwrap_or(next);
        if slot == 0 || slot > next {
            return Err(format!("next prompt operation must be {next}"));
        }
        let id = if let Some(operation) = operation {
            operation.to_string()
        } else if let Some((item, _)) = queued {
            format!("q{item}")
        } else {
            format!("auto-{}", nonce()?)
        };
        let namespace = format!("run:{}", current.run);
        let operation_id = digest(&format!("{namespace}/prompt/{id}"));
        let key_nonce = nonce()?;
        let key = durable_key(&self.socket, &current, &key_nonce, &operation_id);
        let text_digest = digest(text);
        let core_ticket = core.next_ticket();
        // A rejected input_begin/input_chunk does not consume the core
        // ticket.  Include the durable-key nonce so an explicit retry using
        // that ticket cannot collide with the still-live staging slot.
        let staging_id = format!(
            "prompt-{}-{}-{}",
            core_ticket.epoch,
            core_ticket.seq,
            &key_nonce[..12]
        );
        let mut input_preconditions = preconditions(
            &current,
            expected
                .tracked_digest
                .as_ref()
                .map(std::string::ToString::to_string),
        );
        input_preconditions.expected_output_generation = expected.output_generation;
        input_preconditions.expected_title = expected.title.clone();
        input_preconditions.expected_progress = expected.progress.clone();
        let action = GuardedAction {
            operation_key: key.clone(),
            ticket: core_ticket,
            payload_digest: digest(&format!(
                "input:{slot}:{staging_id}:{needs_bracket}:enter:{}",
                text_digest
            )),
            retain: true,
            preconditions: input_preconditions,
            action: json!({
                "kind": "input_commit",
                "staging_id": staging_id.clone(),
                "bracketed": needs_bracket,
                "submit": "enter",
            }),
        };
        validate_guarded_action(&action)?;
        let intent = json!({
            "slot": slot,
            "path": "core_ledger",
            "core": {
                "operation_key": key.value(),
                "dispatch_ticket": core_ticket.value(),
            },
            "accept_digest": super::prompt::accept_digest(&current.provider, text),
            "paste_ms": now_ms(),
            "queue_item": queued.map(|(item, _)| item),
        });
        let marked = queued.map(|(item, revision)| super::operations::QueuedPrompt {
            item,
            revision,
            run: &current.run,
        });
        let request = NewOperation {
            namespace: &namespace,
            action: "prompt",
            id: &id,
            explicit: operation.is_some(),
            target: &current.pane_id,
            run: Some(&current.run),
            boot: &current.boot,
            digest: &text_digest,
            payload_bytes: text.len() as u64,
        };
        let (admission, slot) =
            store.admit_prompt(&request, intent, now_ms(), Some(slot), marked.as_ref())?;
        let ticket = match admission {
            Admission::Dispatch(ticket) => {
                // Match the native path: a checkpoint follows durable
                // admission, never a replay, refusal or unresolved receipt.
                if checkpoint {
                    crate::checkpoint::before_prompt(&current.cwd, &current.name, &current.run);
                }
                ticket
            }
            Admission::Recorded(record) => {
                return Ok(super::prompt::core_public_record(
                    &current.run,
                    slot,
                    &record,
                ));
            }
            Admission::NeedsReconcile(record) => {
                return Err(format!(
                    "outcome_unknown: prompt operation {} is unresolved; query its receipt before retrying",
                    record.operation_key
                ));
            }
        };

        match core.stage_input(&staging_id, &key, text.as_bytes()).await {
            Ok(()) => {}
            Err(error) => {
                let (state, message) = prompt_staging_failure(slot, &error);
                if matches!(
                    &error,
                    Error::Unavailable(_) | Error::Lost(_) | Error::Protocol(_)
                ) {
                    core.broken = true;
                }
                let evidence = json!({
                    "path": "core_ledger",
                    "core_ticket": core_ticket.value(),
                    "core_operation_key": key.value(),
                    "staging_id": staging_id,
                    "error": error.to_string(),
                    "result": state,
                });
                let _ = Self::finish_prompt_admitted(
                    &mut store,
                    core,
                    &ticket,
                    state,
                    &evidence,
                    "the prompt staging result could not be committed",
                )?;
                return Err(message);
            }
        }

        match core.guarded_action(action).await {
            Ok(LedgerResult::Applied {
                ticket: core_ticket,
                result_digest,
                queued_bytes: Some(queued_bytes),
            }) => {
                let evidence = input_result_evidence(
                    &key,
                    core_ticket,
                    &result_digest,
                    "applied",
                    Some(queued_bytes),
                );
                let record = Self::finish_prompt_admitted(
                    &mut store,
                    core,
                    &ticket,
                    "delivered",
                    &evidence,
                    "the prompt input_commit applied, but its durable receipt could not be committed",
                )?;
                if core
                    .retire_receipt(&key, core_ticket, &result_digest, record.updated_ms)
                    .await
                    .is_err()
                {
                    core.broken = true;
                }
                if super::observe::observable(&current) {
                    crate::coordinator::poke(&self.socket);
                }
                Ok(super::prompt::core_public_record(
                    &current.run,
                    slot,
                    &record,
                ))
            }
            Ok(LedgerResult::Applied {
                ticket: core_ticket,
                result_digest,
                ..
            }) => {
                let mut evidence =
                    core_result_evidence(&key, core_ticket, &result_digest, "applied");
                evidence["error"] = json!("input_commit omitted queued_bytes");
                Self::finish_prompt_admitted(
                    &mut store,
                    core,
                    &ticket,
                    UNKNOWN,
                    &evidence,
                    "the core applied input_commit without queued-byte evidence",
                )?;
                core.broken = true;
                Err("outcome_unknown: input_commit omitted queued-byte evidence".into())
            }
            Ok(result) => {
                self.finish_prompt_result(&mut store, core, &ticket, &key, slot, result)
                    .await
            }
            Err(error) => {
                let (state, message) = prompt_transport_failure(slot, &error);
                if !matches!(&error, Error::InvalidArgument(_) | Error::Busy(_)) {
                    core.broken = true;
                }
                let evidence = json!({"path": "core_ledger", "error": error.to_string()});
                Self::finish_prompt_admitted(
                    &mut store,
                    core,
                    &ticket,
                    state,
                    &evidence,
                    "the prompt input_commit ended without a durable result",
                )?;
                Err(message)
            }
        }
    }

    async fn send_keys(
        &self,
        core: &mut Client,
        agent: &Agent,
        keys: &[String],
        dispatch: KeyDispatch,
    ) -> Result<LedgerResult, Error> {
        let nonce = nonce().map_err(Error::Unavailable)?;
        let operation_key = dispatch.operation_key.unwrap_or_else(|| {
            ephemeral_key(
                &self.socket,
                agent,
                &nonce,
                &format!("keys:{}", keys.join("\u{1f}")),
            )
        });
        let ticket = core.next_ticket();
        let request = GuardedAction {
            operation_key,
            ticket,
            payload_digest: digest(&json!({"keys": keys}).to_string()),
            retain: dispatch.retain,
            preconditions: preconditions(agent, dispatch.tracked_digest),
            action: json!({"kind": "keys", "keys": keys}),
        };
        validate_guarded_action(&request).map_err(Error::InvalidArgument)?;
        core.guarded_action(request).await
    }

    /// Every path after durable admission must leave a receipt. If the
    /// intended receipt cannot commit, make one last best-effort attempt to
    /// retain an unknown outcome and abandon this bridge connection so a new
    /// coordinator reconciles its retained tickets.
    fn finish_admitted(
        store: &mut Store,
        core: &mut Client,
        ticket: &Ticket,
        state: &str,
        evidence: &Value,
        context: &str,
    ) -> Result<Record, String> {
        match store.finish(ticket, state, "core_ledger", Some(evidence), now_ms()) {
            Ok(record) => Ok(record),
            Err(error) => {
                core.broken = true;
                let fallback = json!({
                    "path": "core_ledger",
                    "receipt_error": error,
                    "evidence": evidence,
                });
                match store.finish(ticket, UNKNOWN, "core_ledger", Some(&fallback), now_ms()) {
                    Ok(_) => Err(format!(
                        "outcome_unknown: {context}; the durable receipt was recorded as outcome_unknown"
                    )),
                    Err(fallback_error) => Err(format!(
                        "outcome_unknown: {context}; neither the intended receipt nor outcome_unknown could be recorded: {fallback_error}"
                    )),
                }
            }
        }
    }

    fn finish_prompt_admitted(
        store: &mut Store,
        core: &mut Client,
        ticket: &Ticket,
        state: &str,
        evidence: &Value,
        context: &str,
    ) -> Result<Record, String> {
        let record = Self::finish_admitted(store, core, ticket, state, evidence, context)?;
        store.note_prompt_result(&record).map_err(|error| {
            core.broken = true;
            format!(
                "outcome_unknown: prompt receipt was committed, but its SQLite number could not be committed: {error}"
            )
        })?;
        Ok(record)
    }

    async fn finish_prompt_result(
        &self,
        store: &mut Store,
        core: &mut Client,
        ticket: &Ticket,
        key: &OperationKey,
        slot: u64,
        result: LedgerResult,
    ) -> Result<Value, String> {
        let (state, message, core_ticket, result_digest, evidence) = match result {
            LedgerResult::RejectedBeforeEffect {
                ticket,
                reason,
                result_digest,
            } => {
                let evidence = input_result_evidence(
                    key,
                    ticket,
                    &result_digest,
                    "rejected_before_effect",
                    None,
                );
                (
                    "rejected_before_effect",
                    prompt_rejection(slot, &reason),
                    Some(ticket),
                    Some(result_digest),
                    evidence,
                )
            }
            LedgerResult::NotApplied {
                ticket,
                result_digest,
            } => {
                let evidence =
                    input_result_evidence(key, ticket, &result_digest, "not_applied", None);
                (
                    "not_applied",
                    "not_applied: the core ledger proved this prompt did not reach pane input"
                        .into(),
                    Some(ticket),
                    Some(result_digest),
                    evidence,
                )
            }
            LedgerResult::LedgerFull => (
                "not_applied",
                "ledger_full: the core ledger is full before prompt input_commit".into(),
                None,
                None,
                json!({"path": "core_ledger", "result": "ledger_full"}),
            ),
            LedgerResult::IdempotencyConflict => (
                UNKNOWN,
                "outcome_unknown: idempotency_conflict: the core ledger ticket belongs to a different request".into(),
                None,
                None,
                json!({"path": "core_ledger", "result": "idempotency_conflict"}),
            ),
            LedgerResult::ReceiptNotRetained => (
                UNKNOWN,
                "outcome_unknown: receipt_not_retained: the core ledger cannot prove this prompt result".into(),
                None,
                None,
                json!({"path": "core_ledger", "result": "receipt_not_retained"}),
            ),
            LedgerResult::TicketGap => (
                UNKNOWN,
                "outcome_unknown: ticket_gap: the core ledger ticket sequence is out of sync".into(),
                None,
                None,
                json!({"path": "core_ledger", "result": "ticket_gap"}),
            ),
            LedgerResult::EpochClosed => (
                UNKNOWN,
                "outcome_unknown: epoch_closed: the core ledger connection epoch closed".into(),
                None,
                None,
                json!({"path": "core_ledger", "result": "epoch_closed"}),
            ),
            LedgerResult::EpochUnknown => (
                UNKNOWN,
                "outcome_unknown: epoch_unknown: the core ledger no longer knows this connection epoch".into(),
                None,
                None,
                json!({"path": "core_ledger", "result": "epoch_unknown"}),
            ),
            LedgerResult::Applied { .. } => return Err("invalid prompt ledger result".into()),
        };
        let record = Self::finish_prompt_admitted(
            store,
            core,
            ticket,
            state,
            &evidence,
            "the core returned a prompt result, but its durable receipt could not be committed",
        )?;
        if let (Some(core_ticket), Some(result_digest)) = (core_ticket, result_digest)
            && core
                .retire_receipt(key, core_ticket, &result_digest, record.updated_ms)
                .await
                .is_err()
        {
            core.broken = true;
        }
        Err(message)
    }

    async fn durable_interrupt(
        &self,
        manager: &Manager,
        core: &mut Client,
        mut agent: Agent,
        operation: Option<&str>,
        keys: &[String],
        tracked_digest: &str,
    ) -> Result<Value, String> {
        let mut store = manager.operation_store().await?;
        let (ticket, key, recorded) = self
            .admit_durable(
                &mut store,
                core,
                &agent,
                DurableActionRequest {
                    action: "interrupt",
                    operation,
                    payload_bytes: keys.len() as u64,
                    preflight_keys: Some((keys, tracked_digest)),
                },
            )
            .await?;
        if let Some(recorded) = recorded {
            return Ok(recorded);
        }
        let ticket = ticket.expect("durable dispatch ticket");
        let key = key.expect("durable operation key");
        let fresh = match manager.get_readonly(&agent.pane_id).await {
            Ok(fresh) => fresh,
            Err(error) => {
                let evidence = json!({"path": "core_ledger", "error": error});
                Self::finish_admitted(
                    &mut store,
                    core,
                    &ticket,
                    UNKNOWN,
                    &evidence,
                    "the interrupt target could not be re-read after admission",
                )?;
                return Err(
                    "outcome_unknown: the interrupt target could not be re-read after admission"
                        .into(),
                );
            }
        };
        if !same_core_run(&agent, &fresh) {
            let evidence = json!({"path": "core_ledger", "reason": "target_changed"});
            Self::finish_admitted(
                &mut store,
                core,
                &ticket,
                "rejected_before_effect",
                &evidence,
                "the interrupt target changed before delivery",
            )?;
            return Err("identity_mismatch: interrupt target changed before delivery".into());
        }
        agent = fresh;
        let initial = agent.clone();
        for (index, sent) in keys.iter().enumerate() {
            if index != 0 {
                // This is deliberately observable: a replacement
                // coordinator can reconcile the recorded first ticket before
                // another key is dispatched.
                tokio::time::sleep(INTERRUPT_INTER_KEY_PAUSE).await;
                let fresh = match manager.get_readonly(&initial.pane_id).await {
                    Ok(fresh) => fresh,
                    Err(error) => {
                        let evidence = json!({"path": "core_ledger", "error": error});
                        Self::finish_admitted(
                            &mut store,
                            core,
                            &ticket,
                            "interrupt_partial",
                            &evidence,
                            "an earlier interrupt key applied, but the target could not be re-read",
                        )?;
                        return Err(
                            "interrupt_partial: an earlier interrupt key reached the pane, but the target could not be re-read"
                                .into(),
                        );
                    }
                };
                if !same_core_run(&initial, &fresh) {
                    let evidence = json!({"path": "core_ledger", "reason": "target_changed"});
                    Self::finish_admitted(
                        &mut store,
                        core,
                        &ticket,
                        "interrupt_partial",
                        &evidence,
                        "an earlier interrupt key applied before the target changed",
                    )?;
                    return Err(
                        "interrupt_partial: the target changed before a later interrupt key".into(),
                    );
                }
                let next = core.next_ticket();
                let intent = core_intent(&key, next);
                if let Err(error) = store.note_dispatch(
                    &ticket,
                    "core_dispatch_ticket",
                    "core_ledger",
                    Some(&intent),
                    now_ms(),
                ) {
                    core.broken = true;
                    let evidence = json!({
                        "path": "core_ledger",
                        "error": error,
                        "reconciled": "next_ticket_not_recorded",
                    });
                    Self::finish_admitted(
                        &mut store,
                        core,
                        &ticket,
                        "interrupt_partial",
                        &evidence,
                        "an earlier interrupt key applied, but the next ticket could not be recorded",
                    )?;
                    return Err(
                        "interrupt_partial: an earlier interrupt key applied, but the next ticket could not be recorded"
                            .into(),
                    );
                }
            }
            let dispatch = self
                .send_keys(
                    core,
                    &initial,
                    std::slice::from_ref(sent),
                    KeyDispatch {
                        retain: true,
                        operation_key: Some(key.clone()),
                        tracked_digest: (index == 0).then(|| tracked_digest.to_owned()),
                    },
                )
                .await;
            let final_key = index + 1 == keys.len();
            match dispatch {
                Ok(LedgerResult::Applied {
                    ticket: core_ticket,
                    result_digest,
                    ..
                }) if final_key => {
                    let evidence =
                        core_result_evidence(&key, core_ticket, &result_digest, "applied");
                    let record = Self::finish_admitted(
                        &mut store,
                        core,
                        &ticket,
                        "interrupt_key_delivered",
                        &evidence,
                        "an interrupt key applied, but its durable receipt could not be committed",
                    )?;
                    if core
                        .retire_receipt(&key, core_ticket, &result_digest, record.updated_ms)
                        .await
                        .is_err()
                    {
                        core.broken = true;
                    }
                    return Ok(receipt(&record));
                }
                Ok(LedgerResult::Applied {
                    ticket: core_ticket,
                    result_digest,
                    ..
                }) => {
                    let evidence =
                        core_result_evidence(&key, core_ticket, &result_digest, "applied");
                    let record = match store.note_dispatch(
                        &ticket,
                        "core_action_result",
                        "core_ledger",
                        Some(&evidence),
                        now_ms(),
                    ) {
                        Ok(record) => record,
                        Err(error) => {
                            core.broken = true;
                            let evidence = json!({"path": "core_ledger", "error": error});
                            Self::finish_admitted(
                                &mut store,
                                core,
                                &ticket,
                                "interrupt_partial",
                                &evidence,
                                "an interrupt key applied, but its durable receipt could not be committed",
                            )?;
                            return Err(
                                "interrupt_partial: an interrupt key applied, but its durable receipt could not be committed"
                                    .into(),
                            );
                        }
                    };
                    if core
                        .retire_receipt(&key, core_ticket, &result_digest, record.updated_ms)
                        .await
                        .is_err()
                    {
                        core.broken = true;
                    }
                }
                Ok(result) => {
                    return self
                        .finish_durable_failure(&mut store, core, &ticket, &key, result, index != 0)
                        .await;
                }
                Err(error) => {
                    if matches!(&error, Error::Busy(_)) {
                        let state = if index == 0 {
                            "not_applied"
                        } else {
                            "interrupt_partial"
                        };
                        let evidence = json!({
                            "error": error.to_string(),
                            "path": "core_ledger",
                            "reconciled": "bridge_busy",
                        });
                        Self::finish_admitted(
                            &mut store,
                            core,
                            &ticket,
                            state,
                            &evidence,
                            "the bridge did not consume this interrupt ticket, but its durable receipt could not be committed",
                        )?;
                        return Err(format!("bridge_unavailable: {error}"));
                    }
                    let evidence = json!({"error": error.to_string(), "path": "core_ledger"});
                    let partial = index != 0;
                    let state = if partial {
                        "interrupt_partial"
                    } else if matches!(&error, Error::InvalidArgument(_)) {
                        "not_applied"
                    } else {
                        UNKNOWN
                    };
                    if !matches!(&error, Error::InvalidArgument(_)) {
                        core.broken = true;
                    }
                    Self::finish_admitted(
                        &mut store,
                        core,
                        &ticket,
                        state,
                        &evidence,
                        "the interrupt action ended without a durable result",
                    )?;
                    return Err(match error {
                        Error::InvalidArgument(error) => error,
                        error if partial => format!(
                            "interrupt_partial: an earlier interrupt key reached the pane; {error}"
                        ),
                        error => format!("outcome_unknown: {error}"),
                    });
                }
            }
        }
        Err("invalid interrupt key sequence".into())
    }

    async fn durable_kill(
        &self,
        manager: &Manager,
        core: &mut Client,
        agent: Agent,
        operation: Option<&str>,
    ) -> Result<Value, String> {
        let mut store = manager.operation_store().await?;
        let (ticket, key, recorded) = self
            .admit_durable(
                &mut store,
                core,
                &agent,
                DurableActionRequest {
                    action: "close",
                    operation,
                    payload_bytes: 0,
                    preflight_keys: None,
                },
            )
            .await?;
        if let Some(recorded) = recorded {
            return Ok(recorded);
        }
        let ticket = ticket.expect("durable dispatch ticket");
        let key = key.expect("durable operation key");
        let agent = match manager.get_readonly(&agent.pane_id).await {
            Ok(fresh) if same_core_run(&agent, &fresh) => fresh,
            Ok(_) => {
                let evidence = json!({"path": "core_ledger", "reason": "target_changed"});
                Self::finish_admitted(
                    &mut store,
                    core,
                    &ticket,
                    "rejected_before_effect",
                    &evidence,
                    "the close target changed before delivery",
                )?;
                return Err("identity_mismatch: close target changed before delivery".into());
            }
            Err(error) => {
                let evidence = json!({"path": "core_ledger", "error": error});
                Self::finish_admitted(
                    &mut store,
                    core,
                    &ticket,
                    UNKNOWN,
                    &evidence,
                    "the close target could not be re-read after admission",
                )?;
                return Err(
                    "outcome_unknown: the close target could not be re-read after admission".into(),
                );
            }
        };
        let core_ticket = core.next_ticket();
        let response = core
            .guarded_action(GuardedAction {
                operation_key: key.clone(),
                ticket: core_ticket,
                payload_digest: digest("kill_pane"),
                retain: true,
                preconditions: preconditions(&agent, None),
                action: json!({"kind": "kill_pane"}),
            })
            .await;
        match response {
            Ok(LedgerResult::Applied {
                ticket: core_ticket,
                result_digest,
                ..
            }) => {
                let evidence = core_result_evidence(&key, core_ticket, &result_digest, "applied");
                let record = Self::finish_admitted(
                    &mut store,
                    core,
                    &ticket,
                    "pane_closed",
                    &evidence,
                    "the pane closed, but its durable receipt could not be committed",
                )?;
                if core
                    .retire_receipt(&key, core_ticket, &result_digest, record.updated_ms)
                    .await
                    .is_err()
                {
                    core.broken = true;
                }
                Ok(receipt(&record))
            }
            Ok(result) => {
                self.finish_durable_failure(&mut store, core, &ticket, &key, result, false)
                    .await
            }
            Err(error) => {
                if matches!(&error, Error::Busy(_)) {
                    let evidence = json!({
                        "error": error.to_string(),
                        "path": "core_ledger",
                        "reconciled": "bridge_busy",
                    });
                    Self::finish_admitted(
                        &mut store,
                        core,
                        &ticket,
                        "not_applied",
                        &evidence,
                        "the bridge did not consume this close ticket, but its durable receipt could not be committed",
                    )?;
                    return Err(format!("bridge_unavailable: {error}"));
                }
                let evidence = json!({"error": error.to_string(), "path": "core_ledger"});
                if !matches!(&error, Error::InvalidArgument(_)) {
                    core.broken = true;
                }
                Self::finish_admitted(
                    &mut store,
                    core,
                    &ticket,
                    if matches!(&error, Error::InvalidArgument(_)) {
                        "not_applied"
                    } else {
                        UNKNOWN
                    },
                    &evidence,
                    "the close action ended without a durable result",
                )?;
                Err(match error {
                    Error::InvalidArgument(error) => error,
                    error => format!("outcome_unknown: {error}"),
                })
            }
        }
    }

    async fn admit_durable(
        &self,
        store: &mut Store,
        core: &Client,
        agent: &Agent,
        request: DurableActionRequest<'_>,
    ) -> Result<(Option<Ticket>, Option<OperationKey>, Option<Value>), String> {
        let id = match request.operation {
            Some(id) if valid_operation_id(id) => id.to_owned(),
            Some(_) => {
                return Err(
                    "invalid_argument: operation ID must be 1–64 letters, digits, '.', '_', ':' or '-'"
                        .into(),
                );
            }
            None => format!("auto-{}", nonce()?),
        };
        let namespace = format!("run:{}", agent.run);
        let operation_id = digest(&format!("{namespace}/{}/{id}", request.action));
        let key = durable_key(&self.socket, agent, &nonce()?, &operation_id);
        if let Some((keys, tracked_digest)) = request.preflight_keys {
            for (index, key_text) in keys.iter().enumerate() {
                let request = GuardedAction {
                    operation_key: key.clone(),
                    ticket: core.next_ticket(),
                    payload_digest: digest(&json!({"keys": [key_text]}).to_string()),
                    retain: true,
                    preconditions: preconditions(
                        agent,
                        (index == 0).then(|| tracked_digest.to_owned()),
                    ),
                    action: json!({"kind": "keys", "keys": [key_text]}),
                };
                validate_guarded_action(&request)?;
            }
        }
        let intent = json!({
            "path": "core_ledger",
            "core": {
                "operation_key": key.value(),
                "dispatch_ticket": core.next_ticket().value(),
            },
        });
        let new = NewOperation {
            namespace: &namespace,
            action: request.action,
            id: &id,
            explicit: request.operation.is_some(),
            target: &agent.pane_id,
            run: Some(&agent.run),
            boot: &agent.boot,
            digest: &digest(request.action),
            payload_bytes: request.payload_bytes,
        };
        match store.admit(&new, intent, now_ms())? {
            Admission::Dispatch(ticket) => Ok((Some(ticket), Some(key), None)),
            Admission::Recorded(record) => Ok((None, None, Some(receipt(&record)))),
            Admission::NeedsReconcile(record) => Err(format!(
                "outcome_unknown: operation {} is unresolved; query its receipt before retrying",
                record.operation_key
            )),
        }
    }

    async fn finish_durable_failure(
        &self,
        store: &mut Store,
        core: &mut Client,
        ticket: &Ticket,
        key: &OperationKey,
        result: LedgerResult,
        partial: bool,
    ) -> Result<Value, String> {
        let (base_state, base_message, core_ticket, result_digest) = match result {
            LedgerResult::RejectedBeforeEffect {
                ticket,
                reason,
                result_digest,
            } => (
                "rejected_before_effect",
                format!("rejected_before_effect:{reason}"),
                Some(ticket),
                Some(result_digest),
            ),
            LedgerResult::NotApplied {
                ticket,
                result_digest,
            } => (
                "not_applied",
                "not_applied: the core ledger proved this action did not apply".into(),
                Some(ticket),
                Some(result_digest),
            ),
            LedgerResult::LedgerFull => (
                "not_applied",
                "ledger_full: the core ledger is full before the action".into(),
                None,
                None,
            ),
            LedgerResult::IdempotencyConflict => {
                (
                    UNKNOWN,
                    "outcome_unknown: idempotency_conflict: the core ledger ticket belongs to a different request".into(),
                    None,
                    None,
                )
            }
            LedgerResult::ReceiptNotRetained => {
                (
                    UNKNOWN,
                    "outcome_unknown: receipt_not_retained: the core ledger cannot prove this action result".into(),
                    None,
                    None,
                )
            }
            LedgerResult::TicketGap => (
                UNKNOWN,
                "outcome_unknown: ticket_gap: the core ledger ticket sequence is out of sync".into(),
                None,
                None,
            ),
            LedgerResult::EpochClosed => (
                UNKNOWN,
                "outcome_unknown: epoch_closed: the core ledger connection epoch closed".into(),
                None,
                None,
            ),
            LedgerResult::EpochUnknown => (
                UNKNOWN,
                "outcome_unknown: epoch_unknown: the core ledger no longer knows this connection epoch".into(),
                None,
                None,
            ),
            LedgerResult::Applied { .. } => return Err("invalid durable result".into()),
        };
        let state = if partial {
            "interrupt_partial"
        } else {
            base_state
        };
        let message = if partial {
            format!("interrupt_partial: an earlier interrupt key reached the pane; {base_message}")
        } else {
            base_message
        };
        let evidence = json!({"path": "core_ledger", "result": message});
        let record = Self::finish_admitted(
            store,
            core,
            ticket,
            state,
            &evidence,
            "the core returned a durable interrupt result, but its receipt could not be committed",
        )?;
        if let (Some(core_ticket), Some(result_digest)) = (core_ticket, result_digest)
            && core
                .retire_receipt(key, core_ticket, &result_digest, record.updated_ms)
                .await
                .is_err()
        {
            // The durable receipt is safe, but leave this connection so the
            // next one reconciles the retained slot instead of silently
            // stranding it.
            core.broken = true;
        }
        Err(message)
    }

    async fn reconcile(&self, manager: &Manager, core: &mut Client) -> Result<(), String> {
        // A new coordinator epoch does not make the preceding epoch's slots
        // disappear. Ask the core for every epoch that still owns a slot,
        // then consume every page for each one before deciding which SQLite
        // intent can be retried.
        let epochs = core
            .ledger_epochs()
            .await
            .map_err(|error| error.to_string())?;
        let mut listed = Vec::new();
        for epoch in epochs {
            let entries = core
                .ledger_list(epoch.epoch)
                .await
                .map_err(|error| error.to_string())?;
            if entries.len() as u64 != epoch.retained {
                return Err(format!(
                    "invalid action bridge protocol: epoch {} reports {} retained slots but listed {}",
                    epoch.epoch,
                    epoch.retained,
                    entries.len()
                ));
            }
            listed.extend(entries);
        }
        let recorded = manager
            .command(&["show-options", "-gqv", super::STORE_OPTION])
            .await?
            .trim()
            .to_owned();
        let Some(mut store) = Store::open_existing(&manager.native.socket, &recorded)? else {
            if listed.is_empty() {
                return Ok(());
            }
            let mut store = manager.operation_store().await?;
            let boot = core.core_boot_id().to_owned();
            for entry in &listed {
                retire_orphan(&mut store, core, &boot, entry).await?;
            }
            return Ok(());
        };
        let records = store.list(false, 4096)?;
        let boot = core.core_boot_id().to_owned();
        let mut retained = HashSet::new();

        for entry in &listed {
            let Some((original, intent)) = stored_intent_for_entry(&records, entry) else {
                retire_orphan(&mut store, core, &boot, entry).await?;
                continue;
            };
            let current = store
                .get(&original.operation_key)?
                .ok_or("operation disappeared during core ledger reconciliation")?;
            let has_receipt = (current.state != super::operations::DISPATCHING
                && current.state != UNKNOWN)
                || has_ticket_receipt(&current, entry.ticket, &entry.operation_key);
            let decision = reconcile_decision(has_receipt, &entry.result);
            let evidence = ledger_result_evidence(entry);
            let current = if current.state == UNKNOWN {
                // Retaining the ledger result is part of reconciliation, not
                // just cleanup. Once the slot is retired this receipt is the
                // operator's only evidence of what the core proved.
                let settled =
                    prompt_core_state(&current, &entry.result).or_else(|| match &entry.result {
                        LedgerResult::RejectedBeforeEffect { .. } if has_core_effect(&current) => {
                            Some("interrupt_partial")
                        }
                        LedgerResult::RejectedBeforeEffect { .. } => Some("rejected_before_effect"),
                        LedgerResult::NotApplied { .. } if has_core_effect(&current) => {
                            Some("interrupt_partial")
                        }
                        _ => None,
                    });
                match settled {
                    Some(state) => store.settle_unknown(
                        &current,
                        state,
                        "core_ledger_reconcile",
                        Some(&evidence),
                        now_ms(),
                    )?,
                    None => store.note_unknown(
                        &current,
                        "core_ledger_result",
                        "core_ledger_reconcile",
                        Some(&evidence),
                        now_ms(),
                    )?,
                }
            } else if decision == ReconcileDecision::OutcomeUnknown
                && current.state == super::operations::DISPATCHING
            {
                let state = prompt_core_state(&current, &entry.result).unwrap_or_else(|| {
                    match &entry.result {
                        LedgerResult::RejectedBeforeEffect { .. } if has_core_effect(&current) => {
                            "interrupt_partial"
                        }
                        LedgerResult::RejectedBeforeEffect { .. } => "rejected_before_effect",
                        _ => UNKNOWN,
                    }
                });
                store.finish(
                    &ticket_for(&current),
                    state,
                    "core_ledger_reconcile",
                    Some(&evidence),
                    now_ms(),
                )?
            } else {
                current
            };
            store.note_prompt_result(&current)?;
            let result_digest = entry
                .result
                .result_digest()
                .ok_or("retained ledger entry has no result digest")?;
            core.retire_receipt(
                &intent.operation_key,
                entry.ticket,
                result_digest,
                current.updated_ms,
            )
            .await
            .map_err(|error| error.to_string())?;
            retained.insert((entry.ticket, entry.operation_key.ledger_digest()));
        }
        // A boot change removes the old core's ledger entirely. Its unresolved
        // intents cannot be queried against this boot, so make that fact
        // durable rather than silently skipping them.
        for original in &records {
            if original.boot == boot || original.state != super::operations::DISPATCHING {
                continue;
            }
            let current = store
                .get(&original.operation_key)?
                .ok_or("operation disappeared during core boot reconciliation")?;
            if core_intents(&current).is_empty() {
                continue;
            }
            let evidence = json!({"path": "core_ledger", "reconciled": "boot_mismatch"});
            let current = store.finish(
                &ticket_for(&current),
                UNKNOWN,
                "core_ledger_reconcile",
                Some(&evidence),
                now_ms(),
            )?;
            store.note_prompt_result(&current)?;
        }

        // Any current-boot intent that did not appear in ledger_list belongs
        // either to a closed, already-retired epoch or to a ticket that never
        // reached the core. Query every one, including old epochs. Query
        // errors are deliberately fatal to this reconciliation pass: leaving
        // the record unresolved is safer than treating an unread answer as a
        // no-op, and the next connection retries the work.
        for original in &records {
            if original.boot != boot {
                continue;
            }
            let current = store
                .get(&original.operation_key)?
                .ok_or("operation disappeared during core ledger reconciliation")?;
            if !matches!(
                current.state.as_str(),
                super::operations::DISPATCHING | UNKNOWN
            ) {
                continue;
            }
            let intents = core_intents(&current);
            let missing: Vec<_> = intents
                .into_iter()
                .filter(|intent| {
                    !retained.contains(&(intent.ticket, intent.operation_key.ledger_digest()))
                        && !has_ticket_receipt(&current, intent.ticket, &intent.operation_key)
                })
                .collect();
            if missing.is_empty() {
                // A result recorded before a coordinator died is enough to
                // settle a dispatching operation even after it retired the
                // corresponding core slot. Multi-key interrupts need their
                // special partial-result accounting; input commits and close
                // actions have one effect and must not be compared with their
                // payload byte count.
                if let Some(state) = recorded_prompt_core_state(&current) {
                    let evidence = json!({
                        "path": "core_ledger",
                        "reconciled": "recorded_input_result",
                    });
                    let current = if current.state == super::operations::DISPATCHING {
                        store.finish(
                            &ticket_for(&current),
                            state,
                            "core_ledger_reconcile",
                            Some(&evidence),
                            now_ms(),
                        )?
                    } else if current.state == UNKNOWN {
                        store.settle_unknown(
                            &current,
                            state,
                            "core_ledger_reconcile",
                            Some(&evidence),
                            now_ms(),
                        )?
                    } else {
                        current
                    };
                    store.note_prompt_result(&current)?;
                    continue;
                }
                if current.state == super::operations::DISPATCHING {
                    let recovered = match current.action.as_str() {
                        "close" if has_core_effect(&current) => {
                            Some(("pane_closed", "recorded_kill_pane"))
                        }
                        "interrupt" if has_core_effect(&current) => {
                            if applied_core_effects(&current) < current.payload_bytes as usize {
                                Some(("interrupt_partial", "missing_later_ticket"))
                            } else {
                                Some(("interrupt_key_delivered", "all_keys_applied"))
                            }
                        }
                        _ => None,
                    };
                    if let Some((state, reconciled)) = recovered {
                        let evidence = json!({
                            "path": "core_ledger",
                            "reconciled": reconciled,
                        });
                        let current = store.finish(
                            &ticket_for(&current),
                            state,
                            "core_ledger_reconcile",
                            Some(&evidence),
                            now_ms(),
                        )?;
                        store.note_prompt_result(&current)?;
                    }
                }
                continue;
            }

            let mut all_not_applied = true;
            let mut unresolved = None;
            for intent in missing {
                match core.ledger_query(intent.ticket).await {
                    Ok(LedgerResult::NotApplied { .. }) => {}
                    Ok(LedgerResult::ReceiptNotRetained | LedgerResult::EpochUnknown) => {
                        all_not_applied = false;
                        unresolved.get_or_insert_with(
                            || json!({"path": "core_ledger", "reconciled": "receipt_not_retained"}),
                        );
                    }
                    Ok(result) => {
                        all_not_applied = false;
                        unresolved.get_or_insert_with(
                            || json!({"path": "core_ledger", "reconciled": result.result_text()}),
                        );
                    }
                    Err(error) => {
                        return Err(format!(
                            "core ledger query for {}:{} failed: {error}",
                            intent.ticket.epoch, intent.ticket.seq
                        ));
                    }
                }
            }
            let current = store
                .get(&original.operation_key)?
                .ok_or("operation disappeared during core ledger reconciliation")?;
            let state = if all_not_applied {
                if has_core_effect(&current) {
                    "interrupt_partial"
                } else {
                    "not_applied"
                }
            } else {
                UNKNOWN
            };
            let evidence = unresolved
                .unwrap_or_else(|| json!({"path": "core_ledger", "reconciled": "not_applied"}));
            if current.state == super::operations::DISPATCHING {
                let current = store.finish(
                    &ticket_for(&current),
                    state,
                    "core_ledger_reconcile",
                    Some(&evidence),
                    now_ms(),
                )?;
                store.note_prompt_result(&current)?;
            } else if state != UNKNOWN {
                let current = store.settle_unknown(
                    &current,
                    state,
                    "core_ledger_reconcile",
                    Some(&evidence),
                    now_ms(),
                )?;
                store.note_prompt_result(&current)?;
            }
        }
        if core.is_broken() {
            return Err("the coordinator bridge connection ended during reconciliation".into());
        }
        Ok(())
    }
}

struct ActiveAction<'a> {
    owner: &'a CoordinatorActions,
}

impl<'a> ActiveAction<'a> {
    fn new(owner: &'a CoordinatorActions) -> Self {
        owner.active.fetch_add(1, Ordering::SeqCst);
        Self { owner }
    }
}

impl Drop for ActiveAction<'_> {
    fn drop(&mut self) {
        if self.owner.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.owner.idle.notify_waiters();
        }
    }
}

#[derive(Clone, Copy)]
enum Retry {
    Never,
    SameRun,
}

struct KeyDispatch {
    retain: bool,
    operation_key: Option<OperationKey>,
    tracked_digest: Option<String>,
}

impl KeyDispatch {
    fn ephemeral(agent: &Agent, tracked: bool) -> Self {
        Self {
            retain: false,
            operation_key: None,
            tracked_digest: tracked.then(|| digest(&agent.tracked_encoded)),
        }
    }
}

struct Expected {
    pane: String,
    boot: String,
    generation: String,
    run: String,
    meta_digest: String,
    tracked_digest: Option<String>,
    foreground_pgid: String,
    process_epoch: Option<String>,
    output_generation: Option<u64>,
    title: Option<String>,
    progress: Option<String>,
}

impl Expected {
    fn parse(value: &Value, tracked_required: bool) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or("invalid_action: expected must be an object")?;
        let required = [
            "pane_id",
            "boot",
            "generation",
            "run",
            "meta_digest",
            "foreground_pgid",
            "process_epoch",
        ];
        let optional = [
            "tracked_digest",
            "expected_output_generation",
            "expected_title",
            "expected_progress",
        ];
        if !(required.len()..=required.len() + optional.len()).contains(&object.len())
            || required.iter().any(|field| !object.contains_key(*field))
            || object.keys().any(|field| {
                !required.contains(&field.as_str()) && !optional.contains(&field.as_str())
            })
            || (tracked_required && !object.contains_key("tracked_digest"))
        {
            return Err("invalid_action: invalid expected schema".into());
        }
        let text = |field: &str| {
            object
                .get(field)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty() && value.len() <= 256)
                .map(str::to_owned)
                .ok_or_else(|| format!("invalid_action: expected {field} is invalid"))
        };
        let digest = |field: &str| {
            let value = text(field)?;
            (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .then_some(value)
                .ok_or_else(|| format!("invalid_action: expected {field} is invalid"))
        };
        let process_epoch = match object.get("process_epoch") {
            Some(Value::Null) => None,
            Some(Value::String(value)) if value.len() <= 256 => Some(value.clone()),
            _ => return Err("invalid_action: expected process_epoch is invalid".into()),
        };
        let foreground_pgid = object
            .get("foreground_pgid")
            .and_then(Value::as_str)
            .filter(|value| value.len() <= 256 && !value.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or("invalid_action: expected foreground_pgid is invalid")?;
        let tracked_digest = object
            .contains_key("tracked_digest")
            .then(|| digest("tracked_digest"))
            .transpose()?;
        let output_generation = match object.get("expected_output_generation") {
            None => None,
            Some(value) => Some(
                decimal_or_number(Some(value), "expected_output_generation")
                    .map_err(|_| "invalid_action: expected output generation is invalid")?,
            ),
        };
        let plain = |field: &str| {
            object
                .get(field)
                .and_then(Value::as_str)
                .filter(|value| value.len() <= 4_096 && !value.contains('\0'))
                .map(str::to_owned)
                .ok_or_else(|| format!("invalid_action: expected {field} is invalid"))
        };
        let title = object
            .contains_key("expected_title")
            .then(|| plain("expected_title"))
            .transpose()?;
        let progress = object
            .contains_key("expected_progress")
            .then(|| plain("expected_progress"))
            .transpose()?;
        Ok(Self {
            pane: text("pane_id")?,
            boot: text("boot")?,
            generation: text("generation")?,
            run: text("run")?,
            meta_digest: digest("meta_digest")?,
            tracked_digest,
            foreground_pgid,
            process_epoch,
            output_generation,
            title,
            progress,
        })
    }

    fn matches(&self, agent: &Agent, tracked: bool) -> Result<(), String> {
        if self.pane != agent.pane_id
            || self.boot != agent.boot
            || self.generation != agent.generation
            || self.run != agent.run
            || self.meta_digest != digest(&agent.encoded)
            || self.foreground_pgid
                != if agent.foreground_group > 0 {
                    agent.foreground_group.to_string()
                } else {
                    String::new()
                }
            || self.process_epoch != agent.process_epoch
            || agent.process != "running"
        {
            return Err(
                "identity_mismatch: agent foreground or run changed before input delivery".into(),
            );
        }
        if tracked {
            let current = digest(&agent.tracked_encoded);
            if self.tracked_digest.as_deref() != Some(current.as_str()) {
                return Err("rejected_before_effect:tracked_changed".into());
            }
        }
        Ok(())
    }
}

fn expected_for_action(
    value: Option<&Value>,
    pane: &str,
    tracked_required: bool,
) -> Result<Expected, String> {
    let expected = Expected::parse(
        value.ok_or("invalid_action: expected is required")?,
        tracked_required,
    )?;
    if expected.pane != pane {
        return Err("invalid_action: expected pane does not match target".into());
    }
    Ok(expected)
}

enum ControlAction {
    Keys {
        pane: String,
        expected: Expected,
        keys: Vec<String>,
        retry: Retry,
    },
    Interrupt {
        pane: String,
        expected: Expected,
        operation: Option<String>,
        keys: Vec<String>,
    },
    Close {
        pane: String,
        expected: Expected,
        operation: Option<String>,
    },
    Prompt {
        pane: String,
        expected: Expected,
        text: String,
        operation: Option<u64>,
        queued: Option<(i64, i64)>,
        needs_bracket: bool,
        checkpoint: bool,
    },
}

struct InterruptAction<'a> {
    pane: &'a str,
    expected: &'a Expected,
    operation: Option<&'a str>,
    keys: &'a [String],
}

struct DurableActionRequest<'a> {
    action: &'a str,
    operation: Option<&'a str>,
    payload_bytes: u64,
    /// Exact guarded-key frames that must fit before this action records its
    /// first dispatch ticket.
    preflight_keys: Option<(&'a [String], &'a str)>,
}

impl ControlAction {
    fn parse(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or("invalid_action: action parameters must be an object")?;
        let operation = object
            .get("operation")
            .and_then(Value::as_str)
            .ok_or("invalid_action: action operation is required")?;
        let allowed: &[&str] = match operation {
            "keys" => &["operation", "pane_id", "expected", "keys", "retry"],
            "interrupt" => &["operation", "pane_id", "expected", "operation_id", "keys"],
            "close" => &["operation", "pane_id", "expected", "operation_id"],
            "prompt" => &[
                "operation",
                "pane_id",
                "expected",
                "text",
                "operation_id",
                "queued",
                "needs_bracket",
                "submit",
                "checkpoint",
            ],
            _ => return Err("invalid_action: unknown core action".into()),
        };
        if object
            .keys()
            .any(|field| !allowed.contains(&field.as_str()))
        {
            return Err("invalid_action: unknown action field".into());
        }
        let pane = object
            .get("pane_id")
            .and_then(Value::as_str)
            .filter(|pane| {
                pane.len() > 1
                    && pane.starts_with('%')
                    && pane[1..].bytes().all(|byte| byte.is_ascii_digit())
            })
            .map(str::to_owned)
            .ok_or("invalid_action: pane_id is invalid")?;
        let operation_id = match object.get("operation_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) if valid_operation_id(id) => Some(id.clone()),
            _ if operation == "prompt" => None,
            _ => return Err("invalid_action: operation_id is invalid".into()),
        };
        match operation {
            "keys" => {
                let keys = parse_keys(object.get("keys"))?;
                let retry = match object.get("retry").and_then(Value::as_str) {
                    Some("never") => Retry::Never,
                    Some("same_run") => Retry::SameRun,
                    _ => return Err("invalid_action: retry is invalid".into()),
                };
                let expected = expected_for_action(
                    object.get("expected"),
                    &pane,
                    matches!(retry, Retry::Never),
                )?;
                Ok(Self::Keys {
                    pane,
                    expected,
                    keys,
                    retry,
                })
            }
            "interrupt" => {
                let expected = expected_for_action(object.get("expected"), &pane, true)?;
                let keys = parse_keys(object.get("keys"))?;
                Ok(Self::Interrupt {
                    pane,
                    expected,
                    operation: operation_id,
                    keys,
                })
            }
            "close" => {
                let expected = expected_for_action(object.get("expected"), &pane, false)?;
                Ok(Self::Close {
                    pane,
                    expected,
                    operation: operation_id,
                })
            }
            "prompt" => {
                if operation_id.is_some() {
                    return Err("invalid_action: prompt operation_id is invalid".into());
                }
                let expected = expected_for_action(object.get("expected"), &pane, true)?;
                let (Some(_), Some(title), Some(progress)) = (
                    expected.output_generation,
                    expected.title.as_ref(),
                    expected.progress.as_ref(),
                ) else {
                    return Err("invalid_action: prompt expected snapshot is incomplete".into());
                };
                if title.len() > 4_096 || progress.len() > 4_096 {
                    return Err("invalid_action: prompt expected snapshot is invalid".into());
                }
                let text = object
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| {
                        !text.is_empty()
                            && text.len() <= 32_768
                            && !text
                                .chars()
                                .any(|c| c.is_control() && c != '\n' && c != '\t')
                    })
                    .map(str::to_owned)
                    .ok_or("invalid_action: prompt text is invalid")?;
                let prompt_operation = match object.get("operation_id") {
                    None | Some(Value::Null) => None,
                    Some(value) => Some(
                        decimal_or_number(Some(value), "prompt operation")
                            .map_err(|_| "invalid_action: prompt operation is invalid")?,
                    ),
                };
                let queued = match object.get("queued") {
                    None | Some(Value::Null) => None,
                    Some(Value::Object(value))
                        if value.len() == 2
                            && value.contains_key("item")
                            && value.contains_key("revision") =>
                    {
                        let item = value
                            .get("item")
                            .and_then(Value::as_i64)
                            .filter(|item| *item > 0)
                            .ok_or("invalid_action: queued item is invalid")?;
                        let revision = value
                            .get("revision")
                            .and_then(Value::as_i64)
                            .filter(|revision| *revision >= 0)
                            .ok_or("invalid_action: queued revision is invalid")?;
                        Some((item, revision))
                    }
                    _ => return Err("invalid_action: queued prompt is invalid".into()),
                };
                if prompt_operation.is_some() && queued.is_some() {
                    return Err("invalid_action: queued prompts cannot name an operation".into());
                }
                let needs_bracket = object
                    .get("needs_bracket")
                    .and_then(Value::as_bool)
                    .ok_or("invalid_action: prompt needs_bracket is invalid")?;
                if needs_bracket != (text.contains('\n') || text.contains('\t')) {
                    return Err("invalid_action: prompt bracket rule is invalid".into());
                }
                if object.get("submit").and_then(Value::as_str) != Some("enter") {
                    return Err("invalid_action: prompt submit is invalid".into());
                }
                let checkpoint = object
                    .get("checkpoint")
                    .and_then(Value::as_bool)
                    .ok_or("invalid_action: prompt checkpoint is invalid")?;
                Ok(Self::Prompt {
                    pane,
                    expected,
                    text,
                    operation: prompt_operation,
                    queued,
                    needs_bracket,
                    checkpoint,
                })
            }
            _ => unreachable!(),
        }
    }

    fn pane(&self) -> &str {
        match self {
            Self::Keys { pane, .. }
            | Self::Interrupt { pane, .. }
            | Self::Close { pane, .. }
            | Self::Prompt { pane, .. } => pane,
        }
    }
}

fn parse_keys(value: Option<&Value>) -> Result<Vec<String>, String> {
    let keys = value
        .and_then(Value::as_array)
        .ok_or("invalid_action: keys must be an array")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or("invalid_action: key must be a string".into())
        })
        .collect::<Result<Vec<_>, String>>()?;
    validate_keys(&keys)?;
    Ok(keys)
}

fn validate_keys(keys: &[String]) -> Result<(), String> {
    if keys.len() > 64
        || keys.iter().map(String::len).sum::<usize>() > 8192
        || keys.iter().any(|key| key.chars().any(char::is_control))
    {
        return Err(
            "invalid_argument: arguments exceed bounds or contain control characters".into(),
        );
    }
    Ok(())
}

fn valid_operation_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
}

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn preconditions(agent: &Agent, tracked_digest: Option<String>) -> Preconditions {
    Preconditions {
        core_boot_id: agent.boot.clone(),
        pane_id: agent.pane_id.clone(),
        pty_generation: agent.generation.clone(),
        foreground_pgid: if agent.foreground_group > 0 {
            agent.foreground_group.to_string()
        } else {
            String::new()
        },
        current_command: agent.foreground_command.clone(),
        meta_digest: digest(&agent.encoded),
        tracked_digest,
        expected_output_generation: None,
        expected_title: None,
        expected_progress: None,
    }
}

pub(super) fn same_core_run(before: &Agent, after: &Agent) -> bool {
    before.pane_id == after.pane_id
        && before.boot == after.boot
        && before.generation == after.generation
        && before.run == after.run
        && before.foreground_group == after.foreground_group
        && before.process_epoch == after.process_epoch
        && after.process == "running"
}

/// Only the core's TRACKED guard is eligible for the caller-side interrupt
/// re-plan. Every other refusal is a completed first judgment.
pub(super) fn tracked_changed(error: &str) -> bool {
    error == "rejected_before_effect:tracked_changed"
        || error == "rejected_before_effect: rejected_before_effect:tracked_changed"
}

#[derive(Default)]
pub(super) struct TrackedChangeRetry {
    used: bool,
}

impl TrackedChangeRetry {
    /// Consumes the one caller-side re-plan allowed for a changed TRACKED
    /// value in the same foreground run.
    pub(super) fn take(&mut self, error: &str) -> bool {
        if self.used || !tracked_changed(error) {
            return false;
        }
        self.used = true;
        true
    }
}

pub(crate) fn expected(agent: &Agent) -> Value {
    json!({
        "pane_id": agent.pane_id,
        "boot": agent.boot,
        "generation": agent.generation,
        "run": agent.run,
        "meta_digest": digest(&agent.encoded),
        "tracked_digest": digest(&agent.tracked_encoded),
        "foreground_pgid": if agent.foreground_group > 0 { agent.foreground_group.to_string() } else { String::new() },
        "process_epoch": agent.process_epoch,
    })
}

pub(crate) fn prompt_expected(agent: &Agent) -> Value {
    let mut value = expected(agent);
    value["expected_output_generation"] = json!(agent.output_generation.to_string());
    value["expected_title"] = json!(&agent.title);
    value["expected_progress"] = json!(&agent.progress);
    value
}

fn durable_key(socket: &Path, agent: &Agent, nonce: &str, operation_id: &str) -> OperationKey {
    let socket = Sha256::digest(socket.as_os_str().as_bytes());
    let environment_id: String = socket[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    OperationKey {
        environment_id,
        principal: "local".into(),
        namespace_epoch: agent.boot.clone(),
        namespace_nonce: nonce.into(),
        operation_id: operation_id.into(),
    }
}

fn ephemeral_key(socket: &Path, agent: &Agent, nonce: &str, operation: &str) -> OperationKey {
    durable_key(socket, agent, nonce, &digest(operation))
}

fn core_intent(operation_key: &OperationKey, ticket: DispatchTicket) -> Value {
    json!({
        "path": "core_ledger",
        "core": {
            "operation_key": operation_key.value(),
            "dispatch_ticket": ticket.value(),
        },
    })
}

fn core_result_evidence(
    operation_key: &OperationKey,
    ticket: DispatchTicket,
    result_digest: &str,
    result: &str,
) -> Value {
    json!({
        "path": "core_ledger",
        "core_ticket": ticket.value(),
        "core_operation_key": operation_key.value(),
        "result_digest": result_digest,
        "result": result,
    })
}

fn input_result_evidence(
    operation_key: &OperationKey,
    ticket: DispatchTicket,
    result_digest: &str,
    result: &str,
    queued_bytes: Option<u64>,
) -> Value {
    let mut evidence = core_result_evidence(operation_key, ticket, result_digest, result);
    if let Some(queued_bytes) = queued_bytes {
        evidence["queued_bytes"] = json!(queued_bytes);
    }
    evidence
}

fn prompt_rejection(slot: u64, reason: &str) -> String {
    format!(
        "rejected_before_effect: prompt operation {slot} was rejected or its delivery is unknown: {reason}; query prompt-receipt before retrying"
    )
}

fn prompt_staging_failure(slot: u64, error: &Error) -> (&'static str, String) {
    match error {
        Error::Rejected { .. } | Error::InvalidArgument(_) | Error::Busy(_) => (
            "rejected_before_effect",
            prompt_rejection(slot, &error.to_string()),
        ),
        // Staging has no input effect and does not reserve a dispatch
        // sequence. Even a lost or malformed staging reply therefore proves
        // this prompt did not reach `input_commit`.
        Error::Unavailable(_) | Error::Lost(_) | Error::Protocol(_) => (
            "not_applied",
            format!("not_applied: prompt input staging ended before input_commit: {error}"),
        ),
    }
}

fn prompt_transport_failure(slot: u64, error: &Error) -> (&'static str, String) {
    match error {
        // `invalid_action` and an exhausted `bridge_busy` reply are known
        // before the core reserves an input_commit ticket.  Keep their
        // prompt receipt in the native path's refusal class rather than
        // inventing a retryable no-op outcome.
        Error::InvalidArgument(_) | Error::Busy(_) | Error::Rejected { .. } => (
            "rejected_before_effect",
            prompt_rejection(slot, &error.to_string()),
        ),
        Error::Unavailable(_) | Error::Lost(_) | Error::Protocol(_) => {
            (UNKNOWN, format!("outcome_unknown: {error}"))
        }
    }
}

fn prompt_core_state(record: &Record, result: &LedgerResult) -> Option<&'static str> {
    if record.action != "prompt" {
        return None;
    }
    match result {
        // An applied input_commit is a delivery receipt only when the core
        // retained its byte-count evidence.  A prompt can never legitimately
        // queue zero bytes (it has text and an Enter), so an omitted value is
        // not enough to promote an uncertain crash recovery to delivered.
        LedgerResult::Applied {
            queued_bytes: Some(_),
            ..
        } => Some("delivered"),
        LedgerResult::Applied { .. } => None,
        LedgerResult::RejectedBeforeEffect { .. } => Some("rejected_before_effect"),
        LedgerResult::NotApplied { .. } => Some("not_applied"),
        LedgerResult::LedgerFull
        | LedgerResult::IdempotencyConflict
        | LedgerResult::ReceiptNotRetained
        | LedgerResult::TicketGap
        | LedgerResult::EpochClosed
        | LedgerResult::EpochUnknown => None,
    }
}

fn ledger_result_evidence(entry: &LedgerEntry) -> Value {
    let mut evidence = json!({
        "path": "core_ledger",
        "core_ticket": entry.ticket.value(),
        "core_operation_key": entry.operation_key.value(),
        "result_digest": entry.result.result_digest(),
        "result": entry.result.result_text(),
    });
    if let LedgerResult::RejectedBeforeEffect { reason, .. } = &entry.result {
        evidence["reason"] = json!(reason);
    }
    if let LedgerResult::Applied {
        queued_bytes: Some(queued_bytes),
        ..
    } = &entry.result
    {
        evidence["queued_bytes"] = json!(queued_bytes);
    }
    evidence
}

#[derive(Clone)]
struct StoredIntent {
    ticket: DispatchTicket,
    operation_key: OperationKey,
}

fn core_intents(record: &Record) -> Vec<StoredIntent> {
    record
        .receipts
        .iter()
        .filter_map(|receipt| receipt.evidence.as_ref())
        .filter_map(|evidence| evidence.get("core"))
        .filter_map(|core| {
            let object = core.as_object()?;
            let ticket = DispatchTicket::parse(object.get("dispatch_ticket")).ok()?;
            let key = object.get("operation_key")?.as_object()?;
            Some(StoredIntent {
                ticket,
                operation_key: OperationKey {
                    environment_id: key.get("environment_id")?.as_str()?.to_owned(),
                    principal: key.get("principal")?.as_str()?.to_owned(),
                    namespace_epoch: key.get("namespace_epoch")?.as_str()?.to_owned(),
                    namespace_nonce: key.get("namespace_nonce")?.as_str()?.to_owned(),
                    operation_id: key.get("operation_id")?.as_str()?.to_owned(),
                },
            })
        })
        .collect()
}

fn ticket_for(record: &Record) -> Ticket {
    Ticket {
        op: record.op,
        key: record.operation_key.clone(),
        ticket: record.ticket.clone(),
    }
}

/// A retained entry is ours only when both its ticket and operation-key
/// digest match. Tickets alone are boot-local and a stale SQLite intent must
/// never retire another operation's ledger slot.
fn stored_intent_for_entry<'a>(
    records: &'a [Record],
    entry: &LedgerEntry,
) -> Option<(&'a Record, StoredIntent)> {
    records
        .iter()
        .flat_map(|record| {
            core_intents(record)
                .into_iter()
                .map(move |intent| (record, intent))
        })
        .find(|(_, intent)| {
            intent.ticket == entry.ticket
                && intent.operation_key.ledger_digest() == entry.operation_key_digest
                && intent.operation_key == entry.operation_key
        })
}

/// An interrupt can have delivered an earlier key before the coordinator
/// stopped. If a later ticket is proven not to have run during reconciliation,
/// preserve that partial outcome rather than claiming the whole operation did
/// not apply.
fn has_core_effect(record: &Record) -> bool {
    applied_core_effects(record) > 0
}

fn recorded_prompt_core_state(record: &Record) -> Option<&'static str> {
    if record.action != "prompt" {
        return None;
    }
    let intent = core_intents(record).pop()?;
    record.receipts.iter().rev().find_map(|receipt| {
        let evidence = receipt.evidence.as_ref()?;
        [evidence, &evidence["evidence"]]
            .into_iter()
            .find_map(|evidence| {
                core_receipt_matches(evidence, intent.ticket, &intent.operation_key).then(|| {
                    match evidence["result"].as_str() {
                        // A staged input is a delivery only if the retained core
                        // answer included its queued byte count.
                        Some("applied") if evidence["queued_bytes"].as_u64().is_some() => {
                            Some("delivered")
                        }
                        Some("rejected_before_effect") => Some("rejected_before_effect"),
                        Some("not_applied") => Some("not_applied"),
                        _ => None,
                    }
                })?
            })
    })
}

fn applied_core_effects(record: &Record) -> usize {
    record
        .receipts
        .iter()
        .filter(|receipt| {
            receipt.evidence.as_ref().is_some_and(|evidence| {
                evidence["path"] == "core_ledger"
                    && evidence["result"] == "applied"
                    && evidence["core_ticket"].is_object()
            })
        })
        .count()
}

fn core_receipt_matches(
    evidence: &Value,
    ticket: DispatchTicket,
    operation_key: &OperationKey,
) -> bool {
    evidence["path"] == "core_ledger"
        && evidence["core_ticket"] == ticket.value()
        && evidence["core_operation_key"] == operation_key.value()
}

fn has_ticket_receipt(
    record: &Record,
    ticket: DispatchTicket,
    operation_key: &OperationKey,
) -> bool {
    record.receipts.iter().any(|receipt| {
        receipt.evidence.as_ref().is_some_and(|evidence| {
            core_receipt_matches(evidence, ticket, operation_key)
                // `finish_admitted` preserves failed-receipt evidence under
                // this key. A staging failure did not consume its ticket, so
                // matching only its ticket could hide a later action that
                // reused that ticket with a new operation key.
                || core_receipt_matches(&evidence["evidence"], ticket, operation_key)
        })
    })
}

fn receipt(record: &Record) -> Value {
    json!({
        "stage": record.state,
        "pane_id": record.target,
        "run": record.run,
        "operation_key": record.operation_key,
        "provider_accepted": false,
        "path": "core_ledger",
    })
}

/// A retained entry without a matching SQLite intent cannot be sent again:
/// the ledger proves an earlier coordinator got far enough to reserve it.
/// An entry with no SQLite intent (a lost or reset store) is recorded as
/// outcome_unknown, then retired with the complete key the core returned, so
/// the slot is not pinned for the rest of the boot.
async fn retire_orphan(
    store: &mut Store,
    core: &mut Client,
    boot: &str,
    entry: &LedgerEntry,
) -> Result<(), String> {
    record_orphan(store, boot, entry)?;
    let result_digest = entry
        .result
        .result_digest()
        .ok_or("retained ledger entry has no result digest")?;
    core.retire_receipt(&entry.operation_key, entry.ticket, result_digest, now_ms())
        .await
        .map_err(|error| error.to_string())
}

fn record_orphan(store: &mut Store, boot: &str, entry: &LedgerEntry) -> Result<(), String> {
    let namespace = format!("core:{}", entry.ticket.epoch);
    let new = NewOperation {
        namespace: &namespace,
        action: "ledger_orphan",
        // `operation_id` alone is not unique across randomized operation
        // keys. The digest is stable, printable, and tells an operator which
        // unrecoverable retained slot this record describes.
        id: &entry.operation_key_digest,
        explicit: true,
        target: "core_ledger",
        run: None,
        boot,
        digest: &entry.payload_digest,
        payload_bytes: 0,
    };
    let evidence = json!({
        "path": "core_ledger",
        "orphan_ticket": entry.ticket.value(),
        "operation_key_digest": entry.operation_key_digest,
        "operation_id": entry.operation_id,
        "result": entry.result.result_text(),
    });
    if let Admission::Dispatch(ticket) = store.admit(&new, evidence.clone(), now_ms())? {
        store.finish(
            &ticket,
            UNKNOWN,
            "core_ledger_reconcile",
            Some(&evidence),
            now_ms(),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(result: &str) -> Value {
        json!({
            "v": 1,
            "kind": "guarded_action",
            "request_id": "r",
            "dispatch_ticket": {"epoch": "7", "seq": "2"},
            "result": result,
            "result_digest": "a".repeat(64),
        })
    }

    #[test]
    fn maps_core_results_without_collapsing_rejections() {
        let ticket = DispatchTicket { epoch: 7, seq: 2 };
        assert!(matches!(
            parse_ledger_reply(&reply("applied"), "guarded_action", ticket).unwrap(),
            LedgerResult::Applied { .. }
        ));
        assert!(matches!(
            parse_ledger_reply(
                &reply("rejected_before_effect:tracked_changed"),
                "guarded_action",
                ticket,
            )
            .unwrap(),
            LedgerResult::RejectedBeforeEffect { reason, .. } if reason == "tracked_changed"
        ));
        assert!(matches!(
            parse_ledger_reply(&reply("not_applied"), "guarded_action", ticket).unwrap(),
            LedgerResult::NotApplied { .. }
        ));
    }

    #[test]
    fn coordinator_prompt_failure_mapping_keeps_guard_refusals_final() {
        let rejected = Error::Rejected {
            code: "staging_full".into(),
            message: "no slot".into(),
        };
        let (state, message) = prompt_staging_failure(4, &rejected);
        assert_eq!(state, "rejected_before_effect");
        assert!(message.starts_with("rejected_before_effect:"));

        let staging_lost = Error::Lost("peer closed".into());
        let (state, message) = prompt_staging_failure(4, &staging_lost);
        assert_eq!(state, "not_applied");
        assert!(message.starts_with("not_applied:"));

        let lost = Error::Lost("peer closed".into());
        let (state, message) = prompt_transport_failure(4, &lost);
        assert_eq!(state, UNKNOWN);
        assert!(message.starts_with("outcome_unknown:"));

        let busy = Error::Busy("no reply slot".into());
        let (state, message) = prompt_transport_failure(4, &busy);
        assert_eq!(state, "rejected_before_effect");
        assert!(message.starts_with("rejected_before_effect:"));

        let prompt = json!({
            "operation": "prompt",
            "pane_id": "%3",
            "expected": {
                "pane_id": "%3",
                "boot": "boot",
                "generation": "1",
                "run": "run",
                "meta_digest": "a".repeat(64),
                "tracked_digest": "b".repeat(64),
                "foreground_pgid": "42",
                "process_epoch": "42:1",
                "expected_output_generation": "9",
                "expected_title": "title",
                "expected_progress": "progress"
            },
            "text": "line one\nline two",
            "operation_id": 4,
            "queued": null,
            "needs_bracket": true,
            "submit": "enter",
            "checkpoint": true
        });
        assert!(matches!(
            ControlAction::parse(&prompt),
            Ok(ControlAction::Prompt {
                operation: Some(4),
                needs_bracket: true,
                ..
            })
        ));
    }

    #[test]
    fn maps_ledger_error_codes() {
        let ticket = DispatchTicket { epoch: 1, seq: 0 };
        for (code, wanted) in [
            ("ledger_full", LedgerResult::LedgerFull),
            ("idempotency_conflict", LedgerResult::IdempotencyConflict),
            ("receipt_not_retained", LedgerResult::ReceiptNotRetained),
            ("ticket_gap", LedgerResult::TicketGap),
            ("epoch_closed", LedgerResult::EpochClosed),
            ("epoch_unknown", LedgerResult::EpochUnknown),
        ] {
            let value = json!({
                "v": 1,
                "kind": "error",
                "request_id": "r",
                "code": code,
                "message": code,
            });
            assert_eq!(
                parse_ledger_reply(&value, "guarded_action", ticket).unwrap(),
                wanted
            );
        }
    }

    #[test]
    fn tracked_digest_is_optional_only_for_same_run_actions() {
        let expected = json!({
            "pane_id": "%3",
            "boot": "boot",
            "generation": "1",
            "run": "run",
            "meta_digest": "a".repeat(64),
            "foreground_pgid": "42",
            "process_epoch": "42:1",
        });
        assert!(Expected::parse(&expected, false).is_ok());
        assert!(Expected::parse(&expected, true).is_err());
    }

    #[test]
    fn ticket_order_advances_only_after_a_consumed_result() {
        let ticket = DispatchTicket { epoch: 1, seq: 0 };
        assert!(consumes_ticket(&LedgerResult::Applied {
            ticket,
            result_digest: "a".repeat(64),
            queued_bytes: None,
        }));
        assert!(consumes_ticket(&LedgerResult::LedgerFull));
        assert!(!consumes_ticket(&LedgerResult::TicketGap));
        assert!(!consumes_ticket(&LedgerResult::EpochClosed));
    }

    #[test]
    fn reconciliation_never_resends_a_retained_ticket() {
        let ticket = DispatchTicket { epoch: 4, seq: 9 };
        let applied = LedgerResult::Applied {
            ticket,
            result_digest: "b".repeat(64),
            queued_bytes: None,
        };
        let not_applied = LedgerResult::NotApplied {
            ticket,
            result_digest: "c".repeat(64),
        };
        assert_eq!(
            reconcile_decision(true, &applied),
            ReconcileDecision::Retire
        );
        assert_eq!(
            reconcile_decision(false, &applied),
            ReconcileDecision::OutcomeUnknown
        );
        assert_eq!(
            reconcile_decision(false, &not_applied),
            ReconcileDecision::OutcomeUnknown
        );
    }

    #[test]
    fn staging_receipt_does_not_hide_an_applied_retry_with_the_same_ticket() {
        // input_begin failure consumes neither the ticket nor an input_commit
        // slot. A retry therefore gets a fresh operation key but can use the
        // same ticket. If it applies and SQLite loses the final receipt, only
        // the retry key identifies the retained core entry.
        let ticket = DispatchTicket { epoch: 4, seq: 9 };
        let staged_key = OperationKey {
            environment_id: "env".into(),
            principal: "local".into(),
            namespace_epoch: "boot".into(),
            namespace_nonce: "staging".into(),
            operation_id: "prompt".into(),
        };
        let retry_key = OperationKey {
            namespace_nonce: "retry".into(),
            ..staged_key.clone()
        };
        let sqlite_ticket = "sqlite-attempt".to_owned();
        let mut first_intent = core_intent(&staged_key, ticket);
        first_intent["ticket"] = json!(sqlite_ticket);
        let mut retry_intent = core_intent(&retry_key, ticket);
        retry_intent["ticket"] = json!(sqlite_ticket);
        let staging_failure = json!({
            "path": "core_ledger",
            "core_ticket": ticket.value(),
            "core_operation_key": staged_key.value(),
            "result": "rejected_before_effect",
        });
        let record = Record {
            op: 1,
            operation_key: "run:r/prompt/1".into(),
            action: "prompt".into(),
            id: "1".into(),
            explicit: true,
            target: "%1".into(),
            run: Some("r".into()),
            boot: "boot".into(),
            digest: "digest".into(),
            payload_bytes: 7,
            state: super::super::operations::DISPATCHING.into(),
            ticket: sqlite_ticket,
            attempts: 2,
            lease_until_ms: 0,
            created_ms: 0,
            updated_ms: 0,
            compacted_ms: None,
            receipts: vec![
                super::super::operations::ReceiptRow {
                    ordinal: 1,
                    stage: "dispatch_intent".into(),
                    source: "core_ledger".into(),
                    evidence: Some(first_intent),
                    at_ms: 0,
                },
                super::super::operations::ReceiptRow {
                    ordinal: 2,
                    stage: "rejected_before_effect".into(),
                    source: "core_ledger".into(),
                    evidence: Some(staging_failure),
                    at_ms: 0,
                },
                super::super::operations::ReceiptRow {
                    ordinal: 3,
                    stage: "dispatch_intent".into(),
                    source: "core_ledger".into(),
                    evidence: Some(retry_intent),
                    at_ms: 0,
                },
            ],
        };
        let entry = LedgerEntry {
            ticket,
            operation_id: retry_key.operation_id.clone(),
            operation_key_digest: retry_key.ledger_digest(),
            operation_key: retry_key.clone(),
            payload_digest: "a".repeat(64),
            result: LedgerResult::Applied {
                ticket,
                result_digest: "b".repeat(64),
                queued_bytes: Some(8),
            },
        };

        assert!(stored_intent_for_entry(std::slice::from_ref(&record), &entry).is_some());
        assert!(has_ticket_receipt(&record, ticket, &staged_key));
        assert!(!has_ticket_receipt(&record, ticket, &retry_key));
        assert_eq!(recorded_prompt_core_state(&record), None);
        assert_eq!(
            reconcile_decision(
                has_ticket_receipt(&record, ticket, &retry_key),
                &entry.result
            ),
            ReconcileDecision::OutcomeUnknown,
        );
        assert_eq!(prompt_core_state(&record, &entry.result), Some("delivered"));
    }

    #[test]
    fn coordinator_interrupt_accepts_only_a_caller_resolved_plan() {
        let expected = json!({
            "pane_id": "%3",
            "boot": "boot",
            "generation": "1",
            "run": "run",
            "meta_digest": "a".repeat(64),
            "tracked_digest": "b".repeat(64),
            "foreground_pgid": "42",
            "process_epoch": "42:1",
        });
        let plan = json!({
            "operation": "interrupt",
            "pane_id": "%3",
            "expected": expected,
            "keys": ["Escape"],
        });
        assert!(matches!(
            ControlAction::parse(&plan).unwrap(),
            ControlAction::Interrupt { keys, .. } if keys == vec!["Escape".to_owned()]
        ));
        let mut stale_shape = plan;
        stale_shape["any_state"] = json!(true);
        assert!(ControlAction::parse(&stale_shape).is_err());
    }

    #[test]
    fn tracked_changed_consumes_only_one_caller_replan() {
        let mut retry = TrackedChangeRetry::default();
        assert!(retry.take("rejected_before_effect: rejected_before_effect:tracked_changed"));
        assert!(!retry.take("rejected_before_effect:tracked_changed"));
        assert!(!retry.take("rejected_before_effect:meta_changed"));
    }

    #[test]
    fn ledger_epoch_pages_preserve_closed_epochs_and_the_active_epoch() {
        let first = parse_ledger_epochs(&json!({
            "v": 1,
            "kind": "ledger_epochs",
            "request_id": "one",
            "epochs": [{"epoch": "2", "watermark": "7", "retained": 1}],
            "next_epoch": "2",
            "truncated": true,
        }))
        .unwrap();
        let second = parse_ledger_epochs(&json!({
            "v": 1,
            "kind": "ledger_epochs",
            "request_id": "two",
            "epochs": [{"epoch": "3", "watermark": "1", "retained": 0}],
            "next_epoch": "3",
            "truncated": false,
        }))
        .unwrap();
        assert_eq!(first.epochs[0].epoch, 2);
        assert!(first.truncated);
        assert_eq!(
            second.epochs[0],
            LedgerEpoch {
                epoch: 3,
                watermark: 1,
                retained: 0
            }
        );
    }

    #[test]
    fn bridge_busy_retries_without_advancing_a_request() {
        let mut retry = BusyRetry::new();
        for delay in [50, 100, 200, 400] {
            assert_eq!(retry.next_delay(), Some(Duration::from_millis(delay)));
        }
        assert_eq!(retry.next_delay(), None);
        assert_eq!(retry.attempts(), BUSY_ATTEMPTS);
    }

    #[tokio::test]
    async fn pane_lock_serializes_one_pane_but_not_another() {
        let locks = Arc::new(PaneActionLocks::default());
        let first = locks.lock("%1").await;
        assert!(locks.is_locked("%1"));
        assert!(!locks.is_locked("%2"));
        let other = locks.lock("%2").await;
        assert!(locks.is_locked("%2"));
        drop(other);
        drop(first);
        assert!(!locks.is_locked("%1"));
    }
}
