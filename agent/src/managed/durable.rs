//! Durable start, interrupt and close, and the `operations` commands.
//! Prompt delivery keeps its own ledger in `prompt.rs`.
//!
//! Interrupt and close write a pending entry to a per-run effects option in
//! the same guarded group as the effect, compare-and-set against the value
//! read before admission. Reconcile writes a fence the same way before it
//! reports that an attempt did not apply.
use super::operations::{self, Admission, NewOperation, Record, Store, Ticket};
use super::{
    Agent, Guarded, Manager, and, decode, encode, failure::AfterEffect, identity_guard, nonce,
};
use crate::observation::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

const EFFECTS: &str = "@masil-agent-operation-effects";
const EFFECTS_FENCE: &str = "@masil-agent-effects-fence";
const MAX_EFFECTS: usize = 16;
const RECONCILE_BATCH: usize = 64;

#[derive(Default, Serialize, Deserialize)]
struct Effects {
    run: String,
    entries: Vec<EffectEntry>,
}

#[derive(Serialize, Deserialize)]
struct EffectEntry {
    key: String,
    ticket: String,
    stage: String,
}

/// A client-chosen operation ID and the namespace the client observed it in:
/// the run for run-bound actions, the server boot for launches.
#[derive(Clone, Copy)]
pub(crate) struct ClientKey<'a> {
    pub pin: &'a str,
    pub id: &'a str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Effect {
    Interrupt,
    Close,
}

/// What a person asked of an interrupt beyond its agent (the CLI; the
/// desk, endpoints and the API ask for neither).
#[derive(Clone, Default)]
pub(crate) struct InterruptRequest {
    /// Send even without an interruptible turn on the screen.
    pub(crate) any_state: bool,
    /// These keys instead of the provider's measured ones.
    pub(crate) keys: Option<Vec<String>>,
}

/// How an interrupt reaches the provider.
pub(super) enum Via {
    /// Keys into the pane, in order.
    Keys(Vec<String>),
    /// OpenCode's own abort, over its answer channel.
    Abort,
}

impl Effect {
    fn action(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::Close => "close",
        }
    }

    fn done(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt_key_delivered",
            Self::Close => "pane_closed",
        }
    }
}

fn sha256(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn transport_rejected(error: &str) -> bool {
    // tmux reported a failed command; a timeout or read error is not proof.
    error.starts_with("native command failed:")
        || error.starts_with("native command exited with exit status")
}

/// The public receipt of a run-bound effect, rebuilt identically on retry.
fn effect_receipt(record: &Record) -> Value {
    let path = record
        .intent()
        .and_then(|intent| intent["path"].as_str())
        .unwrap_or("native_guard");
    json!({"stage":record.state,"pane_id":record.target,"run":record.run,
        "operation_key":record.operation_key,"provider_accepted":false,"path":path})
}

fn finish_effect(
    store: &mut Store,
    ticket: &Ticket,
    effect: Effect,
    now: u64,
) -> Result<Value, AfterEffect> {
    store
        .finish(ticket, effect.done(), "native_group", None, now)
        .map(|record| effect_receipt(&record))
        .map_err(|error| {
            AfterEffect::new(format!(
                "{} applied, but its durable receipt could not be committed: {error}; query `agent operation {}`",
                effect.action(),
                ticket.key
            ))
        })
}

fn start_receipt(record: &Record) -> Value {
    record
        .receipts
        .iter()
        .rev()
        .find(|receipt| receipt.stage == record.state)
        .and_then(|receipt| receipt.evidence.clone())
        .filter(|evidence| evidence.get("stage").is_some())
        .unwrap_or_else(|| {
            json!({"stage":record.state,"name":record.target,"operation_key":record.operation_key,
                "native_session_verified":false,"provider_accepted":false})
        })
}

pub(super) fn launch_digest(
    name: &str,
    provider: &str,
    cwd: &std::path::Path,
    args: &[String],
    session: Option<&str>,
    split: Option<&str>,
) -> String {
    sha256(&json!([name, provider, cwd, args, session, split]).to_string())
}

impl Manager {
    /// Runs of live panes, as `collect` names them, and the server boot.
    pub(super) async fn live_runs(&self) -> Result<(HashSet<String>, String), String> {
        let inventory = self.inventory().await?;
        super::validate_inventory(&inventory)?;
        let boot = match inventory.first() {
            Some(fields) => fields[5].clone(),
            None => self.boot().await?,
        };
        // Every name a pane's run can have: stale metadata makes `collect`
        // fall back to the synthetic one, and a stopped agent (Ctrl-Z) is
        // no longer the foreground but keeps its tracked run.
        let runs = inventory
            .iter()
            .flat_map(|fields| {
                let synthetic = format!("{}-{}-{}-{}", fields[5], fields[0], fields[6], fields[13]);
                // A reused group ID's run carries its leader's start time.
                let recorded = super::epoch_records(&fields[17])
                    .map(|(run, _)| run.to_owned())
                    .collect::<Vec<_>>();
                decode::<super::Metadata>(&fields[11])
                    .map(|meta| meta.run)
                    .into_iter()
                    .chain(decode::<super::Tracked>(&fields[12]).map(|tracked| tracked.run))
                    .chain([synthetic])
                    .chain(recorded)
            })
            .collect();
        Ok((runs, boot))
    }

    /// Delete resolved records whose namespace ended, when the store has grown.
    pub(super) async fn maintain(&self, store: &mut Store) -> Result<(), String> {
        let now = now_ms();
        if store.needs_prune(now)? {
            let (runs, boot) = self.live_runs().await?;
            store.prune(&runs, &boot, now)?;
        }
        Ok(())
    }

    pub async fn interrupt(
        &self,
        agent: &Agent,
        key: Option<ClientKey<'_>>,
    ) -> Result<Value, String> {
        let receipt = self
            .interrupt_with(agent, key, &InterruptRequest::default())
            .await?;
        Ok(self.confirm_interrupt(agent, receipt, 0, 0).await)
    }

    /// Interrupts the agent's turn: with the provider's measured keys, or
    /// OpenCode's abort, and only while its screen shows a turn those end.
    pub async fn interrupt_with(
        &self,
        agent: &Agent,
        key: Option<ClientKey<'_>>,
        request: &InterruptRequest,
    ) -> Result<Value, String> {
        if self.action_path().await? == super::ActionPath::CoreLedger {
            if let Some(key) = key
                && key.pin != agent.run
            {
                return Err(
                    "identity_mismatch: interrupt target run changed; this operation cannot be replayed"
                        .into(),
                );
            }
            let mut store = self.operation_store().await?;
            // Native durable admission returns an existing final receipt
            // before it judges the screen. Preserve that order here: a
            // keyed retry after the turn ended must not be refused merely
            // because the screen is no longer interruptible.
            if let Some(key) = key {
                if !operations::valid_id(key.id) {
                    return Err(
                        "invalid_argument: operation ID must be 1–64 letters, digits, '.', '_', ':' or '-'"
                            .into(),
                    );
                }
                let operation_key =
                    operations::key(&format!("run:{}", agent.run), "interrupt", key.id);
                if let Some(record) = store.get(&operation_key)?
                    && !matches!(
                        record.state.as_str(),
                        operations::DISPATCHING | "rejected_before_effect" | "not_applied"
                    )
                {
                    return Ok(effect_receipt(&record));
                }
            }
            if agent.process != "running" {
                return Err("identity_mismatch: agent is not verified in the foreground".into());
            }
            let (current, via) = self.interrupt_plan(&mut store, agent, request).await?;
            match via {
                // The caller owns the screen judgment. The coordinator only
                // checks identity and dispatches these already-selected keys.
                Via::Keys(keys) => {
                    let mut tracked_retry = super::core_action::TrackedChangeRetry::default();
                    let result = self
                        .core_durable_action(
                            &current,
                            "interrupt",
                            key.map(|key| key.id),
                            Some(&keys),
                        )
                        .await;
                    if !result
                        .as_ref()
                        .err()
                        .is_some_and(|error| tracked_retry.take(error))
                    {
                        return result;
                    }

                    // The caller owns the screen judgment. Re-plan once
                    // when its TRACKED value changed, but never carry an
                    // interrupt into another foreground run.
                    let again = self.get(&current.pane_id).await?;
                    if !super::core_action::same_core_run(&current, &again) {
                        return Err(
                            "identity_mismatch: interrupt target changed before retrying after tracked state changed"
                                .into(),
                        );
                    }
                    let (again, via) = self.interrupt_plan(&mut store, &again, request).await?;
                    return match via {
                        Via::Keys(keys) => {
                            self.core_durable_action(
                                &again,
                                "interrupt",
                                key.map(|key| key.id),
                                Some(&keys),
                            )
                            .await
                        }
                        // The API abort is deliberately local. It depends on
                        // the caller's answer channel and never crosses the
                        // coordinator action bridge.
                        Via::Abort => {
                            self.effect(&again, Effect::Interrupt, key, Some(request))
                                .await
                        }
                    };
                }
                Via::Abort => {
                    // Keep OpenCode's abort on the caller side.
                    return self
                        .effect(&current, Effect::Interrupt, key, Some(request))
                        .await;
                }
            }
        }
        if agent.process != "running" {
            return Err("identity_mismatch: agent is not verified in the foreground".into());
        }
        self.effect(agent, Effect::Interrupt, key, Some(request))
            .await
    }

    /// Whether and how this agent's turn can be interrupted now, judged on
    /// a fresh look at its screen; the agent as seen then.
    pub(super) async fn interrupt_plan(
        &self,
        store: &mut Store,
        agent: &Agent,
        request: &InterruptRequest,
    ) -> Result<(Agent, Via), String> {
        let current = self.get(&agent.pane_id).await?;
        if current.run != agent.run {
            return Err("identity_mismatch: interrupt target run changed".into());
        }
        let evidence = serde_json::to_value(&*current.evidence).map_err(|e| e.to_string())?;
        // A report's state is judged by the screen under it.
        let screen = if evidence["source"] == "run_report" {
            &evidence["screen"]
        } else {
            &evidence
        };
        // How first: without a way, nothing about the screen matters.
        let via = if request.keys.is_none() && self.abort_possible(&current) {
            Via::Abort
        } else {
            let keys = request
                .keys
                .clone()
                .unwrap_or_else(|| self.engine.interrupt_keys(&current.provider));
            if keys.is_empty() {
                let source = screen["manifest_source"].as_str().unwrap_or_default();
                return Err(match source.strip_prefix("override:") {
                    Some(path) => format!(
                        "interrupt_unverified: the detection override {path} names no interrupt_keys; nothing was sent (a person can pass --key)"
                    ),
                    None => format!(
                        "interrupt_unverified: masil has not measured how {} interrupts a turn; nothing was sent (a person can pass --key)",
                        current.provider
                    ),
                });
            }
            Via::Keys(keys)
        };
        if !request.any_state {
            let refused = |why: &str| {
                Err(format!(
                    "interrupt_not_working: {why}; nothing was sent (a person can pass --any-state)"
                ))
            };
            if screen["skip_state_update"] == true {
                return refused("the screen hides the turn (a transcript or another view)");
            }
            if current.state == "blocked"
                || screen["blocked_rule_matched"] == true
                || current.evidence.visible_blocker()
            {
                return refused("a request is waiting; the keys would answer it");
            }
            if screen["interruptible"] != true {
                return refused("the screen shows no turn that these keys interrupt");
            }
            if store.open_requests(&current.run)? > 0 {
                return refused("the agent waits on a request; the keys would answer it");
            }
        }
        Ok((current, via))
    }

    /// What followed a fresh interrupt key, watched for up to `seconds`:
    /// the receipt with `stopped`, and the record's stage moved when the
    /// evidence says what happened. Watching never sends anything.
    pub(crate) async fn confirm_interrupt(
        &self,
        agent: &Agent,
        mut receipt: Value,
        seconds: u64,
        asked_at: u64,
    ) -> Value {
        let stopped = |stage: &str| match stage {
            "provider_stopped" => "confirmed",
            "turn_end_observed" => "turn_ended",
            "completed_before_interrupt" => "completed_first",
            "provider_exited" => "provider_exited",
            _ => "not_checked",
        };
        let stage = receipt["stage"].as_str().unwrap_or_default().to_owned();
        if seconds == 0 || stage != "interrupt_key_delivered" {
            receipt["stopped"] = json!(stopped(&stage));
            return receipt;
        }
        let Some(key) = receipt["operation_key"].as_str().map(str::to_owned) else {
            receipt["stopped"] = json!("not_checked");
            return receipt;
        };
        // Only a delivery made just now: a retry's receipt is as recorded.
        let record = match self
            .operation_store()
            .await
            .and_then(|store| store.get(&key))
        {
            Ok(Some(record)) if now_ms().saturating_sub(record.updated_ms) <= 3_000 => record,
            _ => {
                receipt["stopped"] = json!("not_checked");
                return receipt;
            }
        };
        // From just before the request: a callback may land before the
        // receipt is written.
        let delivered = asked_at;
        // Only where the manifest cannot tell idle: elsewhere an unmatched
        // screen may be a dialog.
        let without_idle_rule = !self.engine.has_idle_rule(&agent.provider);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        // A working screen gone twice in a row: some providers' manifests
        // have no idle rule (the screen then reads unknown).
        let mut gone = 0;
        let verdict = loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            match self.get(&agent.pane_id).await {
                Ok(current) if current.run == agent.run => {
                    if current.state == "exited" {
                        break Some(("provider_exited", json!({"observed": "exited"})));
                    }
                    // The provider said so, after the key.
                    if current
                        .run_evidence
                        .as_ref()
                        .and_then(|evidence| evidence.interrupted_at)
                        .is_some_and(|at| at >= delivered)
                    {
                        break Some((
                            "provider_stopped",
                            json!({"observed": "provider_said_interrupted"}),
                        ));
                    }
                    let evidence = serde_json::to_value(&*current.evidence).unwrap_or_default();
                    if current.state == "idle" {
                        if evidence["source"] == "run_report" {
                            // Only a report written after the key counts.
                            if evidence["observed_at_ms"]
                                .as_u64()
                                .is_some_and(|at| at >= delivered)
                            {
                                let event = current
                                    .run_evidence
                                    .as_ref()
                                    .and_then(|evidence| evidence.report.as_ref())
                                    .and_then(|source| source.event.clone());
                                break Some(match event.as_deref() {
                                    Some("Stop") => {
                                        ("completed_before_interrupt", json!({"event": event}))
                                    }
                                    _ => ("turn_end_observed", json!({"event": event})),
                                });
                            }
                        } else if evidence["visible_idle"] == true {
                            break Some((
                                "turn_end_observed",
                                json!({"rule": evidence["matched_rule"]["id"]}),
                            ));
                        }
                    }
                    let screen = if evidence["source"] == "run_report" {
                        &evidence["screen"]
                    } else {
                        &evidence
                    };
                    gone = if without_idle_rule
                        && current.state != "blocked"
                        && current.state != "working"
                        && screen["interruptible"] != true
                    {
                        gone + 1
                    } else {
                        0
                    };
                    if gone >= 2 {
                        break Some((
                            "turn_end_observed",
                            json!({"observed": "working_screen_gone"}),
                        ));
                    }
                }
                // Its run is gone: the keys ended the agent.
                Ok(_) => break Some(("provider_exited", json!({"observed": "run_replaced"}))),
                Err(error) if super::failure::classify(&error).1 == "target_absent" => {
                    break Some(("provider_exited", json!({"observed": "target_absent"})));
                }
                Err(_) => {}
            }
            if std::time::Instant::now() >= deadline {
                break None;
            }
        };
        match verdict {
            Some((stage, evidence)) => {
                let moved = self.operation_store().await.and_then(|mut store| {
                    store.confirm_interrupt(&record, stage, &evidence, now_ms())
                });
                if matches!(moved, Ok(true)) {
                    receipt["stage"] = json!(stage);
                }
                receipt["stopped"] = json!(stopped(stage));
                receipt["evidence"] = evidence;
            }
            None => receipt["stopped"] = json!("unconfirmed"),
        }
        receipt
    }

    fn abort_possible(&self, agent: &Agent) -> bool {
        super::answer::requested(agent) && agent.session_id.is_some()
    }

    /// Close the agent's pane once per operation key.
    pub async fn close_with_operation(
        &self,
        agent: &Agent,
        key: Option<ClientKey<'_>>,
    ) -> Result<Value, String> {
        if self.action_path().await? == super::ActionPath::CoreLedger {
            if let Some(key) = key
                && key.pin != agent.run
            {
                return Err(
                    "identity_mismatch: close target run changed; this operation cannot be replayed"
                        .into(),
                );
            }
            let outcome = self
                .core_durable_action(agent, "close", key.map(|key| key.id), None)
                .await?;
            let stage = outcome["stage"].as_str().unwrap_or_default();
            if !matches!(
                stage,
                "not_applied" | "outcome_unknown" | "rejected_before_effect"
            ) {
                super::tokens::remove(&self.native.socket, &agent.run);
            }
            return Ok(outcome);
        }
        let outcome = self.effect(agent, Effect::Close, key, None).await?;
        let stage = outcome["stage"].as_str().unwrap_or_default();
        if !matches!(
            stage,
            "not_applied" | "outcome_unknown" | "rejected_before_effect"
        ) {
            super::tokens::remove(&self.native.socket, &agent.run);
        }
        Ok(outcome)
    }

    async fn core_durable_action(
        &self,
        agent: &Agent,
        action: &str,
        operation: Option<&str>,
        keys: Option<&[String]>,
    ) -> Result<Value, String> {
        let socket = self.native.socket.clone();
        let mut params = json!({
            "operation": action,
            "pane_id": agent.pane_id,
            "expected": super::core_action::expected(agent),
            "operation_id": operation,
        });
        if action == "interrupt"
            && let Some(keys) = keys
        {
            params["keys"] = json!(keys);
        }
        tokio::task::spawn_blocking(move || crate::coordinator::action(&socket, params))
            .await
            .map_err(|error| format!("coordinator_unavailable: {error}"))?
    }

    async fn effect(
        &self,
        agent: &Agent,
        effect: Effect,
        key: Option<ClientKey<'_>>,
        asked: Option<&InterruptRequest>,
    ) -> Result<Value, String> {
        if let Some(key) = key {
            if !operations::valid_id(key.id) {
                return Err(
                    "invalid_argument: operation ID must be 1–64 letters, digits, '.', '_', ':' or '-'".into(),
                );
            }
            if key.pin != agent.run {
                return Err(format!(
                    "identity_mismatch: {} target run changed; this operation cannot be replayed",
                    effect.action()
                ));
            }
        }
        let _lock = self.lock_within(std::time::Duration::from_secs(2)).await?;
        let (raw, fence, recorded) = self.effect_values(&agent.pane_id).await?;
        let mut store = self.operation_store_recorded(&recorded).await?;
        let (id, explicit) = match key {
            Some(key) => (key.id.to_owned(), true),
            None => (format!("auto-{}", nonce()?), false),
        };
        let namespace = format!("run:{}", agent.run);
        let digest = sha256(effect.action());
        let mut reconciled = false;
        let (mut raw, mut fence) = (raw, fence);
        loop {
            if reconciled {
                (raw, fence, _) = self.effect_values(&agent.pane_id).await?;
            }
            let request = NewOperation {
                namespace: &namespace,
                action: effect.action(),
                id: &id,
                explicit,
                target: &agent.pane_id,
                run: Some(&agent.run),
                boot: &agent.boot,
                digest: &digest,
                payload_bytes: 0,
            };
            let intent = json!({"effects_sha256": sha256(&raw), "fence": fence});
            let ticket = match store.admit(&request, intent, now_ms())? {
                Admission::Dispatch(ticket) => ticket,
                Admission::Recorded(record) => return Ok(effect_receipt(&record)),
                Admission::NeedsReconcile(record) if !reconciled => {
                    self.reconcile_one(&mut store, &record).await?;
                    reconciled = true;
                    continue;
                }
                Admission::NeedsReconcile(_) => {
                    return Err(
                        "outcome_unknown: operation is still unresolved; see `agent operations`"
                            .into(),
                    );
                }
            };
            // Judged only when it would be sent: a retry of a recorded key
            // gets its receipt above whatever the agent does now.
            let (agent, via) = match asked {
                Some(asked) => match self.interrupt_plan(&mut store, agent, asked).await {
                    Ok((current, via)) => (current, Some(via)),
                    Err(error) => {
                        let _ = store.finish(
                            &ticket,
                            "rejected_before_effect",
                            "guard",
                            Some(&json!({"reason": error})),
                            now_ms(),
                        );
                        return Err(error);
                    }
                },
                None => (agent.clone(), None),
            };
            let keys = match via {
                Some(Via::Keys(keys)) => keys,
                Some(Via::Abort) => {
                    if let Some(outcome) = self
                        .abort_effect(&mut store, &agent, &ticket, (&raw, &fence))
                        .await
                    {
                        return outcome;
                    }
                    // No channel after all: the measured keys.
                    let keys = self.engine.interrupt_keys(&agent.provider);
                    if keys.is_empty() {
                        let _ = store.finish(
                            &ticket,
                            "rejected_before_effect",
                            "guard",
                            None,
                            now_ms(),
                        );
                        return Err(
                            "interrupt_unverified: no answer channel and no measured keys".into(),
                        );
                    }
                    keys
                }
                None => vec!["C-c".to_owned()],
            };
            return self
                .dispatch_effect(&mut store, &agent, effect, &ticket, (&raw, &fence), &keys)
                .await;
        }
    }

    /// OpenCode's own abort for an interrupt; its answer is the evidence.
    /// None when the answer channel cannot be reached.
    async fn abort_effect(
        &self,
        store: &mut Store,
        agent: &Agent,
        ticket: &Ticket,
        (raw, fence): (&str, &str),
    ) -> Option<Result<Value, String>> {
        let (endpoint, session) = self.abort_channel(agent).await?;
        // Its ticket goes into the effects ledger before the request: a
        // process that dies mid-request leaves a pending entry, which
        // reconcile reads as unknown, never as not applied.
        let mut effects = Self::effects_for(&agent.run, raw);
        if effects.entries.len() == MAX_EFFECTS {
            effects.entries.remove(0);
        }
        effects.entries.push(EffectEntry {
            key: ticket.key.clone(),
            ticket: ticket.ticket.clone(),
            stage: "pending".into(),
        });
        let pending = match encode(&effects) {
            Ok(pending) => pending,
            Err(error) => return Some(Err(error)),
        };
        match self
            .compare_and_set(
                &agent.pane_id,
                &[(EFFECTS, raw), (EFFECTS_FENCE, fence)],
                &[(EFFECTS, pending)],
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                let _ = store.finish(ticket, "rejected_before_effect", "guard", None, now_ms());
                return Some(Err(
                    "rejected_before_effect: interrupt was not applied: operation effects changed first".into(),
                ));
            }
            Err(error) => return Some(Err(error)),
        }
        let answered = endpoint.abort(&session).await;
        let now = now_ms();
        let (stage, evidence) = match &answered {
            Ok(()) => ("provider_stopped", json!({"via": "opencode_abort"})),
            Err(error) if error.starts_with("not_applied") => (
                "not_applied",
                json!({"via": "opencode_abort", "error": error}),
            ),
            Err(error) => (
                "outcome_unknown",
                json!({"via": "opencode_abort", "error": error}),
            ),
        };
        let recorded = store
            .finish(ticket, stage, "opencode_abort", Some(&evidence), now)
            .map(|record| effect_receipt(&record))
            .map_err(|error| {
                String::from(AfterEffect::new(format!(
                    "interrupt receipt could not be committed: {error}; query `agent operation {}`",
                    ticket.key
                )))
            });
        Some(match answered {
            Ok(()) => recorded,
            Err(error) => recorded.and(Err(error)),
        })
    }

    /// The effects option, its fence and the server's recorded store instance.
    async fn effect_values(&self, pane: &str) -> Result<(String, String, String), String> {
        let values = self
            .pane_values(pane, &[EFFECTS, EFFECTS_FENCE, super::STORE_OPTION])
            .await?;
        let [raw, fence, store] =
            <[String; 3]>::try_from(values).map_err(|_| "invalid operation effects")?;
        Ok((raw, fence, store))
    }

    fn effects_for(agent_run: &str, raw: &str) -> Effects {
        decode::<Effects>(raw)
            .filter(|effects| effects.run == agent_run)
            .unwrap_or_else(|| Effects {
                run: agent_run.into(),
                entries: Vec::new(),
            })
    }

    async fn dispatch_effect(
        &self,
        store: &mut Store,
        agent: &Agent,
        effect: Effect,
        ticket: &Ticket,
        (raw, fence): (&str, &str),
        keys: &[String],
    ) -> Result<Value, String> {
        let mut effects = Self::effects_for(&agent.run, raw);
        if effects.entries.len() == MAX_EFFECTS {
            effects.entries.remove(0);
        }
        effects.entries.push(EffectEntry {
            key: ticket.key.clone(),
            ticket: ticket.ticket.clone(),
            stage: "pending".into(),
        });
        let pending = encode(&effects)?;
        let mut commands = vec![Self::option(agent, EFFECTS, pending.clone())];
        let send = |key: &str| {
            vec![
                "send-keys".to_owned(),
                "-t".to_owned(),
                agent.pane_id.clone(),
                "--".to_owned(),
                key.to_owned(),
            ]
        };
        let (first, later) = keys.split_first().ok_or("no interrupt key")?;
        match effect {
            Effect::Interrupt => {
                commands.push(send(first));
                // With more keys to come the entry stays pending.
                if later.is_empty() {
                    effects
                        .entries
                        .last_mut()
                        .ok_or("missing effect entry")?
                        .stage = "delivered".into();
                    commands.push(Self::option(agent, EFFECTS, encode(&effects)?));
                }
            }
            // The pane and its options disappear with the kill.
            Effect::Close => {
                commands.push(vec!["kill-pane".into(), "-t".into(), agent.pane_id.clone()]);
            }
        }
        let cas = [
            format!("#{{==:#{{{EFFECTS}}},{raw}}}"),
            format!("#{{==:#{{{EFFECTS_FENCE}}},{fence}}}"),
        ];
        let result = match effect {
            Effect::Interrupt => {
                self.guarded_input_outcome(agent, commands, Some(&and(&cas)))
                    .await
            }
            Effect::Close => {
                let mut guards = vec![identity_guard(agent)];
                guards.extend(cas);
                self.native
                    .guarded_group(
                        &agent.pane_id,
                        &and(&guards),
                        &commands,
                        "masil-agent-stale",
                    )
                    .await
                    .map_err(super::server_unreachable)
                    .map(|output| {
                        if String::from_utf8_lossy(&output.stdout)
                            .lines()
                            .any(|line| line == "masil-agent-stale")
                        {
                            Guarded::Rejected
                        } else {
                            Guarded::Applied
                        }
                    })
            }
        };
        // The keys after the first, each in its own group a moment later
        // (a TUI reads two at once as one Alt key): applied while the
        // pending entry is still the one the first group wrote.
        let result = match result {
            Ok(Guarded::Applied) if effect == Effect::Interrupt && !later.is_empty() => {
                let mut outcome = Ok(Guarded::Applied);
                let mut current = pending.clone();
                for (index, key) in later.iter().enumerate() {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    let mut commands = vec![send(key)];
                    if index + 1 == later.len() {
                        effects
                            .entries
                            .last_mut()
                            .ok_or("missing effect entry")?
                            .stage = "delivered".into();
                        commands.push(Self::option(agent, EFFECTS, encode(&effects)?));
                    }
                    let cas = [
                        format!("#{{==:#{{{EFFECTS}}},{current}}}"),
                        format!("#{{==:#{{{EFFECTS_FENCE}}},{fence}}}"),
                    ];
                    match self
                        .guarded_followup_input(agent, commands, &and(&cas))
                        .await
                    {
                        Ok(Guarded::Applied) => current = encode(&effects)?,
                        Ok(Guarded::Rejected) => {
                            let _ = store.finish(
                                ticket,
                                "interrupt_partial",
                                "guard",
                                Some(&json!({"keys_sent": index + 1, "keys": keys})),
                                now_ms(),
                            );
                            return Err(format!(
                                "interrupt_partial: {} of {} keys reached the pane before it changed; look at the agent",
                                index + 1,
                                keys.len()
                            ));
                        }
                        Err(error) => {
                            outcome = Err(error);
                            break;
                        }
                    }
                }
                outcome
            }
            other => other,
        };
        let now = now_ms();
        match result {
            Ok(Guarded::Applied) => finish_effect(store, ticket, effect, now).map_err(String::from),
            Ok(Guarded::Rejected) => {
                let _ = store.finish(ticket, "rejected_before_effect", "guard", None, now);
                Err(format!(
                    "rejected_before_effect: {} was not applied: agent run or operation effects changed first",
                    effect.action()
                ))
            }
            Err(error) => {
                // A failed settle read still records the attempt as unknown.
                let (state, mut evidence) = self
                    .settle_effect(agent, effect, &ticket.ticket, &sha256(raw), fence)
                    .await
                    .unwrap_or_else(|settle| {
                        (
                            "outcome_unknown",
                            json!({"settle_error": settle.to_string()}),
                        )
                    });
                evidence["error"] = json!(error);
                store
                    .finish(ticket, state, "reconcile", Some(&evidence), now_ms())
                    .map_err(|finish| {
                        String::from(AfterEffect::new(format!(
                            "{} outcome receipt could not be committed: {finish}; query `agent operation {}`",
                            effect.action(),
                            ticket.key
                        )))
                    })?;
                Err(String::from(AfterEffect::new(format!(
                    "{} outcome is {state}: {error}; query `agent operation {}` before retrying",
                    effect.action(),
                    ticket.key
                ))))
            }
        }
    }

    /// Judge an attempt from the pane: its ticket in the effects option, or a
    /// fence that makes the attempt unable to apply later.
    async fn settle_effect(
        &self,
        agent: &Agent,
        effect: Effect,
        ticket: &str,
        effects_sha256: &str,
        fence: &str,
    ) -> Result<(&'static str, Value), AfterEffect> {
        for _ in 0..3 {
            if effect == Effect::Close {
                // Pane IDs are never reused: a missing pane was closed (by this
                // attempt or by anything else); a present one was not killed.
                match self.pane_exists(&agent.pane_id).await {
                    Ok(false) => return Ok(("target_absent", json!({"reason": "pane_gone"}))),
                    Ok(true) => {}
                    Err(error) => return Err(AfterEffect::new(error)),
                }
            }
            let current = match self.get(&agent.pane_id).await {
                Ok(current) if current.run == agent.run => current,
                // The kill is guarded by the run identity, which has changed.
                Ok(_) if effect == Effect::Close => {
                    return Ok(("not_applied", json!({"reason": "run_replaced"})));
                }
                Err(error)
                    if effect == Effect::Close
                        && super::failure::classify(&error).1 == "target_absent" =>
                {
                    return Ok(("not_applied", json!({"reason": "run_replaced"})));
                }
                _ => return Ok(("outcome_unknown", json!({"reason": "run_not_live"}))),
            };
            let (raw, current_fence, _) = self
                .effect_values(&current.pane_id)
                .await
                .map_err(AfterEffect::new)?;
            let effects = Self::effects_for(&agent.run, &raw);
            if let Some(entry) = effects.entries.iter().find(|e| e.ticket == ticket) {
                return Ok(match (effect, entry.stage.as_str()) {
                    (Effect::Interrupt, "delivered") => {
                        (effect.done(), json!({"ledger_stage": "delivered"}))
                    }
                    // The kill runs in the same group as the entry; the pane is alive.
                    (Effect::Close, _) => ("not_applied", json!({"ledger_stage": entry.stage})),
                    _ => ("outcome_unknown", json!({"ledger_stage": entry.stage})),
                });
            }
            if sha256(&raw) != effects_sha256 || current_fence != fence {
                // Entries are only appended. Below the cap nothing was rotated
                // out, so a missing ticket never ran; at the cap it may have.
                return Ok(if effects.entries.len() >= MAX_EFFECTS {
                    ("outcome_unknown", json!({"reason": "effects_rotated"}))
                } else {
                    ("not_applied", json!({"reason": "effects_moved"}))
                });
            }
            if self
                .compare_and_set(
                    &current.pane_id,
                    &[(EFFECTS, &raw), (EFFECTS_FENCE, &current_fence)],
                    &[(EFFECTS_FENCE, nonce().map_err(AfterEffect::new)?)],
                )
                .await
                .map_err(AfterEffect::new)?
            {
                return Ok(("not_applied", json!({"reason": "fenced"})));
            }
        }
        Ok((
            "outcome_unknown",
            json!({"reason": "effects_changed_during_fence"}),
        ))
    }

    async fn pane_exists(&self, pane: &str) -> Result<bool, String> {
        let panes = self
            .command(&["list-panes", "-a", "-F", "#{pane_id}"])
            .await?;
        Ok(panes.lines().any(|line| line == pane))
    }

    async fn settle_start_after_effect(&self, run: &str) -> Result<&'static str, AfterEffect> {
        let inventory = self.inventory().await.map_err(AfterEffect::new)?;
        super::validate_inventory(&inventory).map_err(AfterEffect::new)?;
        let Some(fields) = inventory.iter().find(|fields| {
            decode::<super::Metadata>(&fields[11]).is_some_and(|meta| meta.run == run)
        }) else {
            return Ok("outcome_unknown");
        };
        if fields[4] != "1" {
            return Ok("process_started");
        }
        // A child that refused its directory kept the pane to say so.
        let verdict = self
            .command(&[
                "show-options",
                "-pqv",
                "-t",
                &fields[0],
                super::LAUNCH_VERDICT,
            ])
            .await
            .unwrap_or_default();
        if verdict.trim().starts_with(&format!("{run} cwd_rejected")) {
            // Its child kept the pane only to report; the agent never ran.
            // Gone, the name is free for a retry.
            self.remove_dead_pane(&fields[0])
                .await
                .map_err(AfterEffect::new)?;
            return Ok("cwd_rejected");
        }
        Ok("process_exited")
    }

    /// Start with an optional client key pinned to the server boot.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub async fn start_with_operation(
        &self,
        name: &str,
        provider: &str,
        cwd: &std::path::Path,
        args: &[String],
        session: Option<&str>,
        split: Option<&str>,
        key: Option<ClientKey<'_>>,
        answers: bool,
    ) -> Result<Value, String> {
        if answers {
            // docs/agent-answers.md: only OpenCode's TUI can take the
            // listening options safely.
            if crate::providers::find(provider).map(|provider| provider.id) != Some("opencode") {
                return Err("invalid_argument: --answers is for OpenCode only".into());
            }
            super::answer::check_start_args(args)?;
        }
        let _lock = self.lock()?;
        let identity = self
            .command(&[
                "display-message",
                "-p",
                &format!("#{{masil_core_boot_id}}\t#{{{}}}", super::STORE_OPTION),
            ])
            .await?;
        let (boot, recorded) = identity
            .trim_end_matches('\n')
            .split_once('\t')
            .ok_or("server_unreachable: invalid native server identity")?;
        let boot = super::valid_boot(boot)?.to_owned();
        if let Some(key) = key {
            if !operations::valid_id(key.id) {
                return Err(
                    "invalid_argument: operation ID must be 1–64 letters, digits, '.', '_', ':' or '-'".into(),
                );
            }
            if key.pin != boot {
                return Err(
                    "identity_mismatch: server restarted since this launch was requested; it cannot be replayed"
                        .into(),
                );
            }
        }
        let mut store = self.operation_store_recorded(recorded).await?;
        let (id, explicit) = match key {
            Some(key) => (key.id.to_owned(), true),
            None => (format!("auto-{}", nonce()?), false),
        };
        let namespace = format!("boot:{boot}");
        let provider_id = crate::providers::find(provider)
            .ok_or("unknown_provider: unknown agent provider")?
            .id;
        let canonical = cwd
            .canonicalize()
            .map_err(|e| format!("working directory: {e}"))?;
        let mut digest = launch_digest(name, provider_id, &canonical, args, session, split);
        if answers {
            digest = super::prompt::sha256(&format!("{digest}\nanswers"));
        }
        // A retry of a finished launch answers from its record: the agent it
        // started now owns the name, so launch checks would reject it.
        // An expired attempt is resolved first for the same reason.
        if let Some(mut record) = store.get(&operations::key(&namespace, "start", &id))? {
            if record.digest != digest {
                return Err("operation was already used with different content".into());
            }
            if record.state == operations::DISPATCHING {
                if record.lease_until_ms > now_ms() {
                    return Err(format!(
                        "outcome_unknown: operation {} is in progress; query its receipt before retrying",
                        record.operation_key
                    ));
                }
                record = self.reconcile_one(&mut store, &record).await?;
            }
            if !matches!(
                record.state.as_str(),
                "rejected_before_effect" | "not_applied" | "cwd_rejected"
            ) {
                return Ok(start_receipt(&record));
            }
        }
        let mut prepared = self
            .prepare_launch(name, provider, cwd, args, session, split)
            .await?;
        prepared.answers = answers;
        let mut reconciled = false;
        let ticket = loop {
            let request = NewOperation {
                namespace: &namespace,
                action: "start",
                id: &id,
                explicit,
                target: name,
                run: None,
                boot: &boot,
                digest: &digest,
                payload_bytes: 0,
            };
            match store.admit(&request, json!({}), now_ms())? {
                Admission::Dispatch(ticket) => break ticket,
                Admission::Recorded(record) => return Ok(start_receipt(&record)),
                Admission::NeedsReconcile(record) if !reconciled => {
                    self.reconcile_one(&mut store, &record).await?;
                    reconciled = true;
                }
                Admission::NeedsReconcile(_) => {
                    return Err(
                        "outcome_unknown: launch is still unresolved; see `agent operations`"
                            .into(),
                    );
                }
            }
        };
        // A start in a masil worktree leases it before anything launches,
        // so a removal cannot begin in between (docs/worktrees.md).
        let lease = match &prepared.worktree {
            None => None,
            Some(found) => {
                match crate::worktree::begin(found, &self.native.socket, &boot, &ticket.ticket) {
                    Ok(lease) => Some(lease),
                    Err(error) => {
                        let _ = store.finish(
                            &ticket,
                            "rejected_before_effect",
                            "worktree",
                            Some(&json!({"error": error})),
                            now_ms(),
                        );
                        return Err(format!(
                            "{error} (operation {} is rejected_before_effect)",
                            ticket.key
                        ));
                    }
                }
            }
        };
        // The ticket is the new run's identity, so reconcile can find the pane.
        let launched = self
            .launch(prepared, name, &ticket.ticket, session, split)
            .await;
        if let Some(lease) = &lease {
            match &launched {
                Ok(outcome) => {
                    if let Some(pane) = outcome["pane_id"].as_str() {
                        crate::worktree::launched(lease, pane);
                    }
                }
                Err(error)
                    if error.starts_with("cwd_rejected")
                        || (transport_rejected(error) && !error.contains("was launched")) =>
                {
                    crate::worktree::abandon(lease);
                }
                // Unknown: lease cleanup finds out from the server.
                Err(_) => {}
            }
        }
        match launched {
            Ok(mut outcome) => {
                outcome["operation_key"] = json!(ticket.key);
                if let Some(lease) = &lease {
                    outcome["worktree"] = json!({"id": lease.worktree, "name": lease.name});
                }
                store
                    .finish(&ticket, "process_started", "registration", Some(&outcome), now_ms())
                    .map_err(|error| {
                        String::from(AfterEffect::new(format!(
                            "agent started, but its durable receipt could not be committed: {error}; query `agent operation {}`",
                            ticket.key
                        )))
                    })?;
                Ok(outcome)
            }
            // The child checked its directory and did not start the agent.
            Err(error) if error.starts_with("cwd_rejected") => {
                let _ = store.finish(
                    &ticket,
                    "cwd_rejected",
                    "exec_managed",
                    Some(&json!({"error": error})),
                    now_ms(),
                );
                Err(format!(
                    "{error} (operation {} is cwd_rejected)",
                    ticket.key
                ))
            }
            Err(error) => {
                if transport_rejected(&error) && !error.contains("was launched") {
                    let _ = store.finish(
                        &ticket,
                        "rejected_before_effect",
                        "launch",
                        Some(&json!({"error": error})),
                        now_ms(),
                    );
                    return Err(format!(
                        "rejected_before_effect: {error} (operation {} is rejected_before_effect)",
                        ticket.key
                    ));
                }
                let state = self
                    .settle_start_after_effect(&ticket.ticket)
                    .await
                    .unwrap_or("outcome_unknown");
                store
                    .finish(
                        &ticket,
                        state,
                        "launch",
                        Some(&json!({"error": error})),
                        now_ms(),
                    )
                    .map_err(|finish| {
                        String::from(AfterEffect::new(format!(
                            "launch outcome receipt could not be committed: {finish}; query `agent operation {}`",
                            ticket.key
                        )))
                    })?;
                Err(String::from(AfterEffect::new(format!(
                    "{error} (operation {} is {state})",
                    ticket.key
                ))))
            }
        }
    }

    /// A launch is known started only when a pane carries its run.
    async fn settle_start(&self, run: &str) -> Result<&'static str, String> {
        self.settle_start_after_effect(run)
            .await
            .map_err(String::from)
    }

    /// Resolve one attempt whose lease expired. Callers hold the management lock.
    async fn reconcile_one(&self, store: &mut Store, record: &Record) -> Result<Record, String> {
        let now = now_ms();
        match record.action.as_str() {
            "prompt" => self.reconcile_prompt(store, record).await,
            "start" => {
                let state = self.settle_start(&record.ticket).await?;
                store.resolve(record, state, "reconcile", None, now)
            }
            action @ ("interrupt" | "close") => {
                // A core-ledger effect is settled only by its retained core
                // result. The native effects option says nothing about it:
                // treating a delivered core interrupt as absent would make a
                // later retry send the same operation key again.
                if record
                    .intent()
                    .is_some_and(|intent| intent["path"] == "core_ledger")
                {
                    let evidence = json!({
                        "path": "core_ledger",
                        "reconciled": "awaiting_coordinator_ledger",
                    });
                    return store.resolve(
                        record,
                        operations::UNKNOWN,
                        "core_ledger_reconcile",
                        Some(&evidence),
                        now,
                    );
                }
                let effect = if action == "interrupt" {
                    Effect::Interrupt
                } else {
                    Effect::Close
                };
                let agent = match self.get(&record.target).await {
                    Ok(agent) if Some(&agent.run) == record.run.as_ref() => agent,
                    _ => {
                        let state = match (effect, self.pane_exists(&record.target).await) {
                            (Effect::Close, Ok(false)) => "target_absent",
                            (Effect::Close, Ok(true)) => "not_applied",
                            _ => "outcome_unknown",
                        };
                        return store.resolve(
                            record,
                            state,
                            "reconcile",
                            Some(&json!({"reason": "run_not_live"})),
                            now,
                        );
                    }
                };
                let intent = record.intent().cloned().unwrap_or_default();
                let (state, evidence) = self
                    .settle_effect(
                        &agent,
                        effect,
                        &record.ticket,
                        intent["effects_sha256"].as_str().unwrap_or_default(),
                        intent["fence"].as_str().unwrap_or_default(),
                    )
                    .await?;
                store.resolve(record, state, "reconcile", Some(&evidence), now)
            }
            _ => store.resolve(record, "outcome_unknown", "reconcile", None, now),
        }
    }

    /// `agent operations …` and `agent operation …`.
    pub(super) async fn operations_command(
        &self,
        single: bool,
        args: &[String],
    ) -> Result<Value, String> {
        if !single && args == ["adopt"] {
            // Accept the current store after the user inspected the agents.
            let store = Store::open(&self.native.socket)?;
            let instance = store.instance()?.to_string();
            self.command(&["set-option", "-g", super::STORE_OPTION, &instance])
                .await?;
            return Ok(json!({"stage": "adopted", "instance": instance}));
        }
        let mut store = self.operation_store().await?;
        if single {
            return match args {
                [key] => store
                    .get(key)?
                    .map(|record| json!(record))
                    .ok_or_else(|| "target_absent: operation not found; it never existed or its namespace ended and it was deleted".into()),
                [command, key, flag, verdict] if command == "resolve" && flag == "--as" => {
                    let delivered = match verdict.as_str() {
                        "delivered" => true,
                        "not-delivered" => false,
                        _ => return Err("usage: resolve accepts --as delivered|not-delivered".into()),
                    };
                    let record = store.get(key)?.ok_or("target_absent: operation not found")?;
                    if record.state != operations::UNKNOWN {
                        return Err(format!(
                            "only an outcome_unknown operation can be resolved; this one is {}",
                            record.state
                        ));
                    }
                    let record = if record.action == "prompt" {
                        self.resolve_prompt(&mut store, &record, delivered).await?
                    } else if record.action == "answer" && !delivered {
                        // The same request can then be answered again.
                        store.settle_unknown(&record, "not_applied", "user", None, now_ms())?
                    } else {
                        let state = if delivered {
                            "user_confirmed_delivered"
                        } else {
                            "user_confirmed_not_delivered"
                        };
                        store.settle_unknown(&record, state, "user", None, now_ms())?
                    };
                    Ok(json!(record))
                }
                _ => Err("usage: operation KEY | operation resolve KEY --as delivered|not-delivered".into()),
            };
        }
        match args {
            [command] if command == "status" => {
                let mut status = store.status()?;
                status["unresolved"] = json!(store.list(true, 4096)?.len());
                status["expired"] = json!(store.expired(now_ms(), 4096)?.len());
                Ok(status)
            }
            [command] if command == "reconcile" => {
                let _lock = self.lock()?;
                let mut results = Vec::new();
                for record in store.expired(now_ms(), RECONCILE_BATCH)? {
                    let result = self.reconcile_one(&mut store, &record).await;
                    results.push(match result {
                        Ok(record) => {
                            json!({"operation_key": record.operation_key, "state": record.state})
                        }
                        Err(error) => {
                            json!({"operation_key": record.operation_key, "error": error})
                        }
                    });
                }
                self.maintain(&mut store).await?;
                Ok(json!({"stage": "reconciled", "operations": results}))
            }

            _ => {
                let mut all = false;
                let mut limit = 20usize;
                let mut rest = args;
                if rest.first().is_some_and(|word| word == "list") {
                    rest = &rest[1..];
                }
                let mut i = 0;
                while i < rest.len() {
                    match rest[i].as_str() {
                        "--all" if !all => {
                            all = true;
                            i += 1;
                        }
                        "--limit" => {
                            limit = rest
                                .get(i + 1)
                                .and_then(|value| value.parse().ok())
                                .filter(|value| (1..=512).contains(value))
                                .ok_or("invalid_argument: --limit must be 1–512")?;
                            i += 2;
                        }
                        _ => return Err("usage: operations [list] [--all] [--limit N] | status | reconcile | adopt".into()),
                    }
                }
                Ok(json!({"operations": store.list(!all, limit)?, "unresolved_only": !all}))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_reported_tmux_failures_prove_nothing_ran() {
        assert!(transport_rejected(
            "native command failed: create window failed"
        ));
        assert!(transport_rejected(
            "native command exited with exit status: 1"
        ));
        assert!(!transport_rejected("native command timed out"));
        assert!(!transport_rejected(
            "native command exited with signal: 9 (SIGKILL)"
        ));
        assert!(!transport_rejected("reading native command: broken pipe"));
    }

    #[test]
    fn launch_digests_bind_every_parameter() {
        let cwd = std::path::Path::new("/tmp/project");
        let base = launch_digest("a", "codex", cwd, &[], None, None);
        assert_eq!(base, launch_digest("a", "codex", cwd, &[], None, None));
        assert_ne!(base, launch_digest("b", "codex", cwd, &[], None, None));
        assert_ne!(
            base,
            launch_digest("a", "codex", cwd, &["-x".into()], None, None)
        );
        assert_ne!(base, launch_digest("a", "codex", cwd, &[], Some("s"), None));
        assert_ne!(
            base,
            launch_digest("a", "codex", cwd, &[], None, Some("%1"))
        );
    }

    #[test]
    fn effects_entries_fit_the_option_limit() {
        let effects = Effects {
            run: "r".repeat(32),
            entries: (0..MAX_EFFECTS)
                .map(|_| EffectEntry {
                    key: format!("run:{}/interrupt/{}", "r".repeat(32), "k".repeat(64)),
                    ticket: "t".repeat(32),
                    stage: "delivered".into(),
                })
                .collect(),
        };
        assert!(decode::<Effects>(&encode(&effects).unwrap()).is_some());
    }
}
