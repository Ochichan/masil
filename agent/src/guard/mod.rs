//! Phase 1C: Runaway Circuit Breaker & Prompt Cache Warmth Gauge (D-05, D-06).

pub mod breaker;
pub mod cache_warmth;
pub mod config;

#[cfg(test)]
mod tests {
    use super::breaker::{evaluate_turn_state, extract_error_fingerprint};
    use super::cache_warmth::{
        CacheWarmthSnapshot, HIGH_CONTEXT_TOKEN_THRESHOLD, resolve_ttl_secs,
    };
    use super::config::{CircuitBreakerConfig, GuardConfig};
    use crate::managed::CacheWarmthRecord;
    use std::collections::HashMap;

    #[test]
    fn normalizes_line_numbers_and_trips_on_same_error_streak() {
        let cfg = CircuitBreakerConfig {
            same_error_limit: 4,
            skip_auto_interrupt_when_focused: true,
            ..Default::default()
        };
        let (h1, _) =
            extract_error_fingerprint("src/app.rs:42:10 error: cannot find value `x`").unwrap();
        let (h2, _) =
            extract_error_fingerprint("src/app.rs:99:12 error: cannot find value `x`").unwrap();
        assert_eq!(
            h1, h2,
            "normalized error hashes must match across line shifts"
        );

        let mut rec = None;
        let mut last_eval = None;
        for turn in 1..=4 {
            let ev = evaluate_turn_state(
                rec,
                "run-1",
                "%1",
                turn,
                Some(&format!("tree-{turn}")),
                Some(turn),
                "error: cannot find value `x` at line 10",
                false, // unfocused -> must auto-interrupt!
                &cfg,
                turn * 1000,
            );
            rec = Some(ev.record.clone());
            last_eval = Some(ev);
        }
        let tripped = last_eval.unwrap();
        assert!(tripped.newly_tripped);
        assert!(tripped.should_auto_interrupt);
        assert!(
            tripped
                .record
                .tripped_reason
                .as_deref()
                .unwrap_or("")
                .starts_with("same_error_streak")
        );
    }

    #[test]
    fn tree_oscillation_trips_but_focused_pane_only_shows_banner() {
        let cfg = CircuitBreakerConfig {
            oscillation_window: 6,
            skip_auto_interrupt_when_focused: true,
            ..Default::default()
        };
        let e1 = evaluate_turn_state(
            None,
            "r",
            "%1",
            1,
            Some("treeA"),
            Some(10),
            "",
            true,
            &cfg,
            100,
        );
        let e2 = evaluate_turn_state(
            Some(e1.record),
            "r",
            "%1",
            2,
            Some("treeB"),
            Some(11),
            "",
            true,
            &cfg,
            200,
        );
        let e3 = evaluate_turn_state(
            Some(e2.record),
            "r",
            "%1",
            3,
            Some("treeA"),
            Some(12),
            "",
            true,
            &cfg,
            300,
        );
        assert!(e3.newly_tripped);
        assert!(
            !e3.should_auto_interrupt,
            "D-05: focused pane must only display warning banner, never auto-interrupt"
        );
        assert_eq!(e3.pre_oscillation_checkpoint_id, Some(10));
    }

    #[test]
    fn cache_warmth_tiers_overrides_and_invalidation_warning() {
        let mut overrides = HashMap::new();
        overrides.insert("codex".into(), 900);
        assert_eq!(resolve_ttl_secs("codex", &overrides), 900);
        assert_eq!(resolve_ttl_secs("gemini", &overrides), 300);

        let rec = CacheWarmthRecord {
            run_id: "r1".into(),
            pane_id: "%1".into(),
            provider: "claude".into(),
            ttl_secs: 300,
            last_turn_at_ms: 10_000,
            est_context_tokens: HIGH_CONTEXT_TOKEN_THRESHOLD + 5_000,
            updated_ms: 10_000,
        };
        let hot = CacheWarmthSnapshot::from_record(&rec, 70_000);
        assert_eq!(hot.tier, "hot");
        assert!(hot.invalidate_warning);
        assert!(hot.badge_text.contains("est."));

        let expiring = CacheWarmthSnapshot::from_record(&rec, 270_000);
        assert_eq!(expiring.tier, "expiring");
        assert!(expiring.invalidate_warning);

        let cold = CacheWarmthSnapshot::from_record(&rec, 400_000);
        assert_eq!(cold.tier, "cold");
        assert!(!cold.invalidate_warning);

        let parsed: GuardConfig =
            toml::from_str("[collision]\nauto_warn_sibling = true\n[cache_ttl]\ncodex = 420\n")
                .unwrap();
        assert!(parsed.collision.auto_warn_sibling);
        assert_eq!(parsed.cache_ttl.get("codex"), Some(&420));
    }
}
