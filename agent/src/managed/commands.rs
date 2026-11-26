//! The managed CLI's public and internal verb catalog.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Verb {
    pub(crate) name: &'static str,
    pub(crate) usage: &'static str,
    pub(crate) remote: bool,
}

pub(crate) const VERBS: &[Verb] = &[
    Verb {
        name: "help",
        usage: "",
        remote: false,
    },
    Verb {
        name: "providers",
        usage: "masil-agent agent [--socket MASIL_SOCKET] [--client CLIENT] providers",
        remote: false,
    },
    Verb {
        name: "endpoints",
        usage: "masil-agent agent endpoints list|add|remove|enable|disable ... | [--socket MASIL_SOCKET] endpoints connect ID | disconnect ID | status [ID]",
        remote: false,
    },
    Verb {
        name: "endpoint",
        usage: "masil-agent agent --endpoint ID COMMAND ...",
        remote: true,
    },
    Verb {
        name: "list",
        usage: "masil-agent agent [--socket MASIL_SOCKET] list --all",
        remote: false,
    },
    Verb {
        name: "list",
        usage: "masil-agent agent [--socket MASIL_SOCKET] [--client NAME] list [--client-view]|get TARGET|explain TARGET",
        remote: true,
    },
    Verb {
        name: "get",
        usage: "masil-agent agent [--socket MASIL_SOCKET] [--client NAME] list [--client-view]|get TARGET|explain TARGET",
        remote: true,
    },
    Verb {
        name: "explain",
        usage: "masil-agent agent [--socket MASIL_SOCKET] [--client NAME] list [--client-view]|get TARGET|explain TARGET",
        remote: true,
    },
    Verb {
        name: "find",
        usage: "masil-agent agent [--socket MASIL_SOCKET] find QUERY [--limit N]",
        remote: true,
    },
    Verb {
        name: "capabilities",
        usage: "masil-agent agent [--socket MASIL_SOCKET] capabilities TARGET",
        remote: true,
    },
    Verb {
        name: "capabilities",
        usage: "masil-agent agent capabilities --provider PROVIDER",
        remote: false,
    },
    Verb {
        name: "read",
        usage: "masil-agent agent [--socket MASIL_SOCKET] read TARGET [--history]",
        remote: true,
    },
    Verb {
        name: "start",
        usage: "masil-agent agent [--socket MASIL_SOCKET] start NAME PROVIDER [--cwd DIR] [--worktree NAME|PATH (local only)] [--sandbox worktree|strict|net-off | --no-sandbox] [--split %N] [--session ID] [--answers] [--boot BOOT --operation ID] [-- ARGS...]",
        remote: true,
    },
    Verb {
        name: "attach",
        usage: "masil-agent agent [--socket MASIL_SOCKET] attach TARGET NAME",
        remote: true,
    },
    Verb {
        name: "rename",
        usage: "masil-agent agent [--socket MASIL_SOCKET] rename TARGET NAME",
        remote: true,
    },
    Verb {
        name: "focus",
        usage: "masil-agent agent [--socket MASIL_SOCKET] focus TARGET",
        remote: true,
    },
    Verb {
        name: "send-keys",
        usage: "masil-agent agent [--socket MASIL_SOCKET] send-keys TARGET KEY...",
        remote: true,
    },
    Verb {
        name: "draft",
        usage: "masil-agent agent [--socket MASIL_SOCKET] draft TARGET TEXT",
        remote: true,
    },
    Verb {
        name: "prompt",
        usage: "masil-agent agent [--socket MASIL_SOCKET] prompt TARGET TEXT [--run RUN --operation N]",
        remote: true,
    },
    Verb {
        name: "prompt-receipt",
        usage: "masil-agent agent [--socket MASIL_SOCKET] prompt-receipt TARGET [--run RUN] [--operation N]",
        remote: true,
    },
    Verb {
        name: "interrupt",
        usage: "masil-agent agent [--socket MASIL_SOCKET] interrupt TARGET [--run RUN --operation ID] [--confirm-seconds N] [--any-state] [--key KEY]...",
        remote: true,
    },
    Verb {
        name: "close",
        usage: "masil-agent agent [--socket MASIL_SOCKET] close TARGET [--run RUN --operation ID]",
        remote: true,
    },
    Verb {
        name: "wait",
        usage: "masil-agent agent [--socket MASIL_SOCKET] wait TARGET --state idle|working|blocked|exited|unknown [--after-change] | wait --operation KEY --stage STAGE | wait --run RUN --ended | wait --pane %N|--window @N|--session NAME --closed [--timeout SECONDS]",
        remote: true,
    },
    Verb {
        name: "resume",
        usage: "masil-agent agent [--socket MASIL_SOCKET] resume TARGET --name NAME [--split %N] [--boot BOOT --operation ID]",
        remote: true,
    },
    Verb {
        name: "dictate",
        usage: "masil-agent agent [--socket MASIL_SOCKET] [--client CLIENT] dictate TARGET | dictate --toggle TARGET | dictate --stop | dictate --cancel | dictate --status",
        remote: false,
    },
    Verb {
        name: "requests",
        usage: "masil-agent agent [--socket MASIL_SOCKET] requests TARGET",
        remote: true,
    },
    Verb {
        name: "answer",
        usage: "masil-agent agent [--socket MASIL_SOCKET] answer TARGET REQUEST --choice once|always | --choice reject [--message TEXT] | --answer TEXT... | --reject",
        remote: true,
    },
    Verb {
        name: "queue",
        usage: "masil-agent agent [--socket MASIL_SOCKET] queue TARGET [show ID | add TEXT [--attach PATH]... | add --from ID | edit ID TEXT | attach ID PATH | detach ID N | move ID POSITION | remove ID | send [ID]] [--revision N (edit|attach|detach|move|remove|send only)] | queue --held [remove ID]",
        remote: true,
    },
    Verb {
        name: "changes",
        usage: "masil-agent agent [--socket MASIL_SOCKET] changes TARGET [--file PATH | --handoff REVIEWER]",
        remote: true,
    },
    Verb {
        name: "checkpoint",
        usage: "masil-agent agent [--socket MASIL_SOCKET] checkpoint TARGET make [--reason TEXT] | list | show ID [--file PATH] | restore ID PATH... [--confirm TOKEN]",
        remote: false,
    },
    Verb {
        name: "audit",
        usage: "masil-agent agent [--socket MASIL_SOCKET] audit TARGET [--turn latest|ID] [--history] [--challenge FINDING_ID] [--json]",
        remote: false,
    },
    Verb {
        name: "guard",
        usage: "masil-agent agent [--socket MASIL_SOCKET] guard TARGET [status | extend [--turns N] | dismiss]",
        remote: false,
    },
    Verb {
        name: "dvr",
        usage: "masil-agent agent [--socket MASIL_SOCKET] dvr list|inspect|snapshot|pin|unpin|restore|fork TARGET [--turn N] [--note TEXT] [--branch NAME] [--json]",
        remote: false,
    },
    Verb {
        name: "trace",
        usage: "masil-agent agent [--socket MASIL_SOCKET] trace export TARGET --out FILE [--redact-strict]",
        remote: false,
    },
    Verb {
        name: "inbox",
        usage: "masil-agent agent [--socket MASIL_SOCKET] inbox [list] [--all] [--limit N] | ack ID... | read-all [--through SEQ] | enable | disable | status",
        remote: true,
    },
    Verb {
        name: "notify",
        usage: "masil-agent agent [--socket MASIL_SOCKET] notify enable | disable | status | test",
        remote: false,
    },
    Verb {
        name: "worktree",
        usage: "masil-agent agent [--socket MASIL_SOCKET | --endpoint ID] worktree list [REPO] [--all] | jobs [ID]",
        remote: true,
    },
    Verb {
        name: "receive",
        usage: "",
        remote: false,
    },
    Verb {
        name: "put",
        usage: "masil-agent agent --endpoint ID put LOCAL_FILE (--dir REMOTE_DIR [--name NAME] | --attachment TARGET)",
        remote: true,
    },
    Verb {
        name: "schedule",
        usage: "masil-agent agent [--socket MASIL_SOCKET] schedule add NAME (--cron \"M H DOM MON DOW\" | --every 30m) (--layout TEMPLATE [--session S] [--yes] | --start AGENT PROVIDER --cwd DIR [--worktree W] | --queue AGENT --template TEMPLATE) | list | runs NAME [--limit N] | enable NAME | disable NAME | remove NAME",
        remote: false,
    },
    Verb {
        name: "operations",
        usage: "masil-agent agent [--socket MASIL_SOCKET] operations [list] [--all] [--limit N] | status | reconcile | adopt",
        remote: true,
    },
    Verb {
        name: "operation",
        usage: "masil-agent agent [--socket MASIL_SOCKET] operation KEY | operation resolve KEY --as delivered|not-delivered",
        remote: true,
    },
    Verb {
        name: "ack",
        usage: "masil-agent agent [--socket MASIL_SOCKET] ack TARGET --run RUN --revision REVISION",
        remote: true,
    },
    Verb {
        name: "save",
        usage: "masil-agent agent [--socket MASIL_SOCKET] save FILE",
        remote: true,
    },
    Verb {
        name: "restore",
        usage: "masil-agent agent [--socket MASIL_SOCKET] restore FILE [--allow-fresh] [--again (local only)] [--wait SECONDS (local only)]",
        remote: true,
    },
    Verb {
        name: "view",
        usage: "masil-agent agent [--socket MASIL_SOCKET] [--client NAME] view get|clear|set [--provider ID] [--state STATE] [--workspace NAME] [--sort priority|name|provider|workspace] [--client-view]",
        remote: true,
    },
    Verb {
        name: "integration",
        usage: "masil-agent agent [--socket MASIL_SOCKET] integration status [PROVIDER]",
        remote: false,
    },
    Verb {
        name: "integration",
        usage: "masil-agent agent [--socket MASIL_SOCKET] integration export PROVIDER [--directory ABSOLUTE_PATH]",
        remote: false,
    },
    Verb {
        name: "reload",
        usage: "masil-agent agent [--socket MASIL_SOCKET] reload",
        remote: false,
    },
    Verb {
        name: "coordinator",
        usage: "masil-agent agent [--socket MASIL_SOCKET] coordinator status|start|stop",
        remote: false,
    },
    Verb {
        name: "report",
        usage: "masil-agent agent [--socket MASIL_SOCKET] report --pane %N --run RUN --sequence N --state STATE [--session ID]",
        remote: false,
    },
    Verb {
        name: "ui",
        usage: "masil-agent agent [--socket MASIL_SOCKET] ui|sidebar [--lang en|ko] [--theme dark|light|terminal]",
        remote: false,
    },
    Verb {
        name: "sidebar",
        usage: "masil-agent agent [--socket MASIL_SOCKET] ui|sidebar [--lang en|ko] [--theme dark|light|terminal]",
        remote: false,
    },
    Verb {
        name: "rpc",
        usage: "",
        remote: false,
    },
    Verb {
        name: "endpoint-connect",
        usage: "",
        remote: false,
    },
    Verb {
        name: "exec-managed",
        usage: "",
        remote: false,
    },
    Verb {
        name: "open-native",
        usage: "",
        remote: false,
    },
];

pub(crate) fn verb(name: &str) -> Option<&'static Verb> {
    VERBS.iter().find(|verb| verb.name == name)
}

pub(crate) fn supports_remote(name: &str) -> bool {
    VERBS.iter().any(|verb| verb.name == name && verb.remote)
}

pub(crate) fn help() -> String {
    let mut lines = Vec::new();
    for verb in VERBS {
        if !verb.usage.is_empty() && !lines.contains(&verb.usage) {
            lines.push(verb.usage);
        }
    }
    format!(
        "Native agents (no observer configuration needed):\n{}\n\n{}",
        lines
            .iter()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
        HELP_TRAILER
    )
}

const HELP_TRAILER: &str = "Socket defaults to the current TMUX server. Start creates a new window or split.\nDraft prepares a tmux buffer. Prompt checks idle/foreground identity, pastes, then sends Enter.\nPrompt receipts report delivery, not provider acceptance. Reuse --run RUN --operation N for retries.\nStart, prompt, interrupt and close are recorded in a durable operation store before any effect;\na retry with the same key returns the recorded result and an unknown outcome is never resent.\nRaw send-keys is explicit keyboard delivery, never provider acceptance.\nWait reports an observed state, never task success. A session binding is verified only\nwhen a provider callback carrying this run's identity reported it; requested resume refs stay unverified.";
