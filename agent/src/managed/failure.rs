//! Managed CLI failure classes and durable-result exit codes.

use std::fmt;

/// A stable class for failures reported by `masil-agent agent ...`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Invalid,
    Refused,
    Transient,
    Failed,
    Unknown,
}

impl Class {
    pub(crate) const fn exit_code(self) -> i32 {
        match self {
            Self::Invalid => 2,
            Self::Refused => 3,
            Self::Transient => 4,
            Self::Failed => 5,
            Self::Unknown => 6,
        }
    }
}

const CODES: &[(&str, Class)] = &[
    ("usage", Class::Invalid),
    ("invalid_argument", Class::Invalid),
    ("unknown_provider", Class::Invalid),
    ("unknown_target_syntax", Class::Invalid),
    ("repo_invalid", Class::Invalid),
    ("setup_invalid", Class::Invalid),
    ("identity_mismatch", Class::Refused),
    ("binding_conflict", Class::Refused),
    ("store_newer", Class::Refused),
    ("store_replaced", Class::Refused),
    ("store_unsupported", Class::Refused),
    ("host_key", Class::Refused),
    ("unknown_method", Class::Refused),
    ("coordinator_state_mismatch", Class::Refused),
    ("expired", Class::Refused),
    ("answer_unsafe", Class::Refused),
    ("answer_channel_none", Class::Refused),
    ("answer_refused", Class::Refused),
    ("queue_stale", Class::Refused),
    ("queue_held", Class::Refused),
    ("queue_not_staged", Class::Refused),
    ("queue_remote_unsupported", Class::Refused),
    ("attachment_missing", Class::Refused),
    ("cwd_rejected", Class::Refused),
    ("registry_newer", Class::Refused),
    ("worktree_unavailable", Class::Refused),
    ("worktree_in_use", Class::Refused),
    ("worktree_dirty", Class::Refused),
    ("worktree_locked", Class::Refused),
    ("worktree_registry_mismatch", Class::Refused),
    ("setup_missing", Class::Refused),
    ("setup_untrusted", Class::Refused),
    ("changes_unavailable", Class::Refused),
    ("lock_busy", Class::Transient),
    ("store_unavailable", Class::Transient),
    ("store_full", Class::Transient),
    ("store_busy", Class::Transient),
    ("operation_store_full", Class::Transient),
    ("server_unreachable", Class::Transient),
    ("endpoint_unreachable", Class::Transient),
    ("coordinator_unavailable", Class::Transient),
    ("coordinator_socket_path_too_long", Class::Transient),
    ("answer_unavailable", Class::Transient),
    ("queue_full", Class::Transient),
    ("queue_sending", Class::Transient),
    ("registry_busy", Class::Transient),
    ("server_unanswered", Class::Transient),
    ("changes_timeout", Class::Transient),
    ("registry_unavailable", Class::Transient),
    ("target_absent", Class::Failed),
    ("target_ended", Class::Failed),
    ("rejected_before_effect", Class::Failed),
    ("not_applied", Class::Failed),
    ("job_failed", Class::Failed),
    ("job_cancelled", Class::Failed),
    ("wait_timeout", Class::Unknown),
    ("outcome_unknown", Class::Unknown),
    ("observation_lost", Class::Unknown),
];

fn leading_code(message: &str) -> Option<(&'static str, Class)> {
    CODES.iter().find_map(|&(code, class)| {
        message
            .strip_prefix(code)
            .is_some_and(|rest| rest.starts_with(':'))
            .then_some((code, class))
    })
}

/// Classify only the leading `code:` token of an error message.
pub(crate) fn classify(message: &str) -> (Class, &'static str) {
    if let Some(inner) = message.strip_prefix("store_unavailable: ")
        && let Some((code, class)) = leading_code(inner)
    {
        return (class, code);
    }
    leading_code(message).map_or((Class::Failed, "failed"), |(code, class)| (class, code))
}

/// The message without its leading registered code tokens, for people:
/// `outcome_unknown: server_unreachable: timed out` becomes `timed out`.
pub(crate) fn human(message: &str) -> &str {
    let mut rest = message;
    while let Some((code, _)) = leading_code(rest) {
        rest = rest[code.len() + 1..].trim_start();
    }
    rest
}

pub(crate) fn has_registered_code(message: &str) -> bool {
    leading_code(message).is_some()
}

/// An error encountered after a tmux effect may have been dispatched.
///
/// This intentionally does not implement `From<String>`: callers must mark
/// each post-effect conversion explicitly rather than letting `?` silently
/// turn a potentially unknown outcome into an ordinary failure.
#[derive(Debug)]
pub(crate) struct AfterEffect(String);

impl AfterEffect {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for AfterEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AfterEffect {}

/// After an effect, every failure is an unknown outcome (class 6): a
/// transport or lookup code underneath does not make the effect absent.
impl From<AfterEffect> for String {
    fn from(error: AfterEffect) -> Self {
        match leading_code(&error.0) {
            Some((_, Class::Unknown)) => error.0,
            _ => format!("outcome_unknown: {}", error.0),
        }
    }
}

/// Exit status for a printed durable result. A direct operation query always
/// succeeds as a query, even when its stored stage records a failed outcome.
pub(crate) fn recorded_exit_code(stage: &str, query: bool) -> i32 {
    if query {
        return 0;
    }
    match stage {
        "process_started"
        | "process_exited"
        | "delivered"
        | "user_confirmed_delivered"
        | "interrupt_key_delivered"
        | "native_accepted"
        | "pane_closed" => 0,
        "expired" | "cwd_rejected" => Class::Refused.exit_code(),
        // An attempt still in flight has no known outcome yet.
        "outcome_unknown" | "dispatching" | "pending" => Class::Unknown.exit_code(),
        "rejected_before_effect"
        | "not_applied"
        | "target_absent"
        | "user_confirmed_not_delivered" => Class::Failed.exit_code(),
        _ => Class::Failed.exit_code(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_text_drops_leading_codes_only() {
        assert_eq!(
            human("outcome_unknown: server_unreachable: native command timed out"),
            "native command timed out"
        );
        assert_eq!(human("plain: not a code"), "plain: not a code");
        assert_eq!(
            human("target_absent: agent target not found"),
            "agent target not found"
        );
    }

    #[test]
    fn classifies_registered_leading_codes_and_defaults_to_failed() {
        for &(code, class) in CODES {
            assert_eq!(classify(&format!("{code}: detail")), (class, code));
        }
        assert_eq!(
            classify("usage: expected a target"),
            (Class::Invalid, "usage")
        );
        assert_eq!(
            classify("identity_mismatch: pane changed"),
            (Class::Refused, "identity_mismatch")
        );
        assert_eq!(
            classify("store_busy: try again"),
            (Class::Transient, "store_busy")
        );
        assert_eq!(
            classify("target_absent: no matching pane"),
            (Class::Failed, "target_absent")
        );
        assert_eq!(
            classify("outcome_unknown: command ended early"),
            (Class::Unknown, "outcome_unknown")
        );
        assert_eq!(classify("plain error"), (Class::Failed, "failed"));
        assert_eq!(
            classify("details: usage: is not a leading code"),
            (Class::Failed, "failed")
        );
    }

    #[test]
    fn nested_store_unavailable_uses_the_inner_registered_code() {
        assert_eq!(
            classify("store_unavailable: store_newer: reader generation is old"),
            (Class::Refused, "store_newer")
        );
        assert_eq!(
            classify("store_unavailable: ordinary filesystem failure"),
            (Class::Transient, "store_unavailable")
        );
    }

    #[test]
    fn after_an_effect_every_failure_is_an_unknown_outcome() {
        for message in [
            "receipt write failed",
            "store_busy: retry later",
            "server_unreachable: native command timed out",
            "target_absent: agent target not found",
        ] {
            let converted = String::from(AfterEffect::new(message));
            assert_eq!(classify(&converted).0, Class::Unknown, "{converted}");
            assert_eq!(converted, format!("outcome_unknown: {message}"));
        }
        assert_eq!(
            String::from(AfterEffect::new("wait_timeout: no change")),
            "wait_timeout: no change"
        );
    }

    #[test]
    fn recorded_stages_have_fixed_exit_codes() {
        for stage in [
            "process_started",
            "process_exited",
            "delivered",
            "user_confirmed_delivered",
            "interrupt_key_delivered",
            "pane_closed",
        ] {
            assert_eq!(recorded_exit_code(stage, false), 0, "{stage}");
        }
        for stage in [
            "rejected_before_effect",
            "not_applied",
            "target_absent",
            "user_confirmed_not_delivered",
        ] {
            assert_eq!(recorded_exit_code(stage, false), 5, "{stage}");
        }
        for stage in ["outcome_unknown", "dispatching"] {
            assert_eq!(recorded_exit_code(stage, false), 6, "{stage}");
        }
        assert_eq!(recorded_exit_code("outcome_unknown", true), 0);
    }
}
