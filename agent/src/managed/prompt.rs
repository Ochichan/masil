//! Guarded, explicit prompt delivery with bounded per-run retry receipts.
//!
//! The pane ledger records the effect: the guarded group writes `pending`
//! before the paste and `delivered` after Enter. The group applies only if
//! the ledger and fence still hold the values read before admission, so a
//! late copy of the group cannot type the prompt again. The durable store
//! records admission, dispatch intent and the confirmed stage. The ledger's
//! JSON is unchanged from earlier releases; tickets and the fence live in
//! separate options so older binaries still read it.
use super::operations::{self, Admission, NewOperation, Record, Store, Ticket};
use super::{Agent, Guarded, Manager, and, decode, encode, failure::AfterEffect, nonce};
use crate::observation::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::ffi::OsString;

const OPTION: &str = "@masil-agent-prompt-receipts";
const TICKETS: &str = "@masil-agent-prompt-tickets";
const FENCE: &str = "@masil-agent-prompt-fence";
const MAX_RETAINED: usize = 16;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    run: String,
    sequence: u64,
    entries: Vec<Receipt>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    operation: u64,
    digest: String,
    stage: String,
}

/// Which durable attempt wrote each ledger slot.
#[derive(Default, Serialize, Deserialize)]
struct Tickets {
    run: String,
    entries: Vec<TicketEntry>,
}

#[derive(Serialize, Deserialize)]
struct TicketEntry {
    operation: u64,
    key: String,
    ticket: String,
}

/// Raw option values as the guard format sees them, plus their decoded form.
struct PaneLedger {
    ledger: Ledger,
    tickets: Tickets,
    raw: String,
    fence: String,
    /// The server's recorded operation store instance, read in the same query.
    store: String,
}

impl PaneLedger {
    fn ticket_slot(&self, ticket: &str) -> Option<&Receipt> {
        let operation = self
            .tickets
            .entries
            .iter()
            .find(|entry| entry.ticket == ticket)?
            .operation;
        self.ledger
            .entries
            .iter()
            .find(|e| e.operation == operation)
    }

    fn key(&self, operation: u64) -> String {
        self.tickets
            .entries
            .iter()
            .find(|entry| entry.operation == operation)
            .map(|entry| entry.key.clone())
            .unwrap_or_else(|| explicit_key(&self.ledger.run, operation))
    }

    fn public(&self, entry: &Receipt) -> Value {
        json!({"operation":entry.operation,"run":self.ledger.run,"stage":entry.stage,"provider_accepted":false,
            "task_success":null,"operation_key":self.key(entry.operation)})
    }
}

fn explicit_key(run: &str, operation: u64) -> String {
    operations::key(&format!("run:{run}"), "prompt", &operation.to_string())
}

fn sha256(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// A durable record for a slot the pane ledger no longer holds.
fn public_record(run: &str, operation: u64, record: &Record) -> Value {
    let stage = match record.state.as_str() {
        "delivered" | "user_confirmed_delivered" => "delivered",
        "user_confirmed_not_delivered" => "not_delivered",
        _ => "pending",
    };
    json!({"operation":operation,"run":run,"stage":stage,"provider_accepted":false,"task_success":null,
        "operation_key":record.operation_key})
}

impl Manager {
    pub async fn prompt(&self, agent: &Agent, text: &str) -> Result<Value, String> {
        self.prompt_with_operation(agent, text, None).await
    }

    pub async fn prompt_receipt(
        &self,
        agent: &Agent,
        operation: Option<u64>,
    ) -> Result<Value, String> {
        let pane = self.prompt_ledger(agent).await?;
        let operation = operation.unwrap_or(pane.ledger.sequence);
        if let Some(entry) = pane
            .ledger
            .entries
            .iter()
            .find(|e| e.operation == operation)
        {
            return Ok(pane.public(entry));
        }
        self.retained_prompt(agent, operation).await
    }

    /// Durable receipts outlive the 16 pane-ledger entries for explicit keys.
    async fn retained_prompt(&self, agent: &Agent, operation: u64) -> Result<Value, String> {
        let expired =
            "prompt receipt unavailable or expired; delivery must not be retried automatically";
        if operation == 0 {
            return Err(expired.into());
        }
        let store = self.operation_store().await.map_err(|_| expired)?;
        let record = store
            .get(&explicit_key(&agent.run, operation))
            .ok()
            .flatten()
            .filter(|record| record.state != operations::DISPATCHING)
            .ok_or(expired)?;
        Ok(public_record(&agent.run, operation, &record))
    }

    async fn prompt_ledger(&self, agent: &Agent) -> Result<PaneLedger, String> {
        let values = self
            .pane_values(
                &agent.pane_id,
                &[OPTION, TICKETS, FENCE, super::STORE_OPTION],
            )
            .await?;
        let [raw, tickets, fence, store] =
            <[String; 4]>::try_from(values).map_err(|_| "invalid prompt receipt options")?;
        let fresh = || Ledger {
            run: agent.run.clone(),
            ..Ledger::default()
        };
        let ledger = if raw.is_empty() {
            fresh()
        } else {
            let ledger = decode::<Ledger>(&raw).ok_or("invalid prompt receipts")?;
            if ledger.run == agent.run {
                ledger
            } else {
                fresh()
            }
        };
        if ledger.entries.len() > MAX_RETAINED
            || ledger.entries.iter().any(|e| {
                e.operation == 0
                    || e.operation > ledger.sequence
                    || !["pending", "delivered"].contains(&e.stage.as_str())
            })
        {
            return Err("invalid prompt receipt history".into());
        }
        let tickets = decode::<Tickets>(&tickets)
            .filter(|tickets| tickets.run == agent.run)
            .unwrap_or_else(|| Tickets {
                run: agent.run.clone(),
                entries: Vec::new(),
            });
        Ok(PaneLedger {
            ledger,
            tickets,
            raw,
            fence,
            store,
        })
    }

    pub async fn prompt_with_operation(
        &self,
        agent: &Agent,
        text: &str,
        operation: Option<u64>,
    ) -> Result<Value, String> {
        if text.is_empty()
            || text.len() > 32768
            || text
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Err("prompt must contain 1–32768 bytes without control keys".into());
        }
        let _lock = self.lock()?;
        let explicit = operation.is_some();
        let digest = sha256(text);
        let mut reconciled = false;
        loop {
            let pane = self.prompt_ledger(agent).await?;
            let next = pane
                .ledger
                .sequence
                .checked_add(1)
                .ok_or("prompt operation counter exhausted")?;
            let operation = operation.unwrap_or(next);
            if operation == 0 || operation > next {
                return Err(format!("next prompt operation must be {next}"));
            }
            if operation < next {
                let Some(receipt) = pane
                    .ledger
                    .entries
                    .iter()
                    .find(|r| r.operation == operation)
                else {
                    return self.retained_prompt(agent, operation).await.map_err(|_| {
                        "outcome_unknown: prompt receipt expired; the operation will not be repeated".into()
                    });
                };
                if receipt.digest != digest {
                    return Err("prompt operation was already used with different content".into());
                }
                return Ok(pane.public(receipt));
            }
            if pane.ledger.entries.iter().any(|r| r.stage == "pending") {
                return Err("a prompt delivery is unresolved; inspect its receipt and the native TUI before any new submission".into());
            }
            let current = self.get(&agent.pane_id).await?;
            if current.run != agent.run
                || current.boot != agent.boot
                || current.generation != agent.generation
                || current.session_id != agent.session_id
                || current.revision != agent.revision
            {
                return Err(
                    "identity_mismatch: agent changed while preparing the prompt; inspect it again"
                        .into(),
                );
            }
            if current.state != "idle" || current.process != "running" {
                return Err("prompt requires an idle, verified foreground agent; blocked, working and unknown states cannot receive a prompt".into());
            }
            // A bare newline is a submit key when bracketed paste is not enabled.
            let needs_bracket = text.contains('\n') || text.contains('\t');
            if needs_bracket
                && self
                    .command(&[
                        "display-message",
                        "-p",
                        "-t",
                        &agent.pane_id,
                        "#{masil_bracketed_paste}",
                    ])
                    .await?
                    .trim()
                    != "1"
            {
                return Err(
                    "multiline prompt requires native bracketed paste; prepare a draft instead"
                        .into(),
                );
            }

            // Admission and dispatch intent are durable before the effect.
            let mut store = self.operation_store_recorded(&pane.store).await?;
            let id = if explicit {
                operation.to_string()
            } else {
                format!("auto-{}", nonce()?)
            };
            let namespace = format!("run:{}", agent.run);
            let request = NewOperation {
                namespace: &namespace,
                action: "prompt",
                id: &id,
                explicit,
                target: &agent.pane_id,
                run: Some(&agent.run),
                boot: &agent.boot,
                digest: &digest,
                payload_bytes: text.len() as u64,
            };
            let intent =
                json!({"slot": operation, "ledger_sha256": sha256(&pane.raw), "fence": pane.fence});
            let ticket = match store.admit(&request, intent, now_ms())? {
                Admission::Dispatch(ticket) => ticket,
                Admission::Recorded(record) => {
                    return Ok(public_record(&agent.run, operation, &record));
                }
                Admission::NeedsReconcile(record) if !reconciled => {
                    self.reconcile_prompt(&mut store, &record).await?;
                    reconciled = true;
                    continue;
                }
                Admission::NeedsReconcile(_) => {
                    return Err(
                        "outcome_unknown: prompt operation is still unresolved; see `agent operations`".into(),
                    );
                }
            };

            let buffer = format!("masil-prompt-{}", nonce()?);
            if let Err(error) = self
                .native
                .tmux(
                    ["load-buffer", "-b", &buffer, "-"]
                        .iter()
                        .map(OsString::from),
                    Some(text.as_bytes().to_vec()),
                )
                .await
                .map_err(super::server_unreachable)
            {
                // No input command was sent, so the slot is unused.
                let _ = store.finish(
                    &ticket,
                    "rejected_before_effect",
                    "load_buffer",
                    Some(&json!({"error": error})),
                    now_ms(),
                );
                let _ = self.command(&["delete-buffer", "-b", &buffer]).await;
                return Err(error);
            }
            let PaneLedger {
                mut ledger,
                mut tickets,
                raw,
                fence,
                ..
            } = pane;
            ledger.sequence = operation;
            if ledger.entries.len() == MAX_RETAINED {
                ledger.entries.remove(0);
            }
            ledger.entries.push(Receipt {
                operation,
                digest: digest.clone(),
                stage: "pending".into(),
            });
            tickets.entries.retain(|entry| {
                ledger
                    .entries
                    .iter()
                    .any(|e| e.operation == entry.operation)
            });
            tickets.entries.push(TicketEntry {
                operation,
                key: ticket.key.clone(),
                ticket: ticket.ticket.clone(),
            });
            let pending = encode(&ledger)?;
            ledger
                .entries
                .last_mut()
                .ok_or("missing prompt receipt")?
                .stage = "delivered".into();
            let delivered = encode(&ledger)?;
            let commands = vec![
                Self::option(agent, OPTION, pending),
                Self::option(agent, TICKETS, encode(&tickets)?),
                vec![
                    "paste-buffer".into(),
                    "-p".into(),
                    "-r".into(),
                    "-d".into(),
                    "-b".into(),
                    buffer.clone(),
                    "-t".into(),
                    agent.pane_id.clone(),
                ],
                vec![
                    "send-keys".into(),
                    "-t".into(),
                    agent.pane_id.clone(),
                    "Enter".into(),
                ],
                Self::option(agent, OPTION, delivered),
                vec![
                    "display-message".into(),
                    "-p".into(),
                    "masil-prompt-delivered".into(),
                ],
            ];
            // Check the evidence used by detection at the command queue boundary.
            // Input and output may still race inside the provider; delivery does not
            // establish that the provider accepted this as a new prompt.
            let mut conditions = vec![
                format!("#{{==:#{{{OPTION}}},{raw}}}"),
                format!("#{{==:#{{{FENCE}}},{fence}}}"),
                "#{==:#{synchronize-panes},0}".into(),
                "#{==:#{pane_in_mode},0}".into(),
                format!(
                    "#{{==:#{{pane_output_generation}},{}}}",
                    current.output_generation
                ),
                format!("#{{==:#{{pane_title}},{}}}", format_literal(&current.title)),
                format!(
                    "#{{==:#{{masil_osc_progress}},{}}}",
                    format_literal(&current.progress)
                ),
            ];
            if needs_bracket {
                conditions.push("#{==:#{masil_bracketed_paste},1}".into());
            }
            let result = self
                .guarded_input_outcome(&current, commands, Some(&and(&conditions)))
                .await;
            let _ = self.command(&["delete-buffer", "-b", &buffer]).await;
            return match result {
                Ok(Guarded::Applied) => {
                    finish(&mut store, &ticket, "delivered", "pane_ledger", None)
                        .map_err(String::from)?;
                    // Delivery is committed; a failed read of the pane's
                    // receipt does not make it unknown.
                    Ok(self
                        .prompt_receipt(agent, Some(operation))
                        .await
                        .unwrap_or_else(|_| {
                            json!({"operation": operation, "run": agent.run, "stage": "delivered",
                                   "provider_accepted": false, "task_success": null,
                                   "operation_key": ticket.key})
                        }))
                }
                Ok(Guarded::Rejected) => {
                    let _ =
                        store.finish(&ticket, "rejected_before_effect", "guard", None, now_ms());
                    Err(format!(
                        "rejected_before_effect: prompt operation {operation} was rejected or its delivery is unknown: agent foreground or run changed before input delivery; query prompt-receipt before retrying"
                    ))
                }
                Err(error) => {
                    // Part of the group may have run; settle from the ledger.
                    // A failed settle read still records the attempt as unknown.
                    let (state, evidence) = self
                        .settle_prompt(agent, &ticket.ticket, operation, &sha256(&raw), &fence)
                        .await
                        .unwrap_or_else(|settle| {
                            (
                                "outcome_unknown",
                                json!({"settle_error": settle.to_string()}),
                            )
                        });
                    let mut evidence = evidence;
                    evidence["error"] = json!(error);
                    finish(&mut store, &ticket, state, "pane_ledger", Some(evidence))
                        .map_err(String::from)?;
                    Err(String::from(AfterEffect::new(format!(
                        "prompt operation {operation} was rejected or its delivery is unknown: {error}; query prompt-receipt before retrying"
                    ))))
                }
            };
        }
    }

    /// Judge one attempt from the pane. If its ticket is absent, a fence
    /// written by compare-and-set makes the attempt unable to apply later,
    /// which turns absence into proof.
    async fn settle_prompt(
        &self,
        agent: &Agent,
        ticket: &str,
        slot: u64,
        ledger_sha256: &str,
        fence: &str,
    ) -> Result<(&'static str, Value), AfterEffect> {
        for _ in 0..3 {
            let pane = self.prompt_ledger(agent).await.map_err(AfterEffect::new)?;
            if let Some(entry) = pane.ticket_slot(ticket) {
                let state = if entry.stage == "delivered" {
                    "delivered"
                } else {
                    "outcome_unknown"
                };
                return Ok((state, json!({"ledger_stage": entry.stage})));
            }
            if pane.ledger.sequence >= slot {
                // Only one group can take a slot. If another attempt holds it,
                // this one never ran; if the slot was rotated out, nobody knows.
                return Ok(if pane.ledger.entries.iter().any(|e| e.operation == slot) {
                    (
                        "not_applied",
                        json!({"reason": "slot_taken_by_another_attempt"}),
                    )
                } else {
                    (
                        "outcome_unknown",
                        json!({"reason": "slot_no_longer_retained"}),
                    )
                });
            }
            if sha256(&pane.raw) != ledger_sha256 || pane.fence != fence {
                // The slot is unused and the attempt's compare-and-set can no
                // longer match: the fence is a fresh nonce and the ledger only
                // changes forward.
                return Ok(("not_applied", json!({"reason": "ledger_moved"})));
            }
            if self
                .compare_and_set(
                    &agent.pane_id,
                    &[(OPTION, &pane.raw), (FENCE, &pane.fence)],
                    &[(FENCE, nonce().map_err(AfterEffect::new)?)],
                )
                .await
                .map_err(AfterEffect::new)?
            {
                return Ok(("not_applied", json!({"reason": "fenced"})));
            }
        }
        Ok((
            "outcome_unknown",
            json!({"reason": "ledger_changed_during_fence"}),
        ))
    }

    /// Resolve an expired prompt attempt. Called with the management lock held.
    pub(super) async fn reconcile_prompt(
        &self,
        store: &mut Store,
        record: &Record,
    ) -> Result<Record, String> {
        let now = now_ms();
        let agent = match self.get(&record.target).await {
            Ok(agent) if Some(&agent.run) == record.run.as_ref() => agent,
            _ => {
                return store.resolve(
                    record,
                    "outcome_unknown",
                    "reconcile",
                    Some(&json!({"reason": "run_not_live"})),
                    now,
                );
            }
        };
        let intent = record.intent().cloned().unwrap_or_default();
        let (state, evidence) = self
            .settle_prompt(
                &agent,
                &record.ticket,
                intent["slot"].as_u64().unwrap_or(u64::MAX),
                intent["ledger_sha256"].as_str().unwrap_or_default(),
                intent["fence"].as_str().unwrap_or_default(),
            )
            .await?;
        store.resolve(record, state, "reconcile", Some(&evidence), now)
    }

    /// Record a user's judgement of an unresolved prompt after inspecting the
    /// TUI, and release the pending slot so new prompts are possible.
    pub(super) async fn resolve_prompt(
        &self,
        store: &mut Store,
        record: &Record,
        delivered: bool,
    ) -> Result<Record, String> {
        let _lock = self.lock()?;
        let state = if delivered {
            "user_confirmed_delivered"
        } else {
            "user_confirmed_not_delivered"
        };
        if let Ok(agent) = self.get(&record.target).await
            && Some(&agent.run) == record.run.as_ref()
        {
            let pane = self.prompt_ledger(&agent).await?;
            if let Some(slot) = pane.ticket_slot(&record.ticket).map(|e| e.operation) {
                let mut ledger = pane.ledger;
                if delivered {
                    for entry in &mut ledger.entries {
                        if entry.operation == slot {
                            entry.stage = "delivered".into();
                        }
                    }
                } else {
                    // The slot stays consumed; its receipt is the durable record.
                    ledger.entries.retain(|entry| entry.operation != slot);
                }
                if !self
                    .compare_and_set(
                        &agent.pane_id,
                        &[(OPTION, &pane.raw), (FENCE, &pane.fence)],
                        &[(OPTION, encode(&ledger)?), (FENCE, nonce()?)],
                    )
                    .await?
                {
                    return Err("prompt receipts changed; inspect them again".into());
                }
            }
        }
        store.settle_unknown(record, state, "user", None, now_ms())
    }
}

fn finish(
    store: &mut Store,
    ticket: &Ticket,
    state: &str,
    source: &str,
    evidence: Option<Value>,
) -> Result<(), AfterEffect> {
    store
        .finish(ticket, state, source, evidence.as_ref(), now_ms())
        .map(|_| ())
        .map_err(|error| {
            AfterEffect::new(format!(
                "prompt {state}, but its durable receipt could not be committed: {error}; query prompt-receipt"
            ))
        })
}

fn format_literal(value: &str) -> String {
    value
        .replace('#', "##")
        .replace('}', "#}")
        .replace(',', "#,")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(raw: &str, tickets: Tickets) -> PaneLedger {
        PaneLedger {
            ledger: serde_json::from_str(raw).unwrap(),
            tickets,
            raw: String::new(),
            fence: String::new(),
            store: String::new(),
        }
    }

    #[test]
    fn legacy_ledgers_keep_their_bytes_and_public_shape() {
        let legacy = r#"{"run":"r","sequence":1,"entries":[{"operation":1,"digest":"d","stage":"pending"}]}"#;
        let pane = pane(legacy, Tickets::default());
        assert_eq!(serde_json::to_string(&pane.ledger).unwrap(), legacy);
        assert_eq!(
            pane.public(&pane.ledger.entries[0]),
            json!({"operation":1,"run":"r","stage":"pending","provider_accepted":false,
                "task_success":null,"operation_key":"run:r/prompt/1"})
        );
    }

    #[test]
    fn tickets_identify_the_attempt_that_wrote_a_slot() {
        let raw = r#"{"run":"r","sequence":2,"entries":[{"operation":1,"digest":"a","stage":"delivered"},{"operation":2,"digest":"b","stage":"pending"}]}"#;
        let pane = pane(
            raw,
            Tickets {
                run: "r".into(),
                entries: vec![TicketEntry {
                    operation: 2,
                    key: "run:r/prompt/auto-x".into(),
                    ticket: "t2".into(),
                }],
            },
        );
        assert_eq!(pane.ticket_slot("t2").unwrap().stage, "pending");
        assert!(pane.ticket_slot("t1").is_none());
        assert_eq!(pane.key(2), "run:r/prompt/auto-x");
        assert_eq!(pane.key(1), "run:r/prompt/1");
    }

    #[test]
    fn a_full_ticket_list_fits_the_option_limit() {
        let tickets = Tickets {
            run: "r".repeat(32),
            entries: (0..MAX_RETAINED as u64)
                .map(|i| TicketEntry {
                    operation: u64::MAX - 16 + i,
                    key: format!("run:{}/prompt/auto-{}", "r".repeat(32), "n".repeat(32)),
                    ticket: "t".repeat(32),
                })
                .collect(),
        };
        assert!(decode::<Tickets>(&encode(&tickets).unwrap()).is_some());
    }
}
