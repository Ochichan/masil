//! Which CLI verbs a scope may call (docs/extensions.md). Anything not
//! listed is refused. Nothing that can answer an approval or a question is
//! in any scope: no `answer`, no `send-keys` (an Enter on an approval
//! screen), no provider arguments to `start` (a flag that skips approvals),
//! no text that begins with `!` or `/` (providers run such lines as shell or
//! slash commands, outside their approvals) or is blank (a queued item's
//! paths would begin it). Verbs that could hide what
//! needs a person are `admin`. This prevents accidents, not attacks: a
//! process of the same user has a shell.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Scope {
    Read,
    Act,
    Admin,
}

impl Scope {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        match text {
            "read" => Ok(Self::Read),
            "act" => Ok(Self::Act),
            "admin" => Ok(Self::Admin),
            _ => Err(format!(
                "invalid_argument: scope is read, act or admin, not {text}"
            )),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Act => "act",
            Self::Admin => "admin",
        }
    }
}

/// Characters a provider may drop before it looks for a command: spaces,
/// and invisible format characters such as a byte order mark.
pub(crate) fn invisible(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// Whether text would begin, once spaces and invisible characters are
/// skipped, with `!` or `/`: what providers run as commands.
pub(crate) fn begins_like_command(text: &str) -> bool {
    matches!(text.chars().find(|c| !invisible(*c)), Some('!' | '/'))
}

/// Text a provider reads as a prompt: not blank (a queued item's paths
/// would come first), not what it would run as a command, and no control
/// keys but newline and tab (a draft is pasted later as it is).
fn prompt_text(text: &str) -> bool {
    !matches!(
        text.chars().find(|c| !invisible(*c)),
        None | Some('!' | '/')
    ) && !text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
}

/// The words of `args` that are not `--revision N`, in order, as the queue
/// command reads them.
fn without_revision(args: &[String]) -> Vec<&str> {
    let mut words = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--revision" {
            index += 2;
        } else {
            words.push(args[index].as_str());
            index += 1;
        }
    }
    words
}

/// The scope a call needs; None when no scope may make it.
pub(crate) fn needed(verb: &str, args: &[String]) -> Option<Scope> {
    use Scope::{Act, Admin, Read};
    let word = |at: usize| args.get(at).map(String::as_str);
    Some(match verb {
        "list" => match args {
            [] => Read,
            [json] if json == "--json" => Read,
            // Every endpoint, over SSH.
            [all] if all == "--all" => Admin,
            _ => return None,
        },
        "get" | "explain" | "read" | "find" | "capabilities" | "providers" | "prompt-receipt"
        | "wait" | "requests" => Read,
        "view" => match word(0) {
            Some("get") | None => Read,
            Some("set" | "clear") => Admin,
            _ => return None,
        },
        "operations" => match word(0) {
            None | Some("list" | "status" | "--all" | "--limit") => Read,
            Some("reconcile" | "adopt") => Admin,
            _ => return None,
        },
        "operation" => match word(0) {
            Some("resolve") => Admin,
            Some(word) if !word.starts_with('-') => Read,
            _ => return None,
        },
        "inbox" => match word(0) {
            None | Some("list" | "--all" | "--limit" | "status") => Read,
            Some("ack" | "read-all" | "enable" | "disable") => Admin,
            _ => return None,
        },
        "queue" => {
            let words = without_revision(args);
            match words.as_slice() {
                ["--held"] => Read,
                ["--held", "remove", _] => Act,
                [target] if !target.starts_with('-') => Read,
                [_, "show", _] => Read,
                [_, "add", "--from", _] => Act,
                [_, "add", text, ..] if !text.starts_with('-') && prompt_text(text) => Act,
                [_, "edit", _, text] if prompt_text(text) => Act,
                [_, "attach" | "detach" | "move", _, _] => Act,
                [_, "remove", _] => Act,
                [_, "send"] | [_, "send", _] => Act,
                _ => return None,
            }
        }
        "changes" => {
            if args.iter().any(|arg| arg == "--handoff") {
                Act
            } else {
                Read
            }
        }
        "checkpoint" => match word(1) {
            Some("list" | "show") => Read,
            Some("make") => Act,
            Some("restore") => Admin,
            _ => return None,
        },
        "worktree" => match word(0) {
            Some("list" | "jobs") => Read,
            _ => return None,
        },
        "notify" => match word(0) {
            Some("status") => Read,
            Some("enable" | "disable" | "test") => Admin,
            _ => return None,
        },
        "schedule" => match word(0) {
            Some("list" | "runs") => Read,
            Some("add" | "enable" | "disable" | "remove") => Admin,
            _ => return None,
        },
        "endpoints" | "endpoint" => match word(0) {
            None | Some("status" | "list") => Read,
            Some("connect" | "disconnect" | "add" | "remove" | "enable" | "disable") => Admin,
            _ => return None,
        },
        "coordinator" => match word(0) {
            Some("status") => Read,
            Some("start" | "stop") => Admin,
            _ => return None,
        },
        "integration" => match word(0) {
            Some("status") => Read,
            Some("export") => Admin,
            _ => return None,
        },
        // No provider arguments (`--` and after): a flag can skip approvals.
        "start" if !args.iter().any(|arg| arg == "--") => Act,
        "prompt" | "draft" => match word(1) {
            Some(text) if prompt_text(text) => Act,
            _ => return None,
        },
        "close" | "resume" | "rename" | "attach" => Act,
        // A key on an approval or question screen declines it. Overrides
        // of the screen check and of the keys are a person's (CLI only).
        "interrupt" => {
            let mut index = 1;
            while index < args.len() {
                let value = args.get(index + 1).filter(|value| !value.starts_with('-'));
                match (args[index].as_str(), value) {
                    ("--run" | "--operation", Some(_)) => index += 2,
                    // Within the API's own time limit for a call.
                    ("--confirm-seconds", Some(seconds))
                        if seconds.parse::<u64>().is_ok_and(|seconds| seconds <= 30) =>
                    {
                        index += 2
                    }
                    _ => return None,
                }
            }
            match word(0) {
                Some(target) if !target.starts_with('-') => Admin,
                _ => return None,
            }
        }
        // Starting one turns on this machine's microphone: no scope.
        "dictate" => match args {
            [status] if status == "--status" => Read,
            [stop] if stop == "--stop" || stop == "--cancel" => Act,
            _ => return None,
        },
        "ack" | "save" | "restore" | "reload" => Admin,
        // `answer`, `send-keys`, `start ... --`, interactive and internal
        // verbs: no scope.
        _ => return None,
    })
}

/// Whether `scope` may call `verb` with `args`.
pub(crate) fn allowed(scope: Scope, verb: &str, args: &[String]) -> Result<(), String> {
    match needed(verb, args) {
        Some(needed) if needed <= scope => Ok(()),
        Some(needed) => Err(format!(
            "{verb} needs the {} scope; this connection has {}",
            needed.name(),
            scope.name()
        )),
        None => Err(format!(
            "{verb} with these arguments is not offered through the API"
        )),
    }
}

/// Every verb some scope may call, for the hello.
const VERBS: [&str; 37] = [
    "list",
    "get",
    "explain",
    "read",
    "find",
    "capabilities",
    "providers",
    "prompt-receipt",
    "wait",
    "requests",
    "view",
    "operations",
    "operation",
    "inbox",
    "queue",
    "changes",
    "checkpoint",
    "worktree",
    "notify",
    "schedule",
    "endpoints",
    "endpoint",
    "coordinator",
    "integration",
    "start",
    "prompt",
    "draft",
    "interrupt",
    "close",
    "resume",
    "rename",
    "attach",
    "ack",
    "save",
    "restore",
    "reload",
    "dictate",
];

/// The verbs a scope may call in some form.
pub(crate) fn methods(scope: Scope) -> Vec<&'static str> {
    let forms: [&[&str]; 8] = [
        &[],
        &["x"],
        &["get"],
        &["list"],
        &["status"],
        &["x", "list"],
        &["x", "text"],
        &["x", "make"],
    ];
    let mut methods: Vec<&'static str> = VERBS
        .into_iter()
        .filter(|verb| {
            forms.iter().any(|form| {
                let args: Vec<String> = form.iter().map(|word| (*word).to_owned()).collect();
                needed(verb, &args).is_some_and(|needed| needed <= scope)
            })
        })
        .collect();
    methods.extend(["subscribe", "unsubscribe"]);
    methods
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn nothing_can_answer_for_a_person() {
        for (verb, words) in [
            ("answer", &["builder", "per_1", "--choice", "once"][..]),
            ("send-keys", &["builder", "Enter"][..]),
            ("send-keys", &["builder", "y"][..]),
            ("start", &["x", "claude", "--", "--skip-permissions"][..]),
            ("prompt", &["builder", "!rm -rf /"][..]),
            ("prompt", &["builder", "  /approve"][..]),
            ("draft", &["builder", "!ls"][..]),
            ("queue", &["builder", "add", "!ls"][..]),
            ("queue", &["builder", "edit", "3", "/model"][..]),
            (
                "queue",
                &["--revision", "3", "builder", "edit", "3", "!x"][..],
            ),
            // A blank body would send the paths first.
            (
                "queue",
                &["builder", "add", " ", "--attach", "/etc/hosts"][..],
            ),
            ("queue", &["builder", "edit", "3", ""][..]),
            ("prompt", &["builder", "\n"][..]),
            ("prompt", &["builder", "\u{FEFF}/model"][..]),
            ("prompt", &["builder", "\u{200B} !ls"][..]),
            ("draft", &["builder", "hi\u{1b}[201~\r!ls"][..]),
            ("rpc", &[][..]),
            ("ui", &[][..]),
            ("focus", &["builder"][..]),
            ("put", &["file"][..]),
            ("dictate", &["builder"][..]),
            ("interrupt", &["builder", "--any-state"][..]),
            ("interrupt", &["builder", "--key", "C-c"][..]),
            (
                "interrupt",
                &["builder", "--run", "r", "--operation", "--any-state"][..],
            ),
            ("interrupt", &["builder", "--confirm-seconds", "60"][..]),
            ("dictate", &["--toggle", "builder"][..]),
        ] {
            assert_eq!(needed(verb, &args(words)), None, "{verb} {words:?}");
        }
    }

    #[test]
    fn scopes_follow_the_table() {
        let check = |verb: &str, words: &[&str], scope: Scope| {
            assert_eq!(needed(verb, &args(words)), Some(scope), "{verb} {words:?}");
        };
        check("list", &[], Scope::Read);
        check("list", &["--all"], Scope::Admin);
        check("queue", &["builder"], Scope::Read);
        check(
            "queue",
            &["builder", "add", "look at the tests"],
            Scope::Act,
        );
        check("queue", &["builder", "send", "--revision", "4"], Scope::Act);
        check("queue", &["--revision", "4", "builder", "send"], Scope::Act);
        check("inbox", &["ack", "3"], Scope::Admin);
        check("ack", &["b", "--run", "r", "--revision", "1"], Scope::Admin);
        check("view", &["set", "--state", "idle"], Scope::Admin);
        check("prompt", &["builder", "fix the build"], Scope::Act);
        check("prompt", &["builder", "line one\n\tline two"], Scope::Act);
        check("interrupt", &["builder"], Scope::Admin);
        check(
            "interrupt",
            &["builder", "--run", "r", "--operation", "k"],
            Scope::Admin,
        );
        check("dictate", &["--status"], Scope::Read);
        check("dictate", &["--stop"], Scope::Act);
        check("dictate", &["--cancel"], Scope::Act);
        check("start", &["x", "codex", "--cwd", "/tmp"], Scope::Act);
        check(
            "checkpoint",
            &["builder", "restore", "1", "a"],
            Scope::Admin,
        );
        assert!(allowed(Scope::Read, "prompt", &args(&["b", "hi"])).is_err());
        assert!(allowed(Scope::Admin, "send-keys", &args(&["b", "x"])).is_err());
        assert!(methods(Scope::Read).contains(&"list"));
        assert!(!methods(Scope::Read).contains(&"prompt"));
        assert!(methods(Scope::Act).contains(&"prompt"));
    }

    /// Every verb the CLI has is either in the table or refused on purpose.
    #[test]
    fn every_cli_verb_is_classified() {
        let refused = [
            "help",
            "answer",
            "send-keys",
            "rpc",
            "endpoint-connect",
            "exec-managed",
            "open-native",
            "report",
            "ui",
            "sidebar",
            "focus",
            "receive",
            "put",
        ];
        for verb in crate::managed::verb_names() {
            assert!(
                VERBS.contains(&verb) || refused.contains(&verb),
                "{verb} is neither offered nor refused"
            );
        }
    }
}
