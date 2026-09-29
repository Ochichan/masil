//! Optional provider hook bridge. This module never edits provider configuration.

use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use super::Manager;

const MAX_CALLBACK_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy)]
struct Target {
    id: &'static str,
    capability: Capability,
    events: &'static [&'static str],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Capability {
    Lifecycle,
    SessionOnly,
}

impl Capability {
    fn label(self) -> &'static str {
        match self {
            Self::Lifecycle => "full_lifecycle",
            Self::SessionOnly => "native_session_only",
        }
    }
}

const TARGETS: &[Target] = &[
    Target {
        id: "pi",
        capability: Capability::Lifecycle,
        events: &["session_start", "agent_start", "agent_settled"],
    },
    Target {
        id: "omp",
        capability: Capability::Lifecycle,
        events: &[
            "session_start",
            "session_switch",
            "agent_start",
            "tool_approval_requested",
            "tool_approval_resolved",
            "tool_execution_start",
            "tool_execution_end",
            "agent_end",
        ],
    },
    Target {
        id: "claude",
        capability: Capability::SessionOnly,
        events: &["SessionStart"],
    },
    Target {
        id: "codex",
        capability: Capability::SessionOnly,
        events: &["SessionStart"],
    },
    Target {
        id: "copilot",
        capability: Capability::SessionOnly,
        events: &["SessionStart"],
    },
    Target {
        id: "devin",
        capability: Capability::SessionOnly,
        events: &[
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PermissionRequest",
            "Stop",
        ],
    },
    Target {
        id: "droid",
        capability: Capability::SessionOnly,
        events: &["SessionStart"],
    },
    Target {
        id: "kimi",
        capability: Capability::Lifecycle,
        events: &[
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PostToolUseFailure",
            "SubagentStart",
            "PreCompact",
            "PermissionRequest",
            "PermissionResult",
            "Stop",
            "Interrupt",
        ],
    },
    Target {
        id: "opencode",
        capability: Capability::Lifecycle,
        events: &[
            "chat.message",
            "session.updated",
            "session.status",
            "tool.execute.before",
            "tool.execute.after",
            "permission.asked",
            "permission.replied",
            "question.asked",
            "question.replied",
            "question.rejected",
            "session.compacted",
            "session.error",
            "session.idle",
        ],
    },
    Target {
        id: "kilo",
        capability: Capability::Lifecycle,
        events: &[
            "chat.message",
            "session.created",
            "session.updated",
            "session.status",
            "tool.execute.before",
            "tool.execute.after",
            "permission.asked",
            "permission.replied",
            "question.asked",
            "question.replied",
            "question.rejected",
            "session.compacted",
            "session.error",
            "session.idle",
        ],
    },
    Target {
        id: "hermes",
        capability: Capability::SessionOnly,
        events: &["on_session_start", "on_session_reset", "pre_llm_call"],
    },
    Target {
        id: "qodercli",
        capability: Capability::SessionOnly,
        events: &["SessionStart"],
    },
    Target {
        id: "qwen",
        capability: Capability::SessionOnly,
        events: &["SessionStart"],
    },
    Target {
        id: "cursor",
        capability: Capability::SessionOnly,
        events: &["sessionStart"],
    },
    Target {
        id: "mastracode",
        capability: Capability::Lifecycle,
        events: &[
            "SessionStart",
            "UserPromptSubmit",
            "AgentStart",
            "PreToolUse",
            "PermissionRequest",
            "PermissionResult",
            "SubagentStart",
            "SubagentEnd",
            "Interrupt",
            "AgentEnd",
            "Stop",
        ],
    },
    Target {
        id: "agy",
        capability: Capability::SessionOnly,
        events: &["PreInvocation"],
    },
    Target {
        id: "grok",
        capability: Capability::SessionOnly,
        events: &["session_start", "SessionStart", "sessionStart"],
    },
];

struct MappedCallback {
    state: Option<&'static str>,
    session: Option<String>,
    event: String,
    child_session: bool,
    root_binding: bool,
    allow_root_switch: bool,
}

pub async fn run(manager: &Manager, args: &[String]) -> Result<Value, String> {
    match args {
        [command] if command == "status" => Ok(json!({
            "installed": false,
            "targets": TARGETS.iter().map(target_status).collect::<Vec<_>>(),
        })),
        [command, provider] if command == "status" => {
            let target = target(provider)?;
            Ok(target_status(target))
        }
        [command, provider] if command == "export" => export(target(provider)?, None),
        [command, provider, flag, directory] if command == "export" && flag == "--directory" => {
            export(target(provider)?, Some(directory))
        }
        [command, provider] if command == "hook" => {
            hook_from_stdin(manager, target(provider)?, None).await
        }
        [command, provider, action] if command == "hook" => {
            hook_from_stdin(manager, target(provider)?, Some(action)).await
        }
        _ => Err(
            "usage: integration status [PROVIDER] | export PROVIDER [--directory ABSOLUTE_PATH] | hook PROVIDER [ACTION]"
                .into(),
        ),
    }
}

fn target(provider: &str) -> Result<&'static Target, String> {
    TARGETS
        .iter()
        .find(|target| target.id == provider)
        .ok_or_else(|| format!("provider has no verified native integration: {provider}"))
}

fn target_status(target: &Target) -> Value {
    json!({
        "provider": target.id,
        "capability": target.capability.label(),
        "events": target.events,
        "actions": if target.capability == Capability::Lifecycle { json!(["session", "working", "blocked", "idle"]) } else { json!(["session"]) },
        "export": export_kind(target.id),
        "export_available": true,
        "installed": false,
        "live_provider_tested": false,
        "source_reviewed": true,
        "state_authority": if target.capability == Capability::Lifecycle { "native_callback" } else { "screen_detection" },
    })
}

fn export(target: &Target, directory: Option<&String>) -> Result<Value, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("could not resolve masil-agent executable: {error}"))?;
    if !executable.is_absolute() {
        return Err("masil-agent executable path is not absolute".into());
    }
    let executable = executable
        .to_str()
        .ok_or("masil-agent executable path is not UTF-8")?;
    let directory = match directory {
        Some(directory) => {
            let path = std::path::PathBuf::from(directory);
            if !path.is_absolute()
                || directory.len() > 4096
                || directory.chars().any(char::is_control)
            {
                return Err("integration export directory must be an absolute path".into());
            }
            path
        }
        None => std::env::current_dir()
            .map_err(|error| format!("could not resolve export directory: {error}"))?,
    };
    let (files, registration) = export_artifacts(target, executable, &directory)?;
    Ok(json!({
        "provider": target.id,
        "capability": target.capability.label(),
        "installed": false,
        "live_provider_tested": false,
        "source_reviewed": true,
        "export": export_kind(target.id),
        "directory": directory,
        "files": files,
        "registration": registration,
        "events": target.events,
        "actions": if target.capability == Capability::Lifecycle { json!(["session", "working", "blocked", "idle"]) } else { json!(["session"]) },
        "usage": "Write each returned file at its absolute destination and merge the registration fragment only after review. masil does not install or merge these files.",
    }))
}

fn export_kind(provider: &str) -> &'static str {
    match provider {
        "pi" | "omp" => "typescript_extension",
        "opencode" | "kilo" => "javascript_plugin",
        "hermes" => "python_plugin",
        _ => "command_hook_with_registration",
    }
}

fn export_artifacts(
    target: &Target,
    executable: &str,
    directory: &std::path::Path,
) -> Result<(Vec<Value>, Value), String> {
    match target.id {
        "pi" | "omp" => {
            let name = format!("masil-{}-agent-state.ts", target.id);
            let destination = directory.join(&name);
            Ok((
                vec![artifact(
                    &destination,
                    "0644",
                    &typescript_extension(target.id, executable),
                )],
                json!({
                    "kind": "drop_in_extension",
                    "path": destination,
                    "entrypoint": "default function(pi)",
                }),
            ))
        }
        "opencode" | "kilo" => {
            let name = format!("masil-{}-agent-state.js", target.id);
            let destination = directory.join(&name);
            Ok((
                vec![artifact(
                    &destination,
                    "0644",
                    &javascript_plugin(target.id, executable),
                )],
                json!({
                    "kind": "drop_in_plugin",
                    "path": destination,
                    "entrypoint": if target.id == "opencode" { "default plugin object and MasilAgentStatePlugin" } else { "MasilAgentStatePlugin" },
                }),
            ))
        }
        "hermes" => {
            let plugin_dir = directory.join("masil-agent-state");
            let init = plugin_dir.join("__init__.py");
            let manifest = plugin_dir.join("plugin.yaml");
            Ok((
                vec![
                    artifact(&init, "0644", &hermes_plugin(executable)),
                    artifact(
                        &manifest,
                        "0644",
                        "name: masil-agent-state\nversion: \"1.0\"\ndescription: Report Hermes session identity to a managed masil pane\n",
                    ),
                ],
                json!({
                    "kind": "hermes_plugin_directory",
                    "path": plugin_dir,
                    "enable": "add masil-agent-state to Hermes config.yaml plugins",
                }),
            ))
        }
        _ => {
            let name = format!("masil-{}-hook.sh", target.id);
            let destination = directory.join(&name);
            let script = shell_bridge(target.id, executable);
            Ok((
                vec![artifact(&destination, "0755", &script)],
                registration_fragment(target, &destination),
            ))
        }
    }
}

fn artifact(path: &std::path::Path, mode: &str, content: &str) -> Value {
    json!({"path": path, "mode": mode, "content": content})
}

fn shell_bridge(provider: &str, executable: &str) -> String {
    format!(
        "#!/bin/sh\n# masil generated bridge for {provider}; provider configuration is not installed.\nset -eu\n[ -n \"${{TMUX_PANE:-}}\" ] || exit 64\n[ -n \"${{MASIL_AGENT_RUN:-}}\" ] || exit 64\nexec {binary} agent integration hook {provider} \"${{1:-}}\"\n",
        provider = posix_quote(provider),
        binary = posix_quote(executable),
    )
}

fn typescript_extension(provider: &str, executable: &str) -> String {
    let listeners = if provider == "pi" {
        r#"
  pi.on("session_start", (event: any, ctx: any) => send("session", session(event, ctx)));
  pi.on("agent_start", (event: any, ctx: any) => send("working", session(event, ctx)));
  pi.on("agent_settled", (event: any, ctx: any) => send("idle", session(event, ctx)));
"#
    } else {
        r#"
  if (process.env.OMPCODE === "1") return;
  pi.on("session_start", (event: any, ctx: any) => send("session", session(event, ctx)));
  pi.on("session_switch", (event: any, ctx: any) => send("session", session(event, ctx)));
  pi.on("agent_start", (event: any, ctx: any) => send("working", session(event, ctx)));
  pi.on("tool_approval_requested", (event: any, ctx: any) => send("blocked", session(event, ctx)));
  pi.on("tool_approval_resolved", (event: any, ctx: any) => send("working", session(event, ctx)));
  pi.on("tool_execution_start", (event: any, ctx: any) => { if (event?.toolName === "ask") send("blocked", session(event, ctx)); });
  pi.on("tool_execution_end", (event: any, ctx: any) => { if (event?.toolName === "ask") send("working", session(event, ctx)); });
  pi.on("agent_end", (event: any, ctx: any) => {
    const failed = [...(Array.isArray(event?.messages) ? event.messages : [])].reverse().find((message: any) => message?.role === "assistant")?.stopReason === "error";
    if (event?.willContinue !== true && !failed) send("idle", session(event, ctx));
  });
"#
    };
    r#"// masil generated extension; configuration is not installed.
import { spawn } from "node:child_process";
const MASIL_AGENT = __BINARY__;
const MAX_CALLBACK_BYTES = 256 * 1024;
function scopeConfigured() {
  const pane = process.env.TMUX_PANE;
  const run = process.env.MASIL_AGENT_RUN;
  const socket = process.env.MASIL_AGENT_SOCKET;
  const valid = (value: unknown): value is string => typeof value === "string" &&
    value.length > 0 && value.length <= 1024 && !/[\u0000-\u001f\u007f]/u.test(value);
  return typeof pane === "string" && /^%[0-9]+$/u.test(pane) &&
    valid(run) && valid(socket) && socket.startsWith("/");
}
function send(action: string, payload: unknown) {
  if (!scopeConfigured()) return;
  let body: string | undefined;
  try { body = JSON.stringify(payload ?? {}); } catch { return; }
  if (typeof body !== "string" || Buffer.byteLength(body, "utf8") > MAX_CALLBACK_BYTES) return;
  const child = spawn(MASIL_AGENT, ["agent", "integration", "hook", "__PROVIDER__", action], {
    env: process.env, stdio: ["pipe", "ignore", "ignore"], shell: false,
  });
  child.on("error", () => {});
  child.stdin.on("error", () => {});
  const timeout = setTimeout(() => { try { child.kill(); } catch {} }, 1500);
  timeout.unref?.();
  child.on("close", () => clearTimeout(timeout));
  child.stdin.end(body);
}
function session(event: any, ctx: any) {
  return { hook_event_name: event?.type, source: event?.reason,
    session_id: ctx?.sessionManager?.getSessionId?.() };
}
export default function (pi: any) {
__LISTENERS__
}
"#
    .replace(
        "__BINARY__",
        &serde_json::to_string(executable).unwrap_or_default(),
    )
    .replace("__PROVIDER__", provider)
    .replace("__LISTENERS__", listeners)
}

fn javascript_plugin(provider: &str, executable: &str) -> String {
    let default_export = if provider == "opencode" {
        r#"
export default { id: "masil.opencode", server: MasilAgentStatePlugin, setup() {} };
"#
    } else {
        ""
    };
    r#"// masil generated plugin; configuration is not installed.
import { spawn } from "node:child_process";
const MASIL_AGENT = __BINARY__;
const MAX_CALLBACK_BYTES = 256 * 1024;
function scopeConfigured() {
  const pane = process.env.TMUX_PANE;
  const run = process.env.MASIL_AGENT_RUN;
  const socket = process.env.MASIL_AGENT_SOCKET;
  const valid = value => typeof value === "string" && value.length > 0 &&
    value.length <= 1024 && !/[\u0000-\u001f\u007f]/u.test(value);
  return typeof pane === "string" && /^%[0-9]+$/u.test(pane) &&
    valid(run) && valid(socket) && socket.startsWith("/");
}
function send(payload, action) {
  if (!scopeConfigured()) return;
  let body;
  try { body = JSON.stringify(payload ?? {}); } catch { return; }
  if (typeof body !== "string" || Buffer.byteLength(body, "utf8") > MAX_CALLBACK_BYTES) return;
  const args = ["agent", "integration", "hook", "__PROVIDER__"];
  if (action) args.push(action);
  const child = spawn(MASIL_AGENT, args, { env: process.env,
    stdio: ["pipe", "ignore", "ignore"], shell: false });
  child.on("error", () => {});
  child.stdin.on("error", () => {});
  const timeout = setTimeout(() => { try { child.kill(); } catch {} }, 1500);
  timeout.unref?.();
  child.on("close", () => clearTimeout(timeout));
  child.stdin.end(body);
}
let rootSessionID;
const childSessions = new Set();
export const MasilAgentStatePlugin = async () => ({
  "chat.message": async ({ sessionID }) => {
    if (!sessionID || childSessions.has(sessionID)) return;
    const switched = rootSessionID && rootSessionID !== sessionID;
    rootSessionID = sessionID;
    send({ hook_event_name: switched ? "masil.session.selected" : "chat.message",
      masil_scope: "frontend", sessionID }, switched ? "session" : "working");
  },
  event: async ({ event }) => {
    const properties = event?.properties ?? {};
    const sessionID = properties.sessionID;
    if (properties.info?.id && properties.info?.parentID) {
      childSessions.add(properties.info.id);
      return;
    }
    if (!rootSessionID || !sessionID || childSessions.has(sessionID) || sessionID !== rootSessionID) return;
    send({ event });
  },
});
__DEFAULT__
"#
    .replace(
        "__BINARY__",
        &serde_json::to_string(executable).unwrap_or_default(),
    )
    .replace("__PROVIDER__", provider)
    .replace("__DEFAULT__", default_export)
}

fn hermes_plugin(executable: &str) -> String {
    r#"# masil generated Hermes plugin; configuration is not installed.
import json
import os
import subprocess

MASIL_AGENT = __BINARY__
INTERACTIVE = {"cli", "tui", "desktop", "acp"}

def report(event, **kwargs):
    if kwargs.get("platform") not in INTERACTIVE:
        return
    session_id = kwargs.get("session_id")
    if not isinstance(session_id, str) or not session_id:
        return
    payload = json.dumps({"event": event, "session_id": session_id})
    try:
        subprocess.run([MASIL_AGENT, "agent", "integration", "hook", "hermes", "session"],
                       input=payload, text=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       timeout=1, check=False, env=os.environ.copy())
    except Exception:
        pass

def register(ctx):
    ctx.register_hook("on_session_start", lambda **kwargs: report("on_session_start", **kwargs))
    ctx.register_hook("on_session_reset", lambda **kwargs: report("on_session_reset", **kwargs))
    ctx.register_hook("pre_llm_call", lambda **kwargs: report("pre_llm_call", **kwargs))
"#
    .replace(
        "__BINARY__",
        &serde_json::to_string(executable).unwrap_or_default(),
    )
}

fn registration_fragment(target: &Target, hook_path: &std::path::Path) -> Value {
    let command = |action: &str| {
        format!(
            "sh {} {}",
            posix_quote(&hook_path.display().to_string()),
            posix_quote(action)
        )
    };
    let nested =
        |action: &str| json!({"hooks":[{"type":"command","command":command(action),"timeout":10}]});
    match target.id {
        "claude" => {
            json!({"format":"settings.json fragment","hooks":{"SessionStart":[{"matcher":"^(startup|resume|clear|compact|fork)$","hooks":[{"type":"command","command":command("session"),"timeout":10}]}]}})
        }
        "codex" => {
            json!({"format":"hooks.json fragment","hooks":{"SessionStart":[nested("session")]},"enable":"set features.hooks=true in config.toml"})
        }
        "droid" | "qodercli" | "qwen" => {
            json!({"format":"JSON hooks fragment","hooks":{"SessionStart":[nested("session")]}})
        }
        "copilot" => {
            json!({"format":"settings.json fragment","hooks":{"SessionStart":[{"type":"command","bash":command("session"),"timeoutSec":10}]}})
        }
        "devin" => {
            json!({"format":"config.json fragment","hooks": target.events.iter().map(|event| ((*event).to_string(), json!([nested("session")]))).collect::<Map<String,Value>>() })
        }
        "cursor" => {
            json!({"format":"hooks.json fragment","version":1,"hooks":{"sessionStart":[{"command":command("session")}]}})
        }
        "mastracode" => {
            let actions = [
                ("SessionStart", "session"),
                ("UserPromptSubmit", "working"),
                ("AgentStart", "working"),
                ("PreToolUse", "working"),
                ("PermissionRequest", "blocked"),
                ("PermissionResult", "working"),
                ("SubagentStart", "working"),
                ("SubagentEnd", "working"),
                ("Interrupt", "idle"),
                ("AgentEnd", "idle"),
                ("Stop", "idle"),
            ];
            let hooks = actions.into_iter().map(|(event,action)| (event.to_string(),json!([{"type":"command","command":command(action),"timeout":10000,"description":"Report MastraCode state to masil"}]))).collect::<Map<String,Value>>();
            json!({"format":"hooks.json fragment","hooks":hooks})
        }
        "agy" => {
            json!({"format":"hooks.json fragment","hooks":{"masil":{"PreInvocation":[{"type":"command","command":command("session"),"timeout":10}]}}})
        }
        "grok" => {
            json!({"format":"dedicated hooks/masil.json","hooks":{"SessionStart":[nested("session")]}})
        }
        "kimi" => {
            let actions = [
                ("SessionStart", "session"),
                ("UserPromptSubmit", "working"),
                ("PermissionRequest", "blocked"),
                ("PermissionResult", "working"),
                ("Stop", "idle"),
                ("Interrupt", "idle"),
            ];
            let content = actions
                .into_iter()
                .map(|(event, action)| {
                    format!(
                        "[[hooks]]\nevent = {event:?}\ncommand = {:?}\ntimeout = 10\n",
                        command(action)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            json!({"format":"config.toml fragment","content":content,"limitation":"PreToolUse AskUserQuestion matcher entries require a manual provider-specific merge"})
        }
        _ => json!({"format":"manual","limitation":"adapter requires provider registration"}),
    }
}

fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn hook_from_stdin(
    manager: &Manager,
    target: &Target,
    action: Option<&String>,
) -> Result<Value, String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_CALLBACK_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read hook callback: {error}"))?;
    if bytes.len() > MAX_CALLBACK_BYTES {
        return Err(format!("hook callback exceeds {MAX_CALLBACK_BYTES} bytes"));
    }
    hook(manager, target, action.map(String::as_str), &bytes).await
}

async fn hook(
    manager: &Manager,
    target: &Target,
    action: Option<&str>,
    bytes: &[u8],
) -> Result<Value, String> {
    let payload: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid hook callback JSON: {error}"))?;
    let payload = payload
        .as_object()
        .ok_or("hook callback must be a JSON object")?;
    let pane = required_env("TMUX_PANE")?;
    crate::pane_id(&pane)?;
    let run = required_env("MASIL_AGENT_RUN")?;
    let sequence = callback_sequence()?;
    let mapped = map_callback(target, action.filter(|action| !action.is_empty()), payload)?;

    let agent = manager.get(&pane).await?;
    if agent.run != run || agent.provider != target.id || agent.process != "running" {
        return Err("stale or mismatched integration callback".into());
    }
    if matches!(target.id, "opencode" | "kilo") {
        if mapped.child_session {
            return Err("child-session callback cannot update the managed root session".into());
        }
        let callback_session = mapped
            .session
            .as_deref()
            .ok_or("OpenCode-family callback did not include a root session ID")?;
        match agent.session_id.as_deref() {
            None if !mapped.root_binding => {
                return Err(
                    "root session is unbound; a frontend-scoped callback is required".into(),
                );
            }
            Some(root) if root != callback_session && !mapped.allow_root_switch => {
                return Err("callback does not belong to the selected root session".into());
            }
            _ => {}
        }
    }

    if let Some(state) = mapped.state {
        manager
            .report_snapshot(&agent, sequence, state, mapped.session.as_deref())
            .await?;
    } else {
        let session = mapped
            .session
            .as_deref()
            .ok_or("identity callback did not include a session reference")?;
        manager
            .report_identity_snapshot(&agent, sequence, session)
            .await?;
    }
    Ok(json!({
        "stage": "native_callback_recorded",
        "provider": target.id,
        "pane_id": pane,
        "run": run,
        "sequence": sequence,
        "event": mapped.event,
        "state": mapped.state.unwrap_or("unknown"),
        "session_id": mapped.session,
        "provider_accepted": false,
    }))
}

fn required_env(name: &str) -> Result<String, String> {
    let value = std::env::var(name).map_err(|_| format!("{name} is required"))?;
    if value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control) {
        return Err(format!("{name} is invalid"));
    }
    Ok(value)
}

fn callback_sequence() -> Result<u64, String> {
    if let Ok(value) = std::env::var("MASIL_AGENT_SEQUENCE") {
        let sequence = value
            .parse::<u64>()
            .map_err(|_| "MASIL_AGENT_SEQUENCE must be a positive integer")?;
        return (sequence > 0)
            .then_some(sequence)
            .ok_or_else(|| "MASIL_AGENT_SEQUENCE must be a positive integer".into());
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "system clock is before the Unix epoch")?
        .as_nanos();
    Ok(nanos.min(u64::MAX as u128) as u64)
}

fn map_callback(
    target: &Target,
    action: Option<&str>,
    payload: &Map<String, Value>,
) -> Result<MappedCallback, String> {
    let session = session_id(target.id, payload);
    let (child_session, root_binding, allow_root_switch) =
        if matches!(target.id, "opencode" | "kilo") {
            open_code_provenance(payload)
        } else {
            (false, false, false)
        };
    if let Some(action) = action {
        if action == "session" {
            return Ok(MappedCallback {
                state: None,
                session,
                event: "session".into(),
                child_session,
                root_binding,
                allow_root_switch,
            });
        }
        if !matches!(action, "idle" | "working" | "blocked") {
            return Err(format!("unsupported integration action: {action}"));
        }
        if target.capability != Capability::Lifecycle {
            return Err(format!(
                "{} integration reports session identity only",
                target.id
            ));
        }
        return Ok(MappedCallback {
            state: Some(match action {
                "idle" => "idle",
                "working" => "working",
                "blocked" => "blocked",
                _ => unreachable!(),
            }),
            session,
            event: action.into(),
            child_session,
            root_binding,
            allow_root_switch,
        });
    }

    if target.capability == Capability::SessionOnly {
        let event = callback_event(payload).unwrap_or_default();
        if !event.is_empty() && !session_event_allowed(target.id, &event) {
            return Err(format!("unmapped {} callback event: {event}", target.id));
        }
        return Ok(MappedCallback {
            state: None,
            session,
            event: if event.is_empty() {
                "session".into()
            } else {
                event
            },
            child_session: false,
            root_binding: false,
            allow_root_switch: false,
        });
    }

    map_lifecycle(target.id, payload, session)
}

fn session_event_allowed(provider: &str, event: &str) -> bool {
    let normalized = normalize_event(event);
    match provider {
        "claude" | "codex" | "copilot" | "droid" | "qodercli" | "qwen" => {
            normalized == "sessionstart"
        }
        "cursor" => normalized == "sessionstart",
        "devin" => matches!(
            normalized.as_str(),
            "sessionstart"
                | "userpromptsubmit"
                | "pretooluse"
                | "posttooluse"
                | "permissionrequest"
                | "stop"
        ),
        "hermes" => matches!(
            normalized.as_str(),
            "onsessionstart" | "onsessionreset" | "prellmcall"
        ),
        "agy" => normalized == "preinvocation",
        "grok" => normalized == "sessionstart",
        _ => false,
    }
}

fn map_lifecycle(
    provider: &str,
    payload: &Map<String, Value>,
    session: Option<String>,
) -> Result<MappedCallback, String> {
    if matches!(provider, "opencode" | "kilo") {
        return map_open_code(provider, payload, session);
    }
    let event = callback_event(payload).ok_or("hook callback did not include an event name")?;
    let normalized = normalize_event(&event);
    let state = match provider {
        "pi" => match normalized.as_str() {
            "sessionstart" => None,
            "agentstart" => Some("working"),
            "agentsettled" => Some("idle"),
            _ => return Err(format!("unmapped pi callback event: {event}")),
        },
        "omp" => match normalized.as_str() {
            "sessionstart" | "sessionswitch" => None,
            "agentstart" | "toolapprovalresolved" => Some("working"),
            "toolapprovalrequested" => Some("blocked"),
            "toolexecutionstart"
                if first_text(payload, &["tool_name", "toolName"]) == Some("ask") =>
            {
                Some("blocked")
            }
            "toolexecutionend"
                if first_text(payload, &["tool_name", "toolName"]) == Some("ask") =>
            {
                Some("working")
            }
            "agentend" if omp_agent_end_is_idle(payload) => Some("idle"),
            _ => return Err(format!("unmapped omp callback event: {event}")),
        },
        "kimi" => match normalized.as_str() {
            "sessionstart" => None,
            "userpromptsubmit" | "posttooluse" | "posttoolusefailure" | "subagentstart"
            | "precompact" | "permissionresult" => Some("working"),
            "pretooluse"
                if first_text(payload, &["tool_name", "toolName"]) == Some("AskUserQuestion") =>
            {
                Some("blocked")
            }
            "pretooluse" => Some("working"),
            "permissionrequest" => Some("blocked"),
            "stop" | "interrupt" => Some("idle"),
            _ => return Err(format!("unmapped kimi callback event: {event}")),
        },
        "mastracode" => match normalized.as_str() {
            "sessionstart" => None,
            "userpromptsubmit" | "agentstart" | "pretooluse" | "permissionresult"
            | "subagentstart" | "subagentend" => Some("working"),
            "permissionrequest" => Some("blocked"),
            "interrupt" | "agentend" | "stop" => Some("idle"),
            _ => return Err(format!("unmapped mastracode callback event: {event}")),
        },
        _ => {
            return Err(format!(
                "provider does not have lifecycle mapping: {provider}"
            ));
        }
    };
    Ok(MappedCallback {
        state,
        session,
        event,
        child_session: false,
        root_binding: false,
        allow_root_switch: false,
    })
}

fn omp_agent_end_is_idle(payload: &Map<String, Value>) -> bool {
    if payload.get("willContinue").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    payload
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        })
        .is_none_or(|message| message.get("stopReason").and_then(Value::as_str) != Some("error"))
}

fn map_open_code(
    provider: &str,
    payload: &Map<String, Value>,
    session: Option<String>,
) -> Result<MappedCallback, String> {
    let event_object = payload
        .get("event")
        .and_then(Value::as_object)
        .unwrap_or(payload);
    let event = first_text(event_object, &["type", "event", "hook_event_name"])
        .ok_or("hook callback did not include an event name")?
        .to_string();
    let properties = event_object
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(event_object);
    let (child_session, root_binding, allow_root_switch) = open_code_provenance(payload);
    let state = match event.as_str() {
        "chat.message"
        | "tool.execute.before"
        | "tool.execute.after"
        | "permission.replied"
        | "question.replied"
        | "question.rejected"
        | "session.compacted" => Some("working"),
        "permission.asked" | "question.asked" | "session.error" => Some("blocked"),
        "session.idle" => Some("idle"),
        "session.created" if provider == "kilo" => None,
        "session.updated" => None,
        "masil.session.selected" if root_binding => None,
        "session.status" => match status_name(properties).as_deref() {
            Some("idle") => Some("idle"),
            Some("active" | "busy" | "pending" | "retry" | "running" | "streaming" | "working") => {
                Some("working")
            }
            Some(other) => return Err(format!("unmapped {provider} session status: {other}")),
            None => return Err(format!("{provider} session.status did not include status")),
        },
        _ => return Err(format!("unmapped {provider} callback event: {event}")),
    };
    Ok(MappedCallback {
        state,
        session,
        event,
        child_session,
        root_binding,
        allow_root_switch,
    })
}

fn open_code_provenance(payload: &Map<String, Value>) -> (bool, bool, bool) {
    let event_object = payload
        .get("event")
        .and_then(Value::as_object)
        .unwrap_or(payload);
    let properties = event_object
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(event_object);
    let info = properties
        .get("info")
        .and_then(Value::as_object)
        .unwrap_or(properties);
    let parent_keys = [
        "parentID",
        "parentId",
        "parent_id",
        "parentSessionID",
        "parentSessionId",
        "parent_session_id",
    ];
    let child = [payload, event_object, properties, info]
        .into_iter()
        .any(|object| first_text(object, &parent_keys).is_some());
    let event = first_text(event_object, &["type", "event", "hook_event_name"])
        .map(str::to_string)
        .or_else(|| callback_event(payload))
        .map(|event| normalize_event(&event))
        .unwrap_or_default();
    let frontend = first_text(payload, &["masil_scope"])
        .or_else(|| first_text(event_object, &["masil_scope"]))
        == Some("frontend");
    let root_binding = frontend && matches!(event.as_str(), "chatmessage" | "masilsessionselected");
    let allow_root_switch = frontend && event == "masilsessionselected";
    (child, root_binding, allow_root_switch)
}

fn status_name(properties: &Map<String, Value>) -> Option<String> {
    match properties.get("status")? {
        Value::String(value) => Some(value.to_ascii_lowercase()),
        Value::Object(status) => first_text(status, &["type"]).map(str::to_ascii_lowercase),
        _ => None,
    }
}

fn callback_event(payload: &Map<String, Value>) -> Option<String> {
    first_text(
        payload,
        &["hook_event_name", "hookEventName", "event", "type"],
    )
    .map(str::to_string)
}

fn session_id(provider: &str, payload: &Map<String, Value>) -> Option<String> {
    if provider == "grok"
        && let Ok(value) = std::env::var("GROK_SESSION_ID")
        && valid_text(&value)
    {
        return Some(value);
    }
    let keys = if provider == "agy" {
        &[
            "conversationId",
            "conversation_id",
            "session_id",
            "sessionId",
        ][..]
    } else {
        &[
            "session_id",
            "sessionId",
            "sessionID",
            "conversation_id",
            "conversationId",
            "thread_id",
            "threadId",
        ][..]
    };
    first_text(payload, keys)
        .or_else(|| {
            nested_text(
                payload,
                "properties",
                &["sessionID", "sessionId", "session_id"],
            )
        })
        .or_else(|| {
            payload
                .get("event")
                .and_then(Value::as_object)
                .and_then(|event| {
                    first_text(event, keys).or_else(|| {
                        nested_text(
                            event,
                            "properties",
                            &["sessionID", "sessionId", "session_id"],
                        )
                    })
                })
        })
        .filter(|value| valid_text(value))
        .map(str::to_string)
}

fn first_text<'a>(object: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
}

fn nested_text<'a>(object: &'a Map<String, Value>, parent: &str, keys: &[&str]) -> Option<&'a str> {
    object
        .get(parent)
        .and_then(Value::as_object)
        .and_then(|nested| first_text(nested, keys))
}

fn valid_text(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}

fn normalize_event(event: &str) -> String {
    event
        .chars()
        .filter(|character| !matches!(character, '_' | '-' | '.' | ':'))
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(source: &str) -> Map<String, Value> {
        serde_json::from_str::<Value>(source)
            .unwrap()
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn catalog_has_six_lifecycle_and_eleven_identity_targets() {
        assert_eq!(TARGETS.len(), 17);
        assert_eq!(
            TARGETS
                .iter()
                .filter(|target| target.capability == Capability::Lifecycle)
                .count(),
            6
        );
    }

    #[test]
    fn claude_and_codex_are_identity_only() {
        for provider in ["claude", "codex"] {
            let mapped = map_callback(
                target(provider).unwrap(),
                None,
                &object(r#"{"hook_event_name":"SessionStart","session_id":"session-1"}"#),
            )
            .unwrap();
            assert_eq!(mapped.state, None);
            assert_eq!(mapped.session.as_deref(), Some("session-1"));
        }
        assert!(
            map_callback(
                target("codex").unwrap(),
                Some("working"),
                &object(r#"{"session_id":"session-1"}"#),
            )
            .is_err()
        );
    }

    #[test]
    fn opencode_maps_verified_lifecycle_events() {
        let frontend = map_callback(
            target("opencode").unwrap(),
            Some("working"),
            &object(
                r#"{"hook_event_name":"chat.message","masil_scope":"frontend","sessionID":"root"}"#,
            ),
        )
        .unwrap();
        assert!(frontend.root_binding);
        assert!(!frontend.child_session);

        let selection = map_callback(
            target("opencode").unwrap(),
            Some("session"),
            &object(r#"{"hook_event_name":"masil.session.selected","masil_scope":"frontend","sessionID":"next-root"}"#),
        )
        .unwrap();
        assert!(selection.root_binding);
        assert!(selection.allow_root_switch);

        let working = map_callback(
            target("opencode").unwrap(),
            None,
            &object(r#"{"event":{"type":"session.status","properties":{"sessionID":"s1","status":{"type":"busy"}}}}"#),
        )
        .unwrap();
        assert_eq!(working.state, Some("working"));
        assert_eq!(working.session.as_deref(), Some("s1"));

        let blocked = map_callback(
            target("opencode").unwrap(),
            None,
            &object(r#"{"event":{"type":"permission.asked","properties":{"sessionID":"s1"}}}"#),
        )
        .unwrap();
        assert_eq!(blocked.state, Some("blocked"));
        let child = map_callback(
            target("opencode").unwrap(),
            None,
            &object(r#"{"event":{"type":"session.updated","properties":{"sessionID":"child","info":{"id":"child","parentID":"root"}}}}"#),
        )
        .unwrap();
        assert!(child.child_session);
        assert!(!child.root_binding);
        assert!(
            map_callback(
                target("opencode").unwrap(),
                None,
                &object(r#"{"event":{"type":"unknown.event","properties":{"sessionID":"s1"}}}"#),
            )
            .is_err()
        );
    }

    #[test]
    fn exported_script_quotes_the_executable_and_never_embeds_callback_data() {
        let exported = export(target("codex").unwrap(), None).unwrap();
        let script = exported["files"][0]["content"].as_str().unwrap();
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("exec '"));
        assert!(script.contains("integration hook 'codex'"));
        assert!(!script.contains("eval"));
        assert_eq!(exported["installed"], false);

        let plugin = javascript_plugin("opencode", "/missing/masil-agent");
        assert!(plugin.contains("scopeConfigured()"));
        assert!(plugin.contains("Buffer.byteLength(body, \"utf8\")"));
        assert!(plugin.contains("child.on(\"error\""));
        assert!(plugin.contains("child.stdin.on(\"error\""));
        assert!(plugin.contains("child.kill()"));
        assert!(!plugin.contains("retry"));
    }
}
