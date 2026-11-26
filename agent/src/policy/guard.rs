//! D-10 4-Guard TUI & Structured-API Auto-Approval Safety Engine.
//!
//! Guarantees that automatic permission approval can NEVER misfire:
//! 1. Hard `Low` risk cap (ignores rules requesting medium/high on TUI channel).
//! 2. Command completeness guard (refuses visually truncated `…` / `...` commands).
//! 3. Re-verification of `(run_id, pane_output_generation, prompt_digest)` right
//!    before key delivery + explicit non-destructive `Allow` option (no blind Enter).
//! 4. Permanent SQLite `permission_audit_log` recording + 1-tap rule revocation.

use crate::managed::{PermissionAuditRecord, Store};
use crate::policy::rules::{PolicyAction, PolicyRule};
use masil_wire::rpc::{
    InteractiveOption, InteractivePrompt, InteractivePromptKind, InteractivePromptMode,
    PromptRiskAssessment, PromptRiskTier,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct AutoApproveContext<'a> {
    pub run_id: &'a str,
    pub pane_id: &'a str,
    pub provider: &'a str,
    pub prompt: &'a InteractivePrompt,
    pub assessment: &'a PromptRiskAssessment,
    pub matched_rule: Option<&'a PolicyRule>,
    pub allow_tui_rule_auto_approve: bool,
    pub observed_output_generation: u64,
    pub current_output_generation: u64,
    pub current_prompt_digest: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum GuardDecision {
    Allow {
        rule_id: String,
        option_index: usize,
        option_id: String,
        hotkey: Option<String>,
        mode: String,
    },
    Deny {
        rule_id: String,
        reason: String,
    },
    Escalate {
        guard: String,
        reason: String,
    },
}

/// Evaluates all four D-10 guards before any automated approval action is permitted.
pub fn evaluate_auto_approve_guards(ctx: &AutoApproveContext<'_>) -> GuardDecision {
    // Only Permission / Confirmation prompts can ever be auto-approved.
    if !matches!(
        ctx.prompt.kind,
        InteractivePromptKind::Permission | InteractivePromptKind::Confirmation
    ) {
        return GuardDecision::Escalate {
            guard: "kind_check".into(),
            reason: "questions and free-text inputs always require human answer".into(),
        };
    }

    let Some(rule) = ctx.matched_rule else {
        return GuardDecision::Escalate {
            guard: "no_rule".into(),
            reason: "no user policy rule matched".into(),
        };
    };

    if !rule.enabled {
        return GuardDecision::Escalate {
            guard: "rule_disabled".into(),
            reason: format!("policy rule `{}` is disabled/revoked", rule.id),
        };
    }

    match rule.action {
        PolicyAction::Deny => {
            return GuardDecision::Deny {
                rule_id: rule.id.clone(),
                reason: format!("blocked by deny rule `{}`", rule.id),
            };
        }
        PolicyAction::AlwaysConfirm | PolicyAction::SuggestBatch => {
            return GuardDecision::Escalate {
                guard: "policy_action".into(),
                reason: format!(
                    "rule `{}` requests `{}` rather than auto_approve",
                    rule.id,
                    rule.action.as_str()
                ),
            };
        }
        PolicyAction::AutoApprove => {}
    }

    if ctx.prompt.mode == InteractivePromptMode::TerminalTui && !ctx.allow_tui_rule_auto_approve {
        return GuardDecision::Escalate {
            guard: "tui_disabled".into(),
            reason: "TUI rule auto-approval is disabled in policies.toml".into(),
        };
    }

    // GUARD 1: Hard `Low` risk cap (even if the rule itself specified medium/high).
    if ctx.assessment.tier != PromptRiskTier::Low {
        return GuardDecision::Escalate {
            guard: "guard_1_low_risk_cap".into(),
            reason: format!(
                "risk tier `{}` exceeds hard auto-approve cap `low`",
                ctx.assessment.tier.as_str()
            ),
        };
    }

    // GUARD 2: Command completeness verification (reject visually truncated commands).
    if ctx.assessment.truncated {
        return GuardDecision::Escalate {
            guard: "guard_2_command_truncated".into(),
            reason: "command is visually truncated (`…`/`...`); refusing auto-approve".into(),
        };
    }
    if ctx
        .assessment
        .command
        .as_deref()
        .is_none_or(|c| c.trim().is_empty())
    {
        return GuardDecision::Escalate {
            guard: "guard_2_command_missing".into(),
            reason: "could not extract complete command text; refusing auto-approve".into(),
        };
    }

    // GUARD 3: Re-verify run/pane/provider identity, pane output generation + prompt digest,
    // AND require a verified non-destructive explicit Allow option (NEVER blind Enter on fallback!).
    if ctx.run_id.trim().is_empty()
        || ctx.pane_id.trim().is_empty()
        || ctx.provider.trim().is_empty()
    {
        return GuardDecision::Escalate {
            guard: "guard_3_identity_missing".into(),
            reason: "missing run_id, pane_id, or provider identity; refusing auto-approve".into(),
        };
    }
    if ctx.observed_output_generation != ctx.current_output_generation {
        return GuardDecision::Escalate {
            guard: "guard_3_generation_changed".into(),
            reason: format!(
                "pane_output_generation drifted ({} -> {}) before delivery",
                ctx.observed_output_generation, ctx.current_output_generation
            ),
        };
    }
    if ctx.assessment.prompt_digest != ctx.current_prompt_digest {
        return GuardDecision::Escalate {
            guard: "guard_3_digest_mismatch".into(),
            reason: "prompt_digest changed between evaluation and delivery".into(),
        };
    }
    if ctx.prompt.id.starts_with("fallback") {
        return GuardDecision::Escalate {
            guard: "guard_3_no_blind_enter".into(),
            reason: "refusing blind Enter on unparsed fallback screen".into(),
        };
    }

    let Some(allow_opt) = find_verified_allow_option(ctx.prompt) else {
        return GuardDecision::Escalate {
            guard: "guard_3_no_verified_allow_option".into(),
            reason: "no verified non-destructive Allow option found on prompt".into(),
        };
    };

    GuardDecision::Allow {
        rule_id: rule.id.clone(),
        option_index: allow_opt.index,
        option_id: allow_opt.id.clone(),
        hotkey: allow_opt.hotkey.clone(),
        mode: match ctx.prompt.mode {
            InteractivePromptMode::StructuredApi => "structured_api".into(),
            InteractivePromptMode::TerminalTui => "terminal_tui".into(),
        },
    }
}

/// Finds a verified, non-destructive single-use `Allow` option (preferring `Allow once` / `Yes`
/// over `Always allow`, and strictly forbidding destructive options or raw `\r` blind Enter).
pub fn find_verified_allow_option(prompt: &InteractivePrompt) -> Option<&InteractiveOption> {
    if prompt.id.starts_with("fallback") {
        return None;
    }
    if prompt.mode == InteractivePromptMode::StructuredApi {
        return prompt.options.iter().find(|o| {
            o.id == "once" && !o.is_destructive && !o.is_custom_input && o.disabled_reason.is_none()
        });
    }
    let candidates: Vec<&InteractiveOption> = prompt
        .options
        .iter()
        .filter(|o| {
            if o.is_destructive || o.is_custom_input || o.disabled_reason.is_some() {
                return false;
            }
            // Reject blind bare Enter hotkey on generic "Continue" confirmations.
            if matches!(o.hotkey.as_deref(), Some("\r" | "\n")) {
                return false;
            }
            let lower = o.label.trim().to_ascii_lowercase();
            let is_allow = lower.starts_with("yes")
                || lower.starts_with("allow")
                || lower.starts_with("approve")
                || lower.starts_with("accept")
                || lower == "y"
                || lower.starts_with("run ")
                || lower.starts_with("허용")
                || lower.starts_with("승인")
                || lower == "예";
            // Never pick "always allow" automatically; prefer single-step allow.
            let is_always = lower.contains("always")
                || lower.contains("all future")
                || lower.contains("don't ask again")
                || lower.contains("항상");
            is_allow && !is_always
        })
        .collect();

    // Prefer options with an explicit single-character hotkey (e.g. `1`, `y`, `a`),
    // otherwise the first explicit non-destructive option.
    candidates
        .iter()
        .copied()
        .find(|o| o.hotkey.as_deref().is_some_and(|h| !h.is_empty()))
        .or_else(|| candidates.first().copied())
}

/// Strict predicate for mobile/TUI batch approval: ONLY un-truncated `Low` risk
/// permission prompts that are not `deny` / `always_confirm` and have a verified
/// non-destructive Allow option.
pub fn is_batch_approvable(prompt: &InteractivePrompt) -> bool {
    let Some(risk) = prompt.risk.as_ref() else {
        return false;
    };
    if risk.tier != PromptRiskTier::Low || risk.truncated || !risk.batch_eligible {
        return false;
    }
    if matches!(
        risk.matched_action.as_deref(),
        Some("deny" | "always_confirm")
    ) {
        return false;
    }
    find_verified_allow_option(prompt).is_some()
}

/// GUARD 4: Persists every firewall decision to SQLite `permission_audit_log`.
#[allow(clippy::too_many_arguments)]
pub fn record_decision_audit(
    store: &mut Store,
    run_id: &str,
    pane_id: &str,
    rule_id: Option<&str>,
    provider: &str,
    assessment: &PromptRiskAssessment,
    decision: &str,
    screen_excerpt: Option<&str>,
    now_ms: u64,
) -> Result<i64, String> {
    let excerpt = screen_excerpt.map(|s| s.chars().take(1024).collect::<String>());
    store.record_permission_audit(&PermissionAuditRecord {
        id: 0,
        run_id: run_id.to_owned(),
        pane_id: pane_id.to_owned(),
        rule_id: rule_id.map(str::to_owned),
        provider: provider.to_owned(),
        command: assessment
            .command
            .clone()
            .unwrap_or_else(|| "(unknown)".into()),
        risk_tier: assessment.tier.as_str().to_owned(),
        decision: decision.to_owned(),
        prompt_digest: assessment.prompt_digest.clone(),
        screen_excerpt: excerpt,
        revoked_ms: None,
        created_ms: now_ms,
    })
}
