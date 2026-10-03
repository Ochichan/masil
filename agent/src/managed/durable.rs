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
    json!({"stage":record.state,"pane_id":record.target,"run":record.run,
        "operation_key":record.operation_key,"provider_accepted":false})
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
                decode::<super::Metadata>(&fields[11])
                    .map(|meta| meta.run)
                    .into_iter()
                    .chain(decode::<super::Tracked>(&fields[12]).map(|tracked| tracked.run))
                    .chain([synthetic])
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
        if agent.process != "running" {
            return Err("identity_mismatch: agent is not verified in the foreground".into());
        }
        self.effect(agent, Effect::Interrupt, key).await
    }

    /// Close the agent's pane once per operation key.
    pub async fn close_with_operation(
        &self,
        agent: &Agent,
        key: Option<ClientKey<'_>>,
    ) -> Result<Value, String> {
        let outcome = self.effect(agent, Effect::Close, key).await?;
        let stage = outcome["stage"].as_str().unwrap_or_default();
        if !matches!(
            stage,
            "not_applied" | "outcome_unknown" | "rejected_before_effect"
        ) {
            super::tokens::remove(&self.native.socket, &agent.run);
        }
        Ok(outcome)
    }

    async fn effect(
        &self,
        agent: &Agent,
        effect: Effect,
        key: Option<ClientKey<'_>>,
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
            return self
                .dispatch_effect(&mut store, agent, effect, &ticket, &raw, &fence)
                .await;
        }
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
        raw: &str,
        fence: &str,
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
        let mut commands = vec![Self::option(agent, EFFECTS, pending)];
        match effect {
            Effect::Interrupt => {
                effects
                    .entries
                    .last_mut()
                    .ok_or("missing effect entry")?
                    .stage = "delivered".into();
                commands.push(vec![
                    "send-keys".into(),
                    "-t".into(),
                    agent.pane_id.clone(),
                    "--".into(),
                    "C-c".into(),
                ]);
                commands.push(Self::option(agent, EFFECTS, encode(&effects)?));
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
