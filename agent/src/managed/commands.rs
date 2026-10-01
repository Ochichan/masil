//! The managed CLI's public and internal verb catalog.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Verb {
    pub(crate) name: &'static str,
    pub(crate) usage: &'static str,
    pub(crate) mutating: bool,
    pub(crate) target_stage: Option<&'static str>,
    pub(crate) remote: bool,
}

pub(crate) const VERBS: &[Verb] = &[
    Verb {
        name: "help",
        usage: "",
        mutating: false,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "providers",
        usage: "masil-agent agent [--socket MASIL_SOCKET] [--client CLIENT] providers",
        mutating: false,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "endpoints",
        usage: "masil-agent agent endpoints list|add|remove|enable|disable ...",
        mutating: true,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "endpoint",
        usage: "masil-agent agent --endpoint ID COMMAND ...",
        mutating: true,
        target_stage: None,
        remote: true,
    },
    Verb {
        name: "list",
        usage: "masil-agent agent [--socket MASIL_SOCKET] list --all",
        mutating: false,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "list",
        usage: "masil-agent agent [--socket MASIL_SOCKET] list|get TARGET|explain TARGET",
        mutating: false,
        target_stage: None,
        remote: true,
    },
    Verb {
        name: "get",
        usage: "masil-agent agent [--socket MASIL_SOCKET] list|get TARGET|explain TARGET",
        mutating: false,
        target_stage: None,
        remote: true,
    },
    Verb {
        name: "explain",
        usage: "masil-agent agent [--socket MASIL_SOCKET] list|get TARGET|explain TARGET",
        mutating: false,
        target_stage: None,
        remote: true,
    },
    Verb {
        name: "find",
        usage: "masil-agent agent [--socket MASIL_SOCKET] find QUERY [--limit N]",
        mutating: false,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "capabilities",
        usage: "masil-agent agent [--socket MASIL_SOCKET] capabilities TARGET",
        mutating: false,
        target_stage: None,
        remote: true,
    },
    Verb {
        name: "capabilities",
        usage: "masil-agent agent capabilities --provider PROVIDER",
        mutating: false,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "read",
        usage: "masil-agent agent [--socket MASIL_SOCKET] read TARGET [--history]",
        mutating: false,
        target_stage: None,
        remote: true,
    },
    Verb {
        name: "start",
        usage: "masil-agent agent [--socket MASIL_SOCKET] start NAME PROVIDER --cwd DIR [--split %N] [--session ID] [--boot BOOT --operation ID] [-- ARGS...]",
        mutating: true,
        target_stage: Some("process_started"),
        remote: true,
    },
    Verb {
        name: "attach",
        usage: "masil-agent agent [--socket MASIL_SOCKET] attach TARGET NAME",
        mutating: true,
        target_stage: Some("registered"),
        remote: true,
    },
    Verb {
        name: "rename",
        usage: "masil-agent agent [--socket MASIL_SOCKET] rename TARGET NAME",
        mutating: true,
        target_stage: Some("registered"),
        remote: true,
    },
    Verb {
        name: "focus",
        usage: "masil-agent agent [--socket MASIL_SOCKET] focus TARGET",
        mutating: true,
        target_stage: Some("selected"),
        remote: true,
    },
    Verb {
        name: "send-keys",
        usage: "masil-agent agent [--socket MASIL_SOCKET] send-keys TARGET KEY...",
        mutating: true,
        target_stage: Some("keys_delivered"),
        remote: true,
    },
    Verb {
        name: "draft",
        usage: "masil-agent agent [--socket MASIL_SOCKET] draft TARGET TEXT",
        mutating: true,
        target_stage: Some("draft_prepared"),
        remote: true,
    },
    Verb {
        name: "prompt",
        usage: "masil-agent agent [--socket MASIL_SOCKET] prompt TARGET TEXT [--run RUN --operation N]",
        mutating: true,
        target_stage: Some("delivered"),
        remote: true,
    },
    Verb {
        name: "prompt-receipt",
        usage: "masil-agent agent [--socket MASIL_SOCKET] prompt-receipt TARGET [--run RUN] [--operation N]",
        mutating: false,
        target_stage: Some("delivered"),
        remote: true,
    },
    Verb {
        name: "interrupt",
        usage: "masil-agent agent [--socket MASIL_SOCKET] interrupt|close TARGET [--run RUN --operation ID]",
        mutating: true,
        target_stage: Some("interrupt_key_delivered"),
        remote: true,
    },
    Verb {
        name: "close",
        usage: "masil-agent agent [--socket MASIL_SOCKET] interrupt|close TARGET [--run RUN --operation ID]",
        mutating: true,
        target_stage: Some("pane_closed"),
        remote: true,
    },
    Verb {
        name: "wait",
        usage: "masil-agent agent [--socket MASIL_SOCKET] wait TARGET --state idle|working|blocked|exited [--timeout SECONDS] [--after-change]",
        mutating: false,
        target_stage: None,
        remote: true,
    },
    Verb {
        name: "resume",
        usage: "masil-agent agent [--socket MASIL_SOCKET] resume TARGET --name NAME [--split %N] [--boot BOOT --operation ID]",
        mutating: true,
        target_stage: Some("process_started"),
        remote: true,
    },
    Verb {
        name: "inbox",
        usage: "masil-agent agent [--socket MASIL_SOCKET] inbox [list] [--all] [--limit N] | ack ID... | read-all [--through SEQ] | enable | disable | status",
        mutating: true,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "operations",
        usage: "masil-agent agent [--socket MASIL_SOCKET] operations [list] [--all] [--limit N] | status | reconcile | adopt",
        mutating: true,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "operation",
        usage: "masil-agent agent [--socket MASIL_SOCKET] operation KEY | operation resolve KEY --as delivered|not-delivered",
        mutating: true,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "ack",
        usage: "masil-agent agent [--socket MASIL_SOCKET] ack TARGET --run RUN --revision REVISION",
        mutating: true,
        target_stage: Some("seen"),
        remote: true,
    },
    Verb {
        name: "save",
        usage: "masil-agent agent [--socket MASIL_SOCKET] save FILE",
        mutating: false,
        target_stage: None,
        remote: true,
    },
    Verb {
        name: "restore",
        usage: "masil-agent agent [--socket MASIL_SOCKET] restore FILE [--allow-fresh]",
        mutating: true,
        target_stage: None,
        remote: true,
    },
    Verb {
        name: "view",
        usage: "masil-agent agent [--socket MASIL_SOCKET] view get|clear|set [--provider ID] [--state STATE] [--workspace NAME] [--sort priority|name|provider|workspace]",
        mutating: true,
        target_stage: None,
        remote: true,
    },
    Verb {
        name: "integration",
        usage: "masil-agent agent [--socket MASIL_SOCKET] integration status [PROVIDER]",
        mutating: true,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "integration",
        usage: "masil-agent agent [--socket MASIL_SOCKET] integration export PROVIDER [--directory ABSOLUTE_PATH]",
        mutating: true,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "reload",
        usage: "masil-agent agent [--socket MASIL_SOCKET] reload",
        mutating: true,
        target_stage: Some("manifests_validated"),
        remote: false,
    },
    Verb {
        name: "coordinator",
        usage: "masil-agent agent [--socket MASIL_SOCKET] coordinator status|start|stop",
        mutating: true,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "report",
        usage: "masil-agent agent [--socket MASIL_SOCKET] report --pane %N --run RUN --sequence N --state STATE [--session ID]",
        mutating: true,
        target_stage: Some("report_recorded"),
        remote: false,
    },
    Verb {
        name: "ui",
        usage: "masil-agent agent [--socket MASIL_SOCKET] ui|sidebar [--lang en|ko] [--theme dark|light|terminal]",
        mutating: true,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "sidebar",
        usage: "masil-agent agent [--socket MASIL_SOCKET] ui|sidebar [--lang en|ko] [--theme dark|light|terminal]",
        mutating: true,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "rpc",
        usage: "",
        mutating: false,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "endpoint-connect",
        usage: "",
        mutating: true,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "exec-managed",
        usage: "",
        mutating: true,
        target_stage: None,
        remote: false,
    },
    Verb {
        name: "open-native",
        usage: "",
        mutating: true,
        target_stage: None,
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
