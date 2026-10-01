//! Supported coding-agent commands and native session resume plans.

/// A supported coding-agent provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    pub id: &'static str,
    pub command: &'static str,
    pub aliases: &'static [&'static str],
    pub executables: &'static [&'static str],
}

const PROVIDERS: [Provider; 24] = [
    Provider {
        id: "pi",
        command: "pi",
        aliases: &[],
        executables: &["pi"],
    },
    Provider {
        id: "claude",
        command: "claude",
        aliases: &["claude-code"],
        executables: &["claude", "claude-code"],
    },
    Provider {
        id: "codex",
        command: "codex",
        aliases: &[],
        executables: &["codex"],
    },
    Provider {
        id: "gemini",
        command: "gemini",
        aliases: &[],
        executables: &["gemini"],
    },
    Provider {
        id: "cursor",
        command: if cfg!(windows) {
            "cursor-agent.cmd"
        } else {
            "cursor-agent"
        },
        aliases: &["cursor"],
        executables: &["cursor", "cursor-agent"],
    },
    Provider {
        id: "devin",
        command: "devin",
        aliases: &["devin-cli", "devin cli"],
        executables: &["devin", "devin-cli"],
    },
    Provider {
        id: "agy",
        command: "agy",
        aliases: &["antigravity", "antigravity-cli"],
        executables: &["agy", "antigravity", "antigravity-cli"],
    },
    Provider {
        id: "cline",
        command: "cline",
        aliases: &[".cline"],
        executables: &["cline", ".cline"],
    },
    Provider {
        id: "omp",
        command: "omp",
        aliases: &[],
        executables: &["omp"],
    },
    Provider {
        id: "mastracode",
        command: "mastracode",
        aliases: &["mastra-code", "mastra code"],
        executables: &["mastracode", "mastra-code"],
    },
    Provider {
        id: "opencode",
        command: "opencode",
        aliases: &["opencode2", "open-code"],
        executables: &["opencode", "opencode2", "open-code"],
    },
    Provider {
        id: "copilot",
        command: "copilot",
        aliases: &["github-copilot", "ghcs"],
        executables: &["copilot", "github-copilot", "ghcs"],
    },
    Provider {
        id: "kimi",
        command: "kimi",
        aliases: &["kimi-code", "kimi code"],
        executables: &["kimi", "kimi-code"],
    },
    Provider {
        id: "kiro",
        command: "kiro-cli",
        aliases: &["kiro"],
        executables: &["kiro", "kiro-cli"],
    },
    Provider {
        id: "droid",
        command: "droid",
        aliases: &[],
        executables: &["droid"],
    },
    Provider {
        id: "amp",
        command: "amp",
        aliases: &["amp-local"],
        executables: &["amp", "amp-local"],
    },
    Provider {
        id: "grok",
        command: "grok",
        aliases: &["grok-build"],
        executables: &["grok", "grok-build"],
    },
    Provider {
        id: "hermes",
        command: "hermes",
        aliases: &["hermes-agent"],
        executables: &["hermes", "hermes-agent"],
    },
    Provider {
        id: "kilo",
        command: "kilo",
        aliases: &["kilo-code", "kilo code"],
        executables: &["kilo", "kilo-code"],
    },
    Provider {
        id: "qodercli",
        command: "qodercli",
        aliases: &["qoderclicn", "qoder", "qodercn"],
        executables: &["qodercli", "qoderclicn", "qoder", "qodercn"],
    },
    Provider {
        id: "qwen",
        command: "qwen",
        aliases: &["qwen-code", "qwen code"],
        executables: &["qwen", "qwen-code"],
    },
    Provider {
        id: "letta",
        command: "letta",
        aliases: &["letta-code", "letta code"],
        executables: &["letta", "letta-code"],
    },
    Provider {
        id: "maki",
        command: "maki",
        aliases: &[],
        executables: &["maki"],
    },
    Provider {
        id: "muse",
        command: "muse",
        aliases: &["muse-code", "muse-cli"],
        executables: &["muse", "muse-code", "muse-cli"],
    },
];

pub fn all() -> &'static [Provider] {
    &PROVIDERS
}

pub fn find(label: &str) -> Option<&'static Provider> {
    let normalized = normalize_name(label);
    PROVIDERS.iter().find(|provider| {
        normalized == provider.id
            || normalized == normalize_name(provider.command)
            || provider.aliases.iter().any(|alias| normalized == *alias)
    })
}

/// Identifies an exact executable or a supported Node/Bun/Python wrapper.
///
/// This deliberately does not inspect arbitrary shell command text. A direct
/// executable must be the whole input; wrapper inputs are tokenized only to
/// locate their script argument.
pub fn identify(command: &str) -> Option<&'static Provider> {
    let argv = split_command(command)?;
    if argv.len() == 1 {
        return identify_executable(&argv[0]);
    }

    let runtime = normalize_name(&argv[0]);
    let script = if matches!(runtime.as_str(), "node" | "nodejs" | "bun") {
        if runtime == "node" && cursor_bundled_node(&argv) {
            return find("cursor");
        }
        runtime_script(&argv, &["-e", "--eval", "-p", "--print"], &[])?
    } else if is_python_runtime(&runtime) {
        runtime_script(&argv, &["-c"], &["-m"])?
    } else {
        return None;
    };
    identify_script(script)
}

/// Whether a Codex executable accepts `--no-daemon` (Codex 0.156 and later),
/// judged from its local `--help` output and cached by path, size and
/// modification time. A Codex started with it runs its session in the TUI
/// process, so provider hooks carry that pane's environment rather than the
/// shared daemon's. An unreadable or slow probe means no.
pub(crate) fn codex_no_daemon(executable: &std::path::Path) -> bool {
    use serde_json::{Value, json};
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let Ok(metadata) = std::fs::metadata(executable) else {
        return false;
    };
    let key = executable.to_string_lossy().into_owned();
    let stamp = json!({"size": metadata.size(), "mtime": metadata.mtime(), "mtime_ns": metadata.mtime_nsec()});
    let cache = crate::managed::state_base()
        .ok()
        .map(|base| base.join("masil/provider-probe.json"));
    let mut entries = cache
        .as_ref()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(entry) = entries.get(&key)
        && entry["stamp"] == stamp
    {
        return entry["codex_no_daemon"] == true;
    }
    let Ok(mut child) = Command::new(executable)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let Some(mut stdout) = child.stdout.take() else {
        return false;
    };
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = (&mut stdout).take(256 * 1024).read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    let finished = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let help = reader.join().unwrap_or_default();
    if !finished {
        return false;
    }
    let supported = help.contains("--no-daemon");
    entries.insert(key, json!({"stamp": stamp, "codex_no_daemon": supported}));
    if let Some(path) = cache
        && let Some(directory) = path.parent()
        && std::fs::create_dir_all(directory).is_ok()
    {
        let temporary = path.with_extension("json.tmp");
        if std::fs::write(&temporary, Value::Object(entries).to_string()).is_ok() {
            let _ = std::fs::rename(&temporary, &path);
        }
    }
    supported
}

/// Identifies the provider a running process belongs to from its argv. A
/// runtime (Node, Bun, Python) is judged by its script; any other program
/// by its executable name, whatever arguments follow, so a provider's own
/// helper processes such as `codex app-server` count as that provider.
pub(crate) fn identify_process(argv: &[String]) -> Option<&'static Provider> {
    let first = argv.first()?;
    let runtime = normalize_name(first);
    if matches!(runtime.as_str(), "node" | "nodejs" | "bun") {
        if runtime == "node" && cursor_bundled_node(argv) {
            return find("cursor");
        }
        return identify_script(runtime_script(
            argv,
            &["-e", "--eval", "-p", "--print"],
            &[],
        )?);
    }
    if is_python_runtime(&runtime) {
        return identify_script(runtime_script(argv, &["-c"], &["-m"])?);
    }
    // normalize_name trims the padding a process title rewrite leaves.
    identify_executable(first)
}

fn cursor_bundled_node(argv: &[String]) -> bool {
    let Some((runtime_parent, runtime_name)) = argv.first().and_then(|path| parent_and_name(path))
    else {
        return false;
    };
    let Some((script_parent, script_name)) = argv.get(1).and_then(|path| parent_and_name(path))
    else {
        return false;
    };
    if !runtime_name.eq_ignore_ascii_case("node.exe")
        || !script_name.eq_ignore_ascii_case("index.js")
        || !runtime_parent.eq_ignore_ascii_case(script_parent)
    {
        return false;
    }
    let mut tail = runtime_parent
        .rsplit(['/', '\\'])
        .filter(|component| !component.is_empty());
    matches!(
        (tail.next(), tail.next(), tail.next()),
        (Some(version), Some(versions), Some(package))
            if !version.trim().is_empty()
                && versions.eq_ignore_ascii_case("versions")
                && package.eq_ignore_ascii_case("cursor-agent")
    )
}

fn parent_and_name(path: &str) -> Option<(&str, &str)> {
    let split = path.rfind(['/', '\\'])?;
    let parent = path[..split].trim_end_matches(['/', '\\']);
    let name = &path[split + 1..];
    (!parent.is_empty() && !name.is_empty()).then_some((parent, name))
}

/// Builds the native argv for one of the 18 providers that expose session
/// resume in the herdr reference integrations.
pub fn resume(provider: &str, session: &str) -> Result<Vec<String>, String> {
    let provider = find(provider).ok_or_else(|| format!("unknown provider: {provider}"))?;
    validate_session_ref(provider.id, session)?;

    let args: Vec<String> = match provider.id {
        "claude" => vec!["claude".into(), "--resume".into(), session.into()],
        "codex" => vec!["codex".into(), "resume".into(), session.into()],
        "copilot" => vec!["copilot".into(), format!("--resume={session}")],
        "devin" => vec!["devin".into(), "--resume".into(), session.into()],
        "droid" => vec!["droid".into(), "--resume".into(), session.into()],
        "kimi" => vec!["kimi".into(), "--session".into(), session.into()],
        "mastracode" => vec!["mastracode".into(), "--thread".into(), session.into()],
        "pi" => vec!["pi".into(), "--session".into(), session.into()],
        "omp" => vec!["omp".into(), format!("--resume={session}")],
        "hermes" => vec!["hermes".into(), "--resume".into(), session.into()],
        "opencode" => vec!["opencode".into(), "--session".into(), session.into()],
        "qodercli" => vec!["qodercli".into(), "--resume".into(), session.into()],
        "qwen" => vec!["qwen".into(), "--resume".into(), session.into()],
        "kilo" => vec!["kilo".into(), "--session".into(), session.into()],
        "cursor" => vec![
            if cfg!(windows) {
                "cursor-agent.cmd"
            } else {
                "cursor-agent"
            }
            .into(),
            "--resume".into(),
            session.into(),
        ],
        "agy" => vec!["agy".into(), "--conversation".into(), session.into()],
        "grok" => vec!["grok".into(), "--resume".into(), session.into()],
        "letta" => {
            if let Some(agent_id) = session.strip_prefix("default:") {
                if agent_id.is_empty() || agent_id.starts_with('-') {
                    return Err(
                        "session reference must include a non-option agent after default:".into(),
                    );
                }
                vec![
                    "letta".into(),
                    "--conversation".into(),
                    "default".into(),
                    "--agent".into(),
                    agent_id.into(),
                ]
            } else {
                vec!["letta".into(), "--conversation".into(), session.into()]
            }
        }
        _ => {
            return Err(format!(
                "provider does not support native resume: {}",
                provider.id
            ));
        }
    };
    Ok(args)
}

fn validate_session_ref(provider: &str, session: &str) -> Result<(), String> {
    let max_len = if matches!(provider, "pi" | "omp") {
        4096
    } else {
        512
    };
    if session.is_empty() {
        return Err("session reference must not be empty".into());
    }
    if session.len() > max_len {
        return Err(format!("session reference exceeds {max_len} bytes"));
    }
    if session.starts_with('-') {
        return Err("session reference must not start with an option prefix".into());
    }
    if session.chars().any(char::is_control) {
        return Err("session reference must not contain control characters".into());
    }
    if session.contains('\'') {
        return Err("session reference must not contain apostrophes".into());
    }
    Ok(())
}

fn identify_executable(value: &str) -> Option<&'static Provider> {
    let normalized = normalize_name(value);
    PROVIDERS.iter().find(|provider| {
        provider
            .executables
            .iter()
            .any(|executable| normalized == *executable)
            || (provider.id == "muse"
                && normalized
                    .strip_prefix("muse-bin-")
                    .is_some_and(|version| version.starts_with(|ch: char| ch.is_ascii_digit())))
    })
}

fn identify_script(path: &str) -> Option<&'static Provider> {
    if let Some(provider) = identify_executable(path) {
        return Some(provider);
    }

    let components: Vec<String> = path
        .split(['/', '\\'])
        .filter(|component| !component.is_empty())
        .map(|component| component.to_ascii_lowercase())
        .collect();
    let ends_with = |suffix: &[&str]| {
        components.len() >= suffix.len()
            && components[components.len() - suffix.len()..]
                .iter()
                .zip(suffix)
                .all(|(actual, expected)| actual == expected)
    };

    if ends_with(&[
        "node_modules",
        "@earendil-works",
        "pi-coding-agent",
        "dist",
        "cli.js",
    ]) || ends_with(&[
        "node_modules",
        "@earendil-works",
        "pi-coding-agent",
        "dist",
        "bundle",
        "cli.js",
    ]) {
        return find("pi");
    }
    if ends_with(&[
        "node_modules",
        "@oh-my-pi",
        "pi-coding-agent",
        "dist",
        "cli.js",
    ]) {
        return find("omp");
    }
    if ends_with(&[
        "node_modules",
        "@moonshot-ai",
        "kimi-code",
        "dist",
        "main.mjs",
    ]) {
        return find("kimi");
    }
    if ends_with(&[
        "node_modules",
        "@qwen-code",
        "qwen-code",
        "dist",
        "index.js",
    ]) || ends_with(&["node_modules", "@qwen-code", "qwen-code", "dist", "index"])
    {
        return find("qwen");
    }
    if ends_with(&["node_modules", "mastracode", "dist", "cli.js"])
        || ends_with(&["node_modules", "mastracode", "dist", "cli"])
    {
        return find("mastracode");
    }
    if ends_with(&["node_modules", "@letta-ai", "letta-code", "letta.js"])
        || ends_with(&["node_modules", "@letta-ai", "letta-code", "letta"])
    {
        return find("letta");
    }
    None
}

fn runtime_script<'a>(argv: &'a [String], eval: &[&str], modules: &[&str]) -> Option<&'a str> {
    let mut index = 1;
    while let Some(arg) = argv.get(index) {
        if arg == "--" {
            return argv.get(index + 1).map(String::as_str);
        }
        if flag_matches(arg, eval) || flag_matches(arg, modules) {
            return None;
        }
        if arg.starts_with('-') {
            index += if option_takes_value(arg) { 2 } else { 1 };
            continue;
        }
        return Some(arg);
    }
    None
}

fn flag_matches(arg: &str, flags: &[&str]) -> bool {
    flags.iter().any(|flag| {
        arg == *flag
            || (!flag.starts_with("--") && arg.starts_with(flag) && arg.len() > flag.len())
            || (flag.starts_with("--")
                && arg
                    .strip_prefix(flag)
                    .is_some_and(|rest| rest.starts_with('=')))
    })
}

fn option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-r" | "--require"
            | "--loader"
            | "--import"
            | "--experimental-loader"
            | "--inspect-port"
            | "-W"
            | "-X"
            | "-S"
            | "-L"
            | "-o"
    )
}

pub(crate) fn is_runtime(value: &str) -> bool {
    matches!(value, "node" | "nodejs" | "bun" | "uv") || is_python_runtime(value)
}

fn is_python_runtime(value: &str) -> bool {
    value == "python"
        || value.strip_prefix("python").is_some_and(|version| {
            !version.is_empty()
                && version
                    .split('.')
                    .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        })
}

fn normalize_name(value: &str) -> String {
    let basename = value
        .trim()
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or(value)
        .to_ascii_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".ps1", ".js"] {
        if let Some(stripped) = basename.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    basename
}

fn split_command(command: &str) -> Option<Vec<String>> {
    if command.is_empty() || command.len() > 8192 || command.chars().any(char::is_control) {
        return None;
    }
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for ch in command.chars() {
        if let Some(open) = quote {
            if ch == open {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            ';' | '|' | '&' | '<' | '>' | '`' => return None,
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    argv.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        argv.push(current);
    }
    (!argv.is_empty()).then_some(argv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_matches_reference_provider_set() {
        assert_eq!(all().len(), 24);
        assert_eq!(find("claude-code").map(|p| p.id), Some("claude"));
        assert_eq!(find("qodercn").map(|p| p.id), Some("qodercli"));
        assert_eq!(find("muse-cli").map(|p| p.id), Some("muse"));
    }

    #[test]
    fn detection_accepts_exact_executables_and_known_wrappers_only() {
        assert_eq!(
            identify("/usr/local/bin/codex").map(|p| p.id),
            Some("codex")
        );
        assert_eq!(identify("muse-bin-0.2.1-R10").map(|p| p.id), Some("muse"));
        assert_eq!(
            identify("python3.12 /nix/store/pkg/bin/hermes --resume id").map(|p| p.id),
            Some("hermes")
        );
        assert_eq!(
            identify("node /tmp/node_modules/@qwen-code/qwen-code/dist/index.js").map(|p| p.id),
            Some("qwen")
        );
        assert_eq!(
            identify(r#"C:\Users\me\cursor-agent\versions\1.2.3\node.exe C:\Users\me\cursor-agent\versions\1.2.3\index.js"#)
                .map(|p| p.id),
            Some("cursor")
        );
        assert_eq!(identify("bash -c codex"), None);
        assert_eq!(identify("node -e codex /tmp/codex"), None);
        assert_eq!(identify("notes about codex"), None);
        assert_eq!(identify("codex transcript"), None);
        assert_eq!(identify("node /tmp/codex; rm -rf /tmp/x"), None);
    }

    #[test]
    fn native_resume_maps_reference_argv_and_rejects_injection() {
        let supported = [
            "claude",
            "codex",
            "copilot",
            "devin",
            "droid",
            "kimi",
            "mastracode",
            "pi",
            "omp",
            "hermes",
            "opencode",
            "qodercli",
            "qwen",
            "kilo",
            "cursor",
            "agy",
            "grok",
            "letta",
        ];
        assert_eq!(supported.len(), 18);
        for provider in supported {
            assert!(resume(provider, "abc").is_ok(), "{provider}");
        }
        assert_eq!(resume("codex", "abc").unwrap(), ["codex", "resume", "abc"]);
        assert_eq!(
            resume("copilot", "abc").unwrap(),
            ["copilot", "--resume=abc"]
        );
        assert_eq!(
            resume("letta", "default:agent-1").unwrap(),
            ["letta", "--conversation", "default", "--agent", "agent-1"]
        );
        assert!(resume("amp", "abc").is_err());
        assert!(resume("codex", "--help").is_err());
        assert!(resume("codex", "abc\nother").is_err());
        assert!(resume("codex", "abc' other").is_err());
        assert!(resume("letta", "default:--help").is_err());
    }
}
