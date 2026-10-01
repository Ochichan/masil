//! Optional provider hook bridge. This module never edits provider configuration.

use std::io::Read;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{Map, Value, json};

use super::{Manager, ReportOrigin};

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
        events: &["SessionStart", "PermissionRequest", "Notification", "Stop"],
    },
    Target {
        id: "codex",
        capability: Capability::SessionOnly,
        events: &["SessionStart", "Stop"],
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
    /// The provider announced a deliberate session change (resume, clear,
    /// explicit selection). Other session changes are recorded as conflicts.
    session_switch: bool,
    /// This callback records an inbox event only. It must not change the
    /// pane's reported state or session binding.
    event_only: bool,
}

pub async fn run(manager: &Manager, args: &[String]) -> Result<Option<Value>, String> {
    match args {
        [command] if command == "status" => Ok(Some(json!({
            "installed": false,
            "targets": TARGETS.iter().map(target_status).collect::<Vec<_>>(),
        }))),
        [command, provider] if command == "status" => {
            let target = target(provider)?;
            Ok(Some(target_status(target)))
        }
        [command, provider] if command == "export" => export(target(provider)?, None).map(Some),
        [command, provider, flag, directory] if command == "export" && flag == "--directory" => {
            export(target(provider)?, Some(directory)).map(Some)
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

/// The callback capability masil can accept for a provider, if any.
pub(super) fn target_capability(provider: &str) -> Option<&'static str> {
    TARGETS
        .iter()
        .find(|target| target.id == provider)
        .map(|target| target.capability.label())
}

fn target(provider: &str) -> Result<&'static Target, String> {
    TARGETS
        .iter()
        .find(|target| target.id == provider)
        .ok_or_else(|| {
            format!("unknown_provider: provider has no verified native integration: {provider}")
        })
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
    session_id: ctx?.sessionManager?.getSessionId?.(),
    event: { type: event?.type, toolName: event?.toolName, toolCallId: event?.toolCallId } };
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
            json!({"format":"settings.json fragment","hooks":{
                "SessionStart":[{"matcher":"^(startup|resume|clear|compact|fork)$","hooks":[{"type":"command","command":command("session"),"timeout":10}]}],
                "PermissionRequest":[{"hooks":[{"type":"command","command":command(""),"timeout":10}]}],
                "Notification":[{"matcher":"permission_prompt|elicitation_dialog","hooks":[{"type":"command","command":command(""),"timeout":10}]}],
                "Stop":[{"hooks":[{"type":"command","command":command(""),"timeout":10}]}],
            }})
        }
        "codex" => {
            json!({"format":"hooks.json fragment","hooks":{"SessionStart":[nested("session")],"Stop":[nested("")]},"enable":"set features.hooks=true in config.toml"})
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
                ("SessionStart", "session", None),
                ("UserPromptSubmit", "working", None),
                ("PreToolUse", "blocked", Some("^AskUserQuestion$")),
                ("PermissionRequest", "blocked", None),
                ("PermissionResult", "working", None),
                ("Stop", "idle", None),
                ("Interrupt", "idle", None),
            ];
            let content = actions
                .into_iter()
                .map(|(event, action, matcher)| {
                    let matcher = matcher
                        .map_or_else(String::new, |matcher| format!("matcher = {matcher:?}\n"));
                    format!(
                        "[[hooks]]\nevent = {event:?}\n{matcher}command = {:?}\ntimeout = 10\n",
                        command(action),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            json!({"format":"config.toml fragment","content":content})
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
) -> Result<Option<Value>, String> {
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

/// One process between the hook and the pane.
#[derive(Clone, Debug, PartialEq)]
struct Ancestor {
    group: i32,
    provider: Option<&'static str>,
    /// A shell, as provider hook runners use to start a hook command.
    shell: bool,
}

const MAX_ANCESTRY: usize = 32;
/// Shells and wrappers a provider may use to start a hook command; such a
/// process may sit in its own process group.
const HOOK_SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "fish", "ksh", "mksh", "tcsh", "csh", "yash", "nu", "pwsh",
    "timeout",
];

/// The processes from this process's parent up to and including `pane_pid`,
/// or None when the walk does not reach the pane. Each parent must have
/// started no later than its child, so a reused process ID cannot be
/// spliced into the path.
fn ancestry(pane_pid: i32) -> Option<Vec<Ancestor>> {
    // SAFETY: getppid has no preconditions.
    let first = unsafe { libc::getppid() };
    let mut pid = first;
    let mut child_started = u64::MAX;
    let mut chain = Vec::new();
    for _ in 0..MAX_ANCESTRY {
        if pid <= 1 {
            return None;
        }
        let info = crate::process::info(pid)?;
        if info.started > child_started {
            return None;
        }
        let argv = crate::process::argv(pid).unwrap_or_default();
        let name = argv
            .first()
            .map(|program| program.trim().rsplit('/').next().unwrap_or_default())
            .unwrap_or_default();
        chain.push(Ancestor {
            group: info.group,
            provider: crate::providers::identify_process(&argv).map(|provider| provider.id),
            shell: HOOK_SHELLS.contains(&name.trim_start_matches('-')),
        });
        if pid == pane_pid {
            // SAFETY: as above; the parent must not have changed meanwhile.
            return (unsafe { libc::getppid() } == first).then_some(chain);
        }
        child_started = info.started;
        pid = info.parent;
    }
    None
}

/// Refuses a callback only on evidence that it did not come from the pane's
/// own provider; it is accident prevention within one user, not proof of
/// origin, and passing it authorizes nothing.
/// - The hook must descend from the pane.
/// - Between the hook and the pane, a process outside the pane's foreground
///   group must be a hook shell; anything else, such as a shared provider
///   daemon, is refused whatever its name.
/// - At most one provider run: adjacent processes of one provider (a
///   launcher, a waiting shim, the native program) are one run; a provider
///   process separated by other processes or of another provider, like a
///   nested `claude -p`, starts another.
///
/// With no provider identified on the path the callback is accepted, as
/// before this check.
fn provenance(chain: Option<&[Ancestor]>, provider: &str, foreground: i32) -> Result<(), String> {
    let refuse = |why: &str| {
        Err(format!(
            "identity_mismatch: callback did not come from this pane's provider process ({why})"
        ))
    };
    let Some(chain) = chain else {
        return refuse("it does not descend from the pane");
    };
    let below_pane = &chain[..chain.len().saturating_sub(1)];
    if below_pane
        .iter()
        .any(|process| process.group != foreground && !process.shell)
    {
        return refuse(
            "a process outside the pane's foreground group, such as a shared provider daemon",
        );
    }
    let mut runs: Vec<&'static str> = Vec::new();
    let mut previous: Option<&Ancestor> = None;
    for process in chain {
        if let Some(id) = process.provider {
            // Adjacent processes of one provider are one run: a launcher, a
            // shim that waits for its tool, or the provider's own helper.
            let joins = previous.is_some_and(|child| child.provider == Some(id));
            if !joins {
                runs.push(id);
            }
        }
        previous = Some(process);
    }
    match runs.as_slice() {
        [] => Ok(()),
        [only] if *only != provider => refuse("another provider's process"),
        [_] => Ok(()),
        _ => refuse("a nested provider"),
    }
}

/// Appends a refused callback to a bounded log that doctor reports, since a
/// hook's stderr is rarely shown by the provider.
fn log_refusal(provider: &str, pane: &str, reason: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    const LIMIT: u64 = 64 * 1024;
    let Ok(base) = super::operations::state_base() else {
        return;
    };
    let directory = base.join("masil");
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = directory.join("integration-refusals.log");
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > LIMIT) {
        let _ = std::fs::rename(&path, directory.join("integration-refusals.log.1"));
    }
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
    {
        let line = json!({"at_ms": at, "provider": provider, "pane": pane, "reason": reason});
        let _ = writeln!(file, "{line}");
    }
}

async fn hook(
    manager: &Manager,
    target: &Target,
    action: Option<&str>,
    bytes: &[u8],
) -> Result<Option<Value>, String> {
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

    // A concurrent list or desk poll may win the tracked update; read again.
    let agent = match manager.get(&pane).await {
        Err(error) if error.contains("agent run changed before the action") => {
            manager.get(&pane).await?
        }
        result => result?,
    };
    if agent.run != run || agent.provider != target.id || agent.process != "running" {
        return Err("identity_mismatch: stale or mismatched integration callback".into());
    }
    // The environment names the pane, but a shared provider daemon or a
    // nested provider carries another pane's environment; the process tree
    // decides. A managed pane's process is its foreground group leader.
    let foreground = agent.foreground_group;
    if let Err(error) = provenance(ancestry(foreground).as_deref(), target.id, foreground) {
        log_refusal(target.id, &pane, &error);
        return Err(error);
    }
    if mapped.event_only {
        if let (Some(callback_session), Some(bound_session)) =
            (mapped.session.as_deref(), agent.session_id.as_deref())
            && callback_session != bound_session
        {
            return Ok(None);
        }
        manager
            .inbox_apply(callback_effects(
                target.id, &pane, &run, sequence, &mapped, payload,
            ))
            .await;
        return Ok(None);
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

    // The event is recorded before the state, which stays sequence-gated, so
    // a late callback still reaches the inbox; a contradicted binding (a
    // different session with no switch) records nothing.
    let contradicted = mapped.session.as_deref().is_some_and(|session| {
        agent
            .session_id
            .as_deref()
            .is_some_and(|bound| bound != session)
    }) && !(mapped.session_switch || mapped.allow_root_switch);
    if !contradicted {
        manager
            .inbox_apply(callback_effects(
                target.id, &pane, &run, sequence, &mapped, payload,
            ))
            .await;
    }
    let origin = ReportOrigin::Callback {
        event: &mapped.event,
        frontend: mapped.root_binding,
        switch: mapped.session_switch || mapped.allow_root_switch,
    };
    let result = if let Some(state) = mapped.state {
        manager
            .report_snapshot(&agent, sequence, state, mapped.session.as_deref(), origin)
            .await?
    } else {
        let session = mapped
            .session
            .as_deref()
            .ok_or("identity callback did not include a session reference")?;
        manager
            .report_identity_snapshot(&agent, sequence, session, origin)
            .await?
    };
    Ok(Some(json!({
        "stage": "native_callback_recorded",
        "provider": target.id,
        "pane_id": pane,
        "run": run,
        "sequence": sequence,
        "event": mapped.event,
        "state": mapped.state.unwrap_or("unknown"),
        "state_applied": result.state_applied,
        "session_id": mapped.session,
        "binding": result.binding.map(|binding| binding.label()),
        "provider_accepted": false,
    })))
}

/// Inbox effects of one accepted callback. A reported `blocked` is an event
/// keyed per callback, since hooks are not redelivered.
fn callback_effects(
    provider: &str,
    pane: &str,
    run: &str,
    sequence: u64,
    mapped: &MappedCallback,
    payload: &Map<String, Value>,
) -> Vec<super::inbox::Effect> {
    let source = format!("hook:{provider}");
    let event = callback_event(payload).unwrap_or_else(|| mapped.event.clone());
    let event = normalize_event(&event);
    let summary = callback_summary(provider, &event, payload);
    let mut effects = Vec::new();
    let mut requested = false;

    match (provider, event.as_str()) {
        ("opencode" | "kilo", "permissionasked" | "permissionaskedv2") => {
            if let Some(id) = callback_id(payload, &["id"]) {
                let source_ref = request_ref(&id);
                effects.push(callback_event_effect(
                    provider,
                    pane,
                    run,
                    "approval_requested",
                    source_ref,
                    Some(id),
                    summary.clone(),
                ));
                requested = true;
            }
        }
        ("opencode" | "kilo", "questionasked" | "questionaskedv2") => {
            if let Some(id) = callback_id(payload, &["id"]) {
                let source_ref = request_ref(&id);
                effects.push(callback_event_effect(
                    provider,
                    pane,
                    run,
                    "question_asked",
                    source_ref,
                    Some(id),
                    summary.clone(),
                ));
                requested = true;
            }
        }
        ("opencode" | "kilo", "permissionreplied" | "permissionrepliedv2") => {
            if let Some(id) = callback_id(payload, &["requestID", "requestId", "request_id"]) {
                effects.push(resolve_effect(&source, &id, "replied"));
            }
        }
        ("opencode" | "kilo", "questionreplied" | "questionrepliedv2") => {
            if let Some(id) = callback_id(payload, &["requestID", "requestId", "request_id"]) {
                effects.push(resolve_effect(&source, &id, "replied"));
            }
        }
        ("opencode" | "kilo", "questionrejected" | "questionrejectedv2") => {
            if let Some(id) = callback_id(payload, &["requestID", "requestId", "request_id"]) {
                effects.push(resolve_effect(&source, &id, "rejected"));
            }
        }
        ("kimi", "permissionrequest") => {
            let id = callback_id(payload, &["toolCallId", "tool_call_id"]);
            let source_ref = id
                .as_deref()
                .map(request_ref)
                .unwrap_or_else(|| callback_ref("approval_requested", run, sequence));
            effects.push(callback_event_effect(
                provider,
                pane,
                run,
                "approval_requested",
                source_ref,
                id,
                summary.clone(),
            ));
            requested = true;
        }
        ("kimi", "permissionresult") => {
            if let Some(id) = callback_id(payload, &["toolCallId", "tool_call_id"]) {
                effects.push(resolve_effect(&source, &id, "decided"));
            }
        }
        ("kimi", "pretooluse")
            if callback_tool(payload)
                .is_some_and(|tool| tool.eq_ignore_ascii_case("AskUserQuestion")) =>
        {
            let id = callback_id(payload, &["toolCallId", "tool_call_id"]);
            let source_ref = id
                .as_deref()
                .map(request_ref)
                .unwrap_or_else(|| callback_ref("question_asked", run, sequence));
            effects.push(callback_event_effect(
                provider,
                pane,
                run,
                "question_asked",
                source_ref,
                id,
                summary.clone(),
            ));
            requested = true;
        }
        ("omp", "toolapprovalrequested") => {
            let id = callback_id(payload, &["toolCallId", "tool_call_id"]);
            let source_ref = id
                .as_deref()
                .map(request_ref)
                .unwrap_or_else(|| callback_ref("approval_requested", run, sequence));
            effects.push(callback_event_effect(
                provider,
                pane,
                run,
                "approval_requested",
                source_ref,
                id,
                summary.clone(),
            ));
            requested = true;
        }
        ("omp", "toolapprovalresolved") => {
            if let Some(id) = callback_id(payload, &["toolCallId", "tool_call_id"]) {
                effects.push(resolve_effect(&source, &id, "decided"));
            }
        }
        ("omp", "toolexecutionstart")
            if callback_tool(payload).is_some_and(|tool| tool.eq_ignore_ascii_case("ask")) =>
        {
            let id = callback_id(payload, &["toolCallId", "tool_call_id"]);
            let source_ref = id
                .as_deref()
                .map(request_ref)
                .unwrap_or_else(|| callback_ref("question_asked", run, sequence));
            effects.push(callback_event_effect(
                provider,
                pane,
                run,
                "question_asked",
                source_ref,
                id,
                summary.clone(),
            ));
            requested = true;
        }
        ("omp", "toolexecutionend")
            if callback_tool(payload).is_some_and(|tool| tool.eq_ignore_ascii_case("ask")) =>
        {
            if let Some(id) = callback_id(payload, &["toolCallId", "tool_call_id"]) {
                effects.push(resolve_effect(&source, &id, "completed"));
            }
        }
        ("mastracode", "permissionrequest") => {
            let id = callback_id(
                payload,
                &[
                    "toolCallId",
                    "tool_call_id",
                    "requestID",
                    "requestId",
                    "request_id",
                    "permissionID",
                    "permissionId",
                    "permission_id",
                    "id",
                ],
            );
            let source_ref = id
                .as_deref()
                .map(request_ref)
                .unwrap_or_else(|| callback_ref("approval_requested", run, sequence));
            effects.push(callback_event_effect(
                provider,
                pane,
                run,
                "approval_requested",
                source_ref,
                id,
                summary.clone(),
            ));
            requested = true;
        }
        ("mastracode", "permissionresult") => {
            if let Some(id) = callback_id(
                payload,
                &[
                    "toolCallId",
                    "tool_call_id",
                    "requestID",
                    "requestId",
                    "request_id",
                    "permissionID",
                    "permissionId",
                    "permission_id",
                    "id",
                ],
            ) {
                effects.push(resolve_effect(&source, &id, "decided"));
            }
        }
        ("claude", "permissionrequest") => {
            effects.push(callback_event_effect(
                provider,
                pane,
                run,
                "approval_requested",
                callback_ref("approval_requested", run, sequence),
                None,
                summary.clone(),
            ));
            requested = true;
        }
        ("claude", "notification") => {
            match callback_text(payload, &["notification_type", "notificationType"])
                .map(normalize_event)
                .as_deref()
            {
                Some("permissionprompt") => {
                    effects.push(callback_event_effect(
                        provider,
                        pane,
                        run,
                        "blocked",
                        callback_ref("blocked", run, sequence),
                        None,
                        summary.clone(),
                    ));
                    requested = true;
                }
                Some("elicitationdialog") => {
                    effects.push(callback_event_effect(
                        provider,
                        pane,
                        run,
                        "question_asked",
                        callback_ref("question_asked", run, sequence),
                        None,
                        summary.clone(),
                    ));
                    requested = true;
                }
                _ => {}
            }
        }
        ("claude", "stop") => {
            let id = callback_id(payload, &["prompt_id", "promptId"]);
            let source_ref = id
                .as_deref()
                .map(|id| format!("turn_completed:{run}:{id}"))
                .unwrap_or_else(|| callback_ref("turn_completed", run, sequence));
            effects.push(callback_event_effect(
                provider,
                pane,
                run,
                "turn_completed",
                source_ref,
                id,
                summary.clone(),
            ));
            requested = true;
        }
        ("codex", "stop") => {
            let id = callback_id(payload, &["turn_id", "turnId"]);
            let source_ref = id
                .as_deref()
                .map(|id| format!("turn_completed:{run}:{id}"))
                .unwrap_or_else(|| callback_ref("turn_completed", run, sequence));
            effects.push(callback_event_effect(
                provider,
                pane,
                run,
                "turn_completed",
                source_ref,
                id,
                summary.clone(),
            ));
            requested = true;
        }
        _ => {}
    }

    if !requested && event == "blocked" && provider == "kimi" {
        let id = callback_id(payload, &["toolCallId", "tool_call_id"]);
        let kind = callback_tool(payload)
            .is_some_and(|tool| tool.eq_ignore_ascii_case("AskUserQuestion"))
            .then_some("question_asked")
            .or_else(|| id.as_ref().map(|_| "approval_requested"));
        if let Some(kind) = kind {
            let source_ref = id
                .as_deref()
                .map(request_ref)
                .unwrap_or_else(|| callback_ref(kind, run, sequence));
            effects.push(callback_event_effect(
                provider,
                pane,
                run,
                kind,
                source_ref,
                id,
                summary.clone(),
            ));
            requested = true;
        }
    }
    if !requested && event == "blocked" && provider == "mastracode" {
        let id = callback_id(
            payload,
            &[
                "toolCallId",
                "tool_call_id",
                "requestID",
                "requestId",
                "request_id",
                "permissionID",
                "permissionId",
                "permission_id",
                "id",
            ],
        );
        let source_ref = id
            .as_deref()
            .map(request_ref)
            .unwrap_or_else(|| callback_ref("approval_requested", run, sequence));
        effects.push(callback_event_effect(
            provider,
            pane,
            run,
            "approval_requested",
            source_ref,
            id,
            summary.clone(),
        ));
        requested = true;
    }
    if event == "working"
        && provider == "kimi"
        && let Some(id) = callback_id(payload, &["toolCallId", "tool_call_id"])
        && callback_text(payload, &["decision"]).is_some()
    {
        effects.push(resolve_effect(&source, &id, "decided"));
    }
    if event == "working"
        && provider == "mastracode"
        && let Some(id) = callback_id(
            payload,
            &[
                "toolCallId",
                "tool_call_id",
                "requestID",
                "requestId",
                "request_id",
                "permissionID",
                "permissionId",
                "permission_id",
                "id",
            ],
        )
        && callback_text(payload, &["decision", "result"]).is_some()
    {
        effects.push(resolve_effect(&source, &id, "decided"));
    }

    if !requested && mapped.state == Some("blocked") {
        effects.push(callback_event_effect(
            provider,
            pane,
            run,
            "blocked",
            callback_ref("blocked", run, sequence),
            None,
            summary,
        ));
    }
    effects
}

fn callback_event_effect(
    provider: &str,
    pane: &str,
    run: &str,
    kind: &'static str,
    source_ref: String,
    native_ref: Option<String>,
    summary: Option<Value>,
) -> super::inbox::Effect {
    super::inbox::Effect::Event {
        source: format!("hook:{provider}"),
        source_ref,
        provider: provider.into(),
        pane: pane.into(),
        run: run.into(),
        revision: None,
        kind,
        native_ref,
        summary,
    }
}

fn callback_ref(kind: &str, run: &str, sequence: u64) -> String {
    format!("{kind}:{run}:{sequence}")
}

fn request_ref(id: &str) -> String {
    format!("request:{id}")
}

fn resolve_effect(source: &str, id: &str, resolution: &'static str) -> super::inbox::Effect {
    super::inbox::Effect::Resolve {
        source: source.into(),
        source_ref: request_ref(id),
        resolution,
    }
}

fn callback_value<'a>(payload: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    let event = payload.get("event").and_then(Value::as_object);
    for object in [Some(payload), event].into_iter().flatten() {
        if let Some(value) = keys.iter().find_map(|key| object.get(*key)) {
            return Some(value);
        }
    }
    for object in [Some(payload), event].into_iter().flatten() {
        if let Some(properties) = object.get("properties").and_then(Value::as_object)
            && let Some(value) = keys.iter().find_map(|key| properties.get(*key))
        {
            return Some(value);
        }
        if let Some(input) = object
            .get("inputData")
            .or_else(|| object.get("input_data"))
            .and_then(Value::as_object)
            && let Some(value) = keys.iter().find_map(|key| input.get(*key))
        {
            return Some(value);
        }
    }
    None
}

fn callback_text<'a>(payload: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    callback_value(payload, keys)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn callback_id(payload: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    callback_text(payload, keys)
        .filter(|value| valid_text(value))
        .map(str::to_owned)
}

fn callback_tool(payload: &Map<String, Value>) -> Option<String> {
    callback_text(payload, &["tool_name", "toolName", "tool"])
        .or_else(|| {
            callback_value(payload, &["tool"])
                .and_then(Value::as_object)
                .and_then(|tool| first_text(tool, &["name", "tool_name", "toolName"]))
        })
        .map(str::to_owned)
}

fn callback_input_text<'a>(payload: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    callback_value(payload, &["tool_input", "toolInput"])
        .and_then(Value::as_object)
        .and_then(|input| first_text(input, keys))
}

fn callback_command(payload: &Map<String, Value>) -> Option<String> {
    callback_text(payload, &["command"])
        .or_else(|| callback_input_text(payload, &["command", "cmd", "script"]))
        .or_else(|| callback_value(payload, &["tool_input", "toolInput"]).and_then(Value::as_str))
        .map(str::to_owned)
}

fn callback_file_path(payload: &Map<String, Value>) -> Option<String> {
    callback_text(payload, &["file_path", "filePath", "path"])
        .or_else(|| callback_input_text(payload, &["file_path", "filePath", "path"]))
        .map(str::to_owned)
}

fn callback_url(payload: &Map<String, Value>) -> Option<String> {
    callback_text(payload, &["url"])
        .or_else(|| callback_input_text(payload, &["url"]))
        .map(str::to_owned)
}

fn callback_summary(provider: &str, event: &str, payload: &Map<String, Value>) -> Option<Value> {
    let mut summary = Map::new();
    let tool = callback_tool(payload);
    if let Some(tool) = tool.as_deref() {
        summary.insert("tool".into(), json!(summary_text(tool, 120, 256)));
        let tool = tool.to_ascii_lowercase();
        if is_shell_tool(&tool) {
            if let Some(command) = callback_command(payload) {
                summary.insert("command".into(), json!(summary_text(&command, 120, 480)));
            }
        } else if is_file_tool(&tool) {
            if let Some(path) = callback_file_path(payload) {
                summary.insert("file_path".into(), json!(summary_text(&path, 256, 512)));
            }
        } else if is_fetch_tool(&tool)
            && let Some(url) = callback_url(payload)
        {
            summary.insert(
                "url".into(),
                json!(summary_text(url_without_query(&url), 256, 512)),
            );
        }
    }
    if matches!(provider, "opencode" | "kilo")
        && matches!(event, "permissionasked" | "permissionaskedv2")
    {
        if let Some(permission) = callback_text(payload, &["permission"]) {
            summary.insert(
                "permission".into(),
                json!(summary_text(permission, 120, 256)),
            );
        }
        if let Some(patterns) = callback_value(payload, &["patterns"]).and_then(Value::as_array) {
            let patterns = patterns
                .iter()
                .filter_map(Value::as_str)
                .take(3)
                .map(|pattern| Value::String(summary_text(pattern, 120, 128)))
                .collect::<Vec<_>>();
            if !patterns.is_empty() {
                summary.insert("patterns".into(), Value::Array(patterns));
            }
        }
    }
    if summary.is_empty() {
        return None;
    }
    let summary = Value::Object(summary);
    (summary.to_string().len() <= 2048).then_some(summary)
}

fn is_shell_tool(tool: &str) -> bool {
    matches!(
        tool,
        "bash" | "sh" | "zsh" | "fish" | "pwsh" | "powershell" | "shell"
    ) || tool.contains("shell")
}

fn is_file_tool(tool: &str) -> bool {
    [
        "read", "write", "edit", "patch", "file", "glob", "grep", "search",
    ]
    .iter()
    .any(|name| tool.contains(name))
}

fn is_fetch_tool(tool: &str) -> bool {
    tool.contains("fetch") || tool.contains("http")
}

fn url_without_query(url: &str) -> &str {
    let end = [url.find('?'), url.find('#')]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(url.len());
    &url[..end]
}

struct Redactors {
    assignment: Regex,
    authorization: Regex,
    option_equals: Regex,
    option_value: Regex,
    short_option: Regex,
    bearer: Regex,
    openai_key: Regex,
    github_token: Regex,
}

fn redactors() -> &'static Redactors {
    static REDACTORS: OnceLock<Redactors> = OnceLock::new();
    REDACTORS.get_or_init(|| Redactors {
        assignment: Regex::new(r#"(?i)\b([a-z_][a-z0-9_]*)=(?:\"[^\"]*\"|'[^']*'|\S+)"#).expect("valid assignment redaction regex"),
        authorization: Regex::new(r"(?i)\bauthorization\s*:\s*[^\r\n]*").expect("valid authorization redaction regex"),
        option_equals: Regex::new(r"(?i)(--(?:password|passwd|token|secret|api[-_]?key|auth)=)\S+").expect("valid option redaction regex"),
        option_value: Regex::new(r#"(?i)(--(?:password|passwd|token|secret|api[-_]?key|auth)\s+)(?:\"[^\"]*\"|'[^']*'|\S+)"#).expect("valid option-value redaction regex"),
        short_option: Regex::new(r#"(?i)(^|\s)(-p\s+)(?:\"[^\"]*\"|'[^']*'|\S+)"#).expect("valid short-option redaction regex"),
        bearer: Regex::new(r"(?i)(\bbearer\s+)[a-z0-9._~+/=-]+").expect("valid bearer redaction regex"),
        openai_key: Regex::new(r"\bsk-[a-zA-Z0-9_-]+\b").expect("valid OpenAI-key redaction regex"),
        github_token: Regex::new(r"\bghp_[a-zA-Z0-9]+\b").expect("valid GitHub-token redaction regex"),
    })
}

fn redact_text(value: &str) -> String {
    let redactors = redactors();
    let value = redactors
        .authorization
        .replace_all(value, "Authorization: ***");
    let value = redactors
        .assignment
        .replace_all(&value, |captures: &regex::Captures<'_>| {
            let key = &captures[1];
            let sensitive = ["TOKEN", "SECRET", "PASSWORD", "PASSWD", "KEY", "AUTH"]
                .iter()
                .any(|needle| key.to_ascii_uppercase().contains(needle));
            if sensitive {
                format!("{key}=***")
            } else {
                captures[0].into()
            }
        });
    let value = redactors.option_equals.replace_all(&value, "$1***");
    let value = redactors.option_value.replace_all(&value, "$1***");
    let value = redactors.short_option.replace_all(&value, "$1$2***");
    let value = redactors.bearer.replace_all(&value, "$1***");
    let value = redactors.openai_key.replace_all(&value, "***");
    redactors
        .github_token
        .replace_all(&value, "***")
        .into_owned()
}

fn summary_text(value: &str, max_chars: usize, max_bytes: usize) -> String {
    let value: String = redact_text(value).chars().take(max_chars).collect();
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].into()
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
    let mut mapped = map_callback_event(target, action, payload)?;
    mapped.session_switch = session_switch(payload);
    Ok(mapped)
}

/// A deliberate session change announced by the provider: an explicit switch
/// or reset event, or a session start whose source is resume/clear. A child
/// provider process starting its own session reports `startup`; compaction
/// keeps the session, so a different ID with `compact` stays a conflict.
fn session_switch(payload: &Map<String, Value>) -> bool {
    let event = callback_event(payload)
        .map(|event| normalize_event(&event))
        .unwrap_or_default();
    let source = first_text(payload, &["source", "reason"]).map(str::to_ascii_lowercase);
    matches!(event.as_str(), "sessionswitch" | "onsessionreset")
        || matches!(
            source.as_deref(),
            Some("resume" | "clear" | "switch" | "fork" | "reset")
        )
}

fn map_callback_event(
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
                session_switch: false,
                event_only: false,
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
            session_switch: false,
            event_only: false,
        });
    }

    if target.capability == Capability::SessionOnly {
        let event = callback_event(payload).unwrap_or_default();
        if !event.is_empty() && !session_event_allowed(target.id, &event) {
            return Err(format!("unmapped {} callback event: {event}", target.id));
        }
        let event_only = event_only_callback(target.id, &event);
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
            session_switch: false,
            event_only,
        });
    }

    map_lifecycle(target.id, payload, session)
}

fn session_event_allowed(provider: &str, event: &str) -> bool {
    let normalized = normalize_event(event);
    match provider {
        "claude" => matches!(
            normalized.as_str(),
            "sessionstart" | "permissionrequest" | "notification" | "stop"
        ),
        "codex" => matches!(normalized.as_str(), "sessionstart" | "stop"),
        "copilot" | "droid" | "qodercli" | "qwen" => normalized == "sessionstart",
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

fn event_only_callback(provider: &str, event: &str) -> bool {
    matches!(
        (provider, normalize_event(event).as_str()),
        ("claude", "permissionrequest" | "notification" | "stop") | ("codex", "stop")
    )
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
                if callback_tool(payload).is_some_and(|tool| tool.eq_ignore_ascii_case("ask")) =>
            {
                Some("blocked")
            }
            "toolexecutionend"
                if callback_tool(payload).is_some_and(|tool| tool.eq_ignore_ascii_case("ask")) =>
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
                if callback_tool(payload)
                    .is_some_and(|tool| tool.eq_ignore_ascii_case("AskUserQuestion")) =>
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
        session_switch: false,
        event_only: false,
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
    let normalized = normalize_event(&event);
    let state = match normalized.as_str() {
        "chatmessage"
        | "toolexecutebefore"
        | "toolexecuteafter"
        | "permissionreplied"
        | "permissionrepliedv2"
        | "questionreplied"
        | "questionrepliedv2"
        | "questionrejected"
        | "questionrejectedv2"
        | "sessioncompacted" => Some("working"),
        "permissionasked" | "permissionaskedv2" | "questionasked" | "questionaskedv2"
        | "sessionerror" => Some("blocked"),
        "sessionidle" => Some("idle"),
        "sessioncreated" if provider == "kilo" => None,
        "sessionupdated" => None,
        "masilsessionselected" if root_binding => None,
        "sessionstatus" => match status_name(properties).as_deref() {
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
        session_switch: false,
        event_only: false,
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

    fn process(group: i32, provider: Option<&'static str>) -> Ancestor {
        Ancestor {
            group,
            provider,
            shell: false,
        }
    }

    fn shell(group: i32) -> Ancestor {
        Ancestor {
            shell: true,
            ..process(group, None)
        }
    }

    fn launcher(group: i32, provider: &'static str) -> Ancestor {
        process(group, Some(provider))
    }

    #[test]
    fn callbacks_from_the_panes_own_provider_are_accepted() {
        // hook <- sh (own session) <- claude <- zsh (pane).
        let claude = [shell(40), process(20, Some("claude")), process(20, None)];
        assert!(provenance(Some(&claude), "claude", 20).is_ok());
        // Codex embedded: hook <- sh -lc <- native codex <- node codex (pane).
        let codex = [shell(41), process(21, Some("codex")), launcher(21, "codex")];
        assert!(provenance(Some(&codex), "codex", 21).is_ok());
        // A shim that waits for its tool joins the tool's run.
        let shim = [
            shell(42),
            process(23, Some("codex")),
            process(23, Some("codex")),
        ];
        assert!(provenance(Some(&shim), "codex", 23).is_ok());
        // Nothing identified on the path: accepted as before the check.
        let unknown = [process(22, None), process(22, None)];
        assert!(provenance(Some(&unknown), "pi", 22).is_ok());
    }

    #[test]
    fn shared_daemons_nested_providers_and_outside_calls_are_refused() {
        // hook <- sh <- app-server daemon (own group, any name) <- TUI <- node.
        for daemon_name in [Some("codex"), None] {
            let daemon = [
                shell(50),
                process(45, daemon_name),
                process(21, Some("codex")),
                launcher(21, "codex"),
            ];
            let error = provenance(Some(&daemon), "codex", 21).unwrap_err();
            assert!(error.contains("shared provider daemon"), "{error}");
        }
        // hook <- sh <- claude -p <- bash <- codex (pane).
        let nested = [
            shell(60),
            process(21, Some("claude")),
            process(21, None),
            process(21, Some("codex")),
        ];
        assert!(
            provenance(Some(&nested), "codex", 21)
                .unwrap_err()
                .contains("nested")
        );
        // A shell that execs its last command leaves the providers adjacent.
        let exec_nested = [
            shell(61),
            process(21, Some("claude")),
            process(21, Some("codex")),
        ];
        assert!(
            provenance(Some(&exec_nested), "codex", 21)
                .unwrap_err()
                .contains("nested")
        );
        // The same provider separated by a shell is nested again.
        let respawned = [
            shell(62),
            process(21, Some("codex")),
            process(21, None),
            process(21, Some("codex")),
        ];
        assert!(
            provenance(Some(&respawned), "codex", 21)
                .unwrap_err()
                .contains("nested")
        );
        let other = [process(21, Some("opencode")), process(21, None)];
        assert!(
            provenance(Some(&other), "claude", 21)
                .unwrap_err()
                .contains("another")
        );
        assert!(
            provenance(None, "claude", 21)
                .unwrap_err()
                .contains("descend")
        );
        for refused in [&nested[..], &other[..], &respawned[..]] {
            let error = provenance(Some(refused), "codex", 21).unwrap_err();
            assert!(error.starts_with("identity_mismatch:"), "{error}");
        }
    }

    use super::super::inbox::Effect;
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

    fn effects(provider: &str, source: &str) -> Vec<Effect> {
        let payload = object(source);
        let mapped = map_callback(target(provider).unwrap(), None, &payload).unwrap();
        callback_effects(provider, "%1", "run-1", 7, &mapped, &payload)
    }

    fn action_effects(provider: &str, action: &str, source: &str) -> Vec<Effect> {
        let payload = object(source);
        let mapped = map_callback(target(provider).unwrap(), Some(action), &payload).unwrap();
        callback_effects(provider, "%1", "run-1", 7, &mapped, &payload)
    }

    #[test]
    fn open_code_and_kilo_requests_use_one_cross_kind_key() {
        let asked = effects(
            "opencode",
            r#"{"event":{"type":"permission.asked.v2","properties":{"id":"permission-1","sessionID":"session-1","permission":"bash","patterns":["src/**","Cargo.toml","README.md","one-more"],"tool":"Bash","command":"TOKEN=secret echo ok"}}}"#,
        );
        match asked.as_slice() {
            [
                Effect::Event {
                    source,
                    source_ref,
                    kind,
                    native_ref,
                    summary,
                    ..
                },
            ] => {
                assert_eq!(source, "hook:opencode");
                assert_eq!(source_ref, "request:permission-1");
                assert_eq!(*kind, "approval_requested");
                assert_eq!(native_ref.as_deref(), Some("permission-1"));
                assert_eq!(summary.as_ref().unwrap()["permission"], "bash");
                assert_eq!(
                    summary.as_ref().unwrap()["patterns"]
                        .as_array()
                        .unwrap()
                        .len(),
                    3
                );
                assert_eq!(summary.as_ref().unwrap()["command"], "TOKEN=*** echo ok");
            }
            other => panic!("unexpected effects: {other:?}"),
        }

        let replied = effects(
            "kilo",
            r#"{"event":{"type":"permission.replied","properties":{"requestID":"permission-1","reply":"allow"}}}"#,
        );
        assert_eq!(
            replied,
            vec![Effect::Resolve {
                source: "hook:kilo".into(),
                source_ref: "request:permission-1".into(),
                resolution: "replied",
            }]
        );

        let rejected = effects(
            "opencode",
            r#"{"event":{"type":"question.rejected.v2","properties":{"request_id":"question-1"}}}"#,
        );
        assert_eq!(
            rejected,
            vec![Effect::Resolve {
                source: "hook:opencode".into(),
                source_ref: "request:question-1".into(),
                resolution: "rejected",
            }]
        );

        let blocked = effects(
            "opencode",
            r#"{"event":{"type":"session.error","properties":{"sessionID":"session-1"}}}"#,
        );
        assert!(matches!(
            blocked.as_slice(),
            [Effect::Event {
                source_ref,
                kind: "blocked",
                summary: None,
                ..
            }] if source_ref == "blocked:run-1:7"
        ));
    }

    #[test]
    fn kimi_omp_and_mastracode_requests_and_resolutions_are_keyed() {
        let kimi = effects(
            "kimi",
            r#"{"hook_event_name":"PermissionRequest","toolCallId":"kimi-1","toolName":"Bash","toolInput":{"command":"--token hidden"},"turnId":"turn-1"}"#,
        );
        assert!(matches!(
            kimi.as_slice(),
            [Effect::Event {
                source_ref,
                kind: "approval_requested",
                native_ref: Some(native_ref),
                summary: Some(summary),
                ..
            }] if source_ref == "request:kimi-1"
                && native_ref == "kimi-1"
                && summary["command"] == "--token ***"
        ));
        assert_eq!(
            effects(
                "kimi",
                r#"{"hook_event_name":"PermissionResult","tool_call_id":"kimi-1","decision":"allow"}"#,
            ),
            vec![Effect::Resolve {
                source: "hook:kimi".into(),
                source_ref: "request:kimi-1".into(),
                resolution: "decided",
            }]
        );
        assert!(matches!(
            effects(
                "kimi",
                r#"{"hook_event_name":"PreToolUse","toolCallId":"kimi-question","toolName":"AskUserQuestion"}"#,
            )
            .as_slice(),
            [Effect::Event {
                source_ref,
                kind: "question_asked",
                ..
            }] if source_ref == "request:kimi-question"
        ));

        assert!(matches!(
            effects(
                "omp",
                r#"{"hook_event_name":"tool_approval_requested","toolCallId":"omp-1","toolName":"Bash"}"#,
            )
            .as_slice(),
            [Effect::Event {
                source_ref,
                kind: "approval_requested",
                ..
            }] if source_ref == "request:omp-1"
        ));
        assert_eq!(
            effects(
                "omp",
                r#"{"hook_event_name":"tool_approval_resolved","tool_call_id":"omp-1"}"#,
            ),
            vec![Effect::Resolve {
                source: "hook:omp".into(),
                source_ref: "request:omp-1".into(),
                resolution: "decided",
            }]
        );
        assert!(matches!(
            effects(
                "omp",
                r#"{"hook_event_name":"tool_execution_start","toolCallId":"omp-question","toolName":"ask"}"#,
            )
            .as_slice(),
            [Effect::Event {
                source_ref,
                kind: "question_asked",
                ..
            }] if source_ref == "request:omp-question"
        ));
        assert_eq!(
            effects(
                "omp",
                r#"{"hook_event_name":"tool_execution_end","toolCallId":"omp-question","toolName":"ask"}"#,
            ),
            vec![Effect::Resolve {
                source: "hook:omp".into(),
                source_ref: "request:omp-question".into(),
                resolution: "completed",
            }]
        );

        assert!(matches!(
            effects(
                "mastracode",
                r#"{"hook_event_name":"PermissionRequest","permission_id":"mastra-1"}"#,
            )
            .as_slice(),
            [Effect::Event {
                source_ref,
                kind: "approval_requested",
                ..
            }] if source_ref == "request:mastra-1"
        ));
        assert_eq!(
            effects(
                "mastracode",
                r#"{"hook_event_name":"PermissionResult","permission_id":"mastra-1"}"#,
            ),
            vec![Effect::Resolve {
                source: "hook:mastracode".into(),
                source_ref: "request:mastra-1".into(),
                resolution: "decided",
            }]
        );

        assert!(matches!(
            action_effects(
                "kimi",
                "blocked",
                r#"{"inputData":{"toolCallId":"kimi-exported-question","toolName":"AskUserQuestion"}}"#,
            )
            .as_slice(),
            [Effect::Event {
                source_ref,
                kind: "question_asked",
                ..
            }] if source_ref == "request:kimi-exported-question"
        ));
        assert_eq!(
            action_effects(
                "kimi",
                "working",
                r#"{"inputData":{"toolCallId":"kimi-exported-result","decision":"allow"}}"#,
            ),
            vec![Effect::Resolve {
                source: "hook:kimi".into(),
                source_ref: "request:kimi-exported-result".into(),
                resolution: "decided",
            }]
        );
    }

    #[test]
    fn claude_and_codex_event_only_callbacks_only_make_inbox_effects() {
        let claude_permission = object(
            r#"{"hook_event_name":"PermissionRequest","session_id":"session-1","prompt_id":"prompt-1","tool_name":"AskUserQuestion","tool_input":"private prompt"}"#,
        );
        let mapped = map_callback(target("claude").unwrap(), None, &claude_permission).unwrap();
        assert!(mapped.event_only);
        let permission_effects =
            callback_effects("claude", "%1", "run-1", 7, &mapped, &claude_permission);
        assert!(matches!(
            permission_effects.as_slice(),
            [Effect::Event {
                source_ref,
                kind: "approval_requested",
                summary: Some(summary),
                ..
            }] if source_ref == "approval_requested:run-1:7"
                && summary["tool"] == "AskUserQuestion"
                && summary.get("prompt_id").is_none()
        ));

        assert!(matches!(
            effects(
                "claude",
                r#"{"hook_event_name":"Notification","notification_type":"elicitation_dialog","message":"private question"}"#,
            )
            .as_slice(),
            [Effect::Event {
                kind: "question_asked",
                source_ref,
                ..
            }] if source_ref == "question_asked:run-1:7"
        ));
        assert!(effects(
            "claude",
            r#"{"hook_event_name":"Notification","notification_type":"unrelated","message":"ignored"}"#,
        )
        .is_empty());
        assert!(matches!(
            effects(
                "claude",
                r#"{"hook_event_name":"Stop","session_id":"session-1","prompt_id":"prompt-1","last_assistant_message":"private answer"}"#,
            )
            .as_slice(),
            [Effect::Event {
                kind: "turn_completed",
                source_ref,
                summary: None,
                ..
            }] if source_ref == "turn_completed:run-1:prompt-1"
        ));

        let codex_stop = object(
            r#"{"hook_event_name":"Stop","session_id":"session-1","turn_id":"turn-1","last_assistant_message":"private answer"}"#,
        );
        let mapped = map_callback(target("codex").unwrap(), None, &codex_stop).unwrap();
        assert!(mapped.event_only);
        assert!(matches!(
            callback_effects("codex", "%1", "run-1", 7, &mapped, &codex_stop).as_slice(),
            [Effect::Event {
                kind: "turn_completed",
                source_ref,
                summary: None,
                ..
            }] if source_ref == "turn_completed:run-1:turn-1"
        ));
    }

    #[test]
    fn summaries_redact_before_they_are_truncated() {
        let redacted = redact_text(
            "API_TOKEN=first --password=third --token fourth -p fifth sk-secret ghp_secret",
        );
        assert!(redacted.contains("API_TOKEN=***"));
        assert!(redacted.contains("--password=***"));
        assert!(redacted.contains("--token ***"));
        assert!(redacted.contains("-p ***"));
        assert!(!redacted.contains("first"));
        assert!(!redacted.contains("second"));
        assert!(!redacted.contains("third"));
        assert!(!redacted.contains("fourth"));
        assert!(!redacted.contains("fifth"));
        assert!(!redacted.contains("sk-secret"));
        assert!(!redacted.contains("ghp_secret"));
        assert_eq!(
            redact_text("Authorization: Bearer second"),
            "Authorization: ***"
        );
        assert_eq!(
            summary_text(&format!("TOKEN={}", "x".repeat(512)), 120, 480),
            "TOKEN=***"
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

        let claude =
            registration_fragment(target("claude").unwrap(), std::path::Path::new("/tmp/hook"));
        assert_eq!(
            claude["hooks"]["Notification"][0]["matcher"],
            "permission_prompt|elicitation_dialog"
        );
        assert_eq!(claude["hooks"]["Stop"][0]["hooks"][0]["timeout"], 10);
        let codex =
            registration_fragment(target("codex").unwrap(), std::path::Path::new("/tmp/hook"));
        assert!(codex["hooks"].get("Stop").is_some());
        let kimi =
            registration_fragment(target("kimi").unwrap(), std::path::Path::new("/tmp/hook"));
        assert!(
            kimi["content"]
                .as_str()
                .unwrap()
                .contains("event = \"PreToolUse\"\nmatcher = \"^AskUserQuestion$\"")
        );
    }
}
