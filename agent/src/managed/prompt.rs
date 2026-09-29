//! Guarded, explicit prompt delivery with bounded per-run retry receipts.
use super::{Agent, Manager, and, decode, encode, nonce};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::ffi::OsString;

const OPTION: &str = "@rmux-agent-prompt-receipts";
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

fn public(run: &str, entry: &Receipt) -> Value {
    json!({"operation":entry.operation,"run":run,"stage":entry.stage,"provider_accepted":false,"task_success":null})
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
        let ledger = self.prompt_ledger(agent).await?;
        let operation = operation.unwrap_or(ledger.sequence);
        let entry = ledger
            .entries
            .iter()
            .find(|e| e.operation == operation)
            .ok_or(
                "prompt receipt unavailable or expired; delivery must not be retried automatically",
            )?;
        Ok(public(&ledger.run, entry))
    }

    async fn prompt_ledger(&self, agent: &Agent) -> Result<Ledger, String> {
        let raw = self
            .command(&["show-options", "-pqv", "-t", &agent.pane_id, OPTION])
            .await?;
        if raw.trim().is_empty() {
            return Ok(Ledger {
                run: agent.run.clone(),
                ..Ledger::default()
            });
        }
        let ledger = decode::<Ledger>(raw.trim()).ok_or("invalid prompt receipts")?;
        if ledger.run != agent.run {
            return Ok(Ledger {
                run: agent.run.clone(),
                ..Ledger::default()
            });
        }
        if ledger.entries.len() > MAX_RETAINED
            || ledger.entries.iter().any(|e| {
                e.operation == 0
                    || e.operation > ledger.sequence
                    || !["pending", "delivered"].contains(&e.stage.as_str())
            })
        {
            return Err("invalid prompt receipt history".into());
        }
        Ok(ledger)
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
        let mut ledger = self.prompt_ledger(agent).await?;
        let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
        let next = ledger
            .sequence
            .checked_add(1)
            .ok_or("prompt operation counter exhausted")?;
        let operation = operation.unwrap_or(next);
        if operation == 0 || operation > next {
            return Err(format!("next prompt operation must be {next}"));
        }
        if operation < next {
            let receipt = ledger
                .entries
                .iter()
                .find(|r| r.operation == operation)
                .ok_or("prompt receipt expired; the operation will not be repeated")?;
            if receipt.digest != digest {
                return Err("prompt operation was already used with different content".into());
            }
            return Ok(public(&ledger.run, receipt));
        }
        if ledger.entries.iter().any(|r| r.stage == "pending") {
            return Err("a prompt delivery is unresolved; inspect its receipt and the native TUI before any new submission".into());
        }
        let current = self.get(&agent.pane_id).await?;
        if current.run != agent.run
            || current.boot != agent.boot
            || current.generation != agent.generation
            || current.session_id != agent.session_id
            || current.revision != agent.revision
        {
            return Err("agent changed while preparing the prompt; inspect it again".into());
        }
        if current.state != "idle" || current.process != "running" {
            return Err("prompt requires an idle, verified foreground agent; blocked, working and unknown states cannot receive a prompt".into());
        }
        // A bare newline is a submit key when bracketed paste is not enabled.
        if (text.contains('\n') || text.contains('\t'))
            && self
                .command(&[
                    "display-message",
                    "-p",
                    "-t",
                    &agent.pane_id,
                    "#{rmux_bracketed_paste}",
                ])
                .await?
                .trim()
                != "1"
        {
            return Err(
                "multiline prompt requires native bracketed paste; prepare a draft instead".into(),
            );
        }
        let buffer = format!("rmux-prompt-{}", nonce()?);
        self.native
            .tmux(
                ["load-buffer", "-b", &buffer, "-"]
                    .iter()
                    .map(OsString::from),
                Some(text.as_bytes().to_vec()),
            )
            .await?;
        ledger.sequence = operation;
        if ledger.entries.len() == MAX_RETAINED {
            ledger.entries.remove(0);
        }
        ledger.entries.push(Receipt {
            operation,
            digest,
            stage: "pending".into(),
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
                "rmux-prompt-delivered".into(),
            ],
        ];
        // Check the evidence used by detection at the command queue boundary.
        // Input and output may still race inside the provider; delivery does not
        // establish that the provider accepted this as a new prompt.
        let needs_bracket = text.contains('\n') || text.contains('\t');
        let mut conditions = vec![
            "#{==:#{synchronize-panes},0}".into(),
            "#{==:#{pane_in_mode},0}".into(),
            format!(
                "#{{==:#{{pane_output_generation}},{}}}",
                current.output_generation
            ),
            format!("#{{==:#{{pane_title}},{}}}", format_literal(&current.title)),
            format!(
                "#{{==:#{{rmux_osc_progress}},{}}}",
                format_literal(&current.progress)
            ),
        ];
        if needs_bracket {
            conditions.push("#{==:#{rmux_bracketed_paste},1}".into());
        }
        let result = self
            .guarded_input_condition(&current, commands, Some(&and(&conditions)))
            .await;
        let _ = self.command(&["delete-buffer", "-b", &buffer]).await;
        if let Err(error) = result {
            return Err(format!(
                "prompt operation {operation} was rejected or its delivery is unknown: {error}; query prompt-receipt before retrying"
            ));
        }
        self.prompt_receipt(agent, Some(operation)).await
    }
}

fn format_literal(value: &str) -> String {
    value
        .replace('#', "##")
        .replace('}', "#}")
        .replace(',', "#,")
}
