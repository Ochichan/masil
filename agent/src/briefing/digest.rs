//! Phase 2B: 90-second Morning / Return Briefing deterministic aggregator.
//!
//! Aggregates across all discovered local masil servers + SQLite stores
//! (`turn_audits`, `collision_snapshots`, `circuit_breakers`, `focus_held_events`)
//! into 3 decisive triage buckets and renders a validated Masil UI Spec v1
//! `UiDocument` for simultaneous TUI, CLI, and Mobile consumption:
//! 1. `🔴 Needs Decision` (`needs_decision`): blocked approvals/questions, tripped
//!    circuit breakers, hard worktree conflicts.
//! 2. `🟢 Ready to Merge` (`ready_to_merge`): idle/finished turns with BS Detector
//!    `verified` AND Collision Radar `clean`.
//! 3. `🟡 Review Recommended` (`review_recommended`): audit warnings/flagged findings
//!    or high-risk diffs.

use crate::briefing::shield::{FocusShieldStatus, get_status_from_store, parse_focus_until_ms};
use crate::managed::Store;
use crate::observation::now_ms;
use crate::plugin::ui_spec::{UI_SPEC_VERSION, UiBlock, UiDocument, UiListItem, UiMetric, UiTone};
use crate::remote::discovery::{
    discover_masil_servers, list_agents_for_server, list_all_host_agents,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BriefingItem {
    pub id: String,
    pub agent_name: String,
    pub remote_id: String,
    pub pane: String,
    pub server_socket: String,
    pub provider: String,
    pub title: String,
    pub subtitle: String,
    pub badge: String,
    pub action: String,
    pub bucket: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BriefingMetrics {
    pub completed_turns: u64,
    pub files_touched: u64,
    pub lines_changed: u64,
    pub breaker_interventions: u64,
    pub active_collisions: u64,
    pub held_notifications: u64,
    pub active_agents: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BriefingReport {
    pub generated_ms: u64,
    pub since_ms: u64,
    pub window_label: String,
    pub focus: Option<FocusShieldStatus>,
    pub metrics: BriefingMetrics,
    pub needs_decision: Vec<BriefingItem>,
    pub ready_to_merge: Vec<BriefingItem>,
    pub review_recommended: Vec<BriefingItem>,
    pub ui: UiDocument,
}

/// Parses `--since` window strings like `"8h"`, `"24h"`, `"30m"` into milliseconds.
pub fn parse_since_window_ms(spec: &str) -> Result<u64, String> {
    let base = 1_000_000_000u64;
    let until = parse_focus_until_ms(spec, base)?;
    Ok(until.saturating_sub(base))
}

/// Generates a 90-second Morning / Return Briefing from live host agents and SQLite
/// state across the specified socket (or all discovered local servers when `None`).
pub fn generate_briefing(
    socket_filter: Option<&Path>,
    since_spec: &str,
) -> Result<BriefingReport, String> {
    let window_ms = parse_since_window_ms(since_spec)?;
    let now = now_ms();
    let since_ms = now.saturating_sub(window_ms);

    let filtered_agents: Vec<Value> = match socket_filter {
        Some(target) => {
            let server_name = target
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("default");
            list_agents_for_server(server_name, &target.to_string_lossy())
        }
        None => list_all_host_agents(),
    };

    let mut store_paths: Vec<std::path::PathBuf> = Vec::new();
    if let Some(sf) = socket_filter {
        store_paths.push(sf.to_path_buf());
    } else {
        for srv in discover_masil_servers() {
            store_paths.push(std::path::PathBuf::from(srv.socket));
        }
    }

    let mut focus: Option<FocusShieldStatus> = None;
    let mut completed_turns: u64 = 0;
    let mut files_touched_set: HashSet<String> = HashSet::new();
    let mut lines_changed: u64 = 0;
    let mut breaker_interventions: u64 = 0;
    let mut active_collisions: u64 = 0;
    let mut held_notifications: u64 = 0;
    let mut collision_items: Vec<BriefingItem> = Vec::new();

    for sp in &store_paths {
        let Ok(mut store) = Store::open(sp) else {
            continue;
        };
        if let Ok(st) = get_status_from_store(&mut store, now)
            && (focus.is_none() || st.active)
        {
            focus = Some(st);
        }
        if let Ok(held) = store.count_focus_held_events(Some(since_ms)) {
            held_notifications += held;
        }
        if let Ok(audits) = store.list_turn_audits(None, None, 200) {
            for a in audits {
                if a.created_ms >= since_ms {
                    completed_turns += 1;
                    if let Ok(hunks) = serde_json::from_str::<Vec<Value>>(&a.hunks_json) {
                        for h in hunks {
                            if let Some(f) = h.get("file").and_then(Value::as_str) {
                                files_touched_set.insert(f.to_owned());
                            }
                            lines_changed += h.get("added").and_then(Value::as_u64).unwrap_or(0);
                            lines_changed += h.get("removed").and_then(Value::as_u64).unwrap_or(0);
                        }
                    }
                }
            }
        }
        if let Ok(breakers) = store.list_circuit_breakers() {
            for cb in breakers {
                if let Some(tripped_at) = cb.tripped_at_ms
                    && tripped_at >= since_ms
                {
                    breaker_interventions += 1;
                }
            }
        }
        if let Ok(colls) = store.list_collision_snapshots(None) {
            for c in colls {
                if c.severity != "clean" {
                    active_collisions += 1;
                    if matches!(c.severity.as_str(), "hard_conflict" | "same_directory_race") {
                        let files: Vec<String> =
                            serde_json::from_str(&c.files_json).unwrap_or_default();
                        collision_items.push(BriefingItem {
                            id: format!("coll:{}:{}", c.agent_a, c.agent_b),
                            agent_name: format!("{} ↔ {}", c.agent_a, c.agent_b),
                            remote_id: String::new(),
                            pane: String::new(),
                            server_socket: sp.to_string_lossy().into_owned(),
                            provider: "worktree".into(),
                            title: format!("{} ↔ {} ({})", c.agent_a, c.agent_b, c.severity),
                            subtitle: if files.is_empty() {
                                c.repo_key.clone()
                            } else {
                                files.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
                            },
                            badge: "CONFLICT".into(),
                            action: "resolve_collision".into(),
                            bucket: "needs_decision".into(),
                        });
                    }
                }
            }
        }
    }

    let (mut needs_decision, ready_to_merge, review_recommended) =
        classify_agents_into_buckets(&filtered_agents);
    needs_decision.extend(collision_items);

    let metrics = BriefingMetrics {
        completed_turns,
        files_touched: files_touched_set.len() as u64,
        lines_changed,
        breaker_interventions,
        active_collisions,
        held_notifications,
        active_agents: filtered_agents
            .iter()
            .filter(|a| a.get("state").and_then(Value::as_str) != Some("exited"))
            .count() as u64,
    };

    let ui = build_briefing_ui_document(
        since_spec,
        &metrics,
        &needs_decision,
        &ready_to_merge,
        &review_recommended,
        focus.as_ref(),
    );

    Ok(BriefingReport {
        generated_ms: now,
        since_ms,
        window_label: since_spec.to_owned(),
        focus,
        metrics,
        needs_decision,
        ready_to_merge,
        review_recommended,
        ui,
    })
}

/// Pure deterministic bucketing of live agent snapshots into the 3 Morning Briefing groups.
pub fn classify_agents_into_buckets(
    agents: &[Value],
) -> (Vec<BriefingItem>, Vec<BriefingItem>, Vec<BriefingItem>) {
    let mut needs_decision = Vec::new();
    let mut ready_to_merge = Vec::new();
    let mut review_recommended = Vec::new();

    for agent in agents {
        let state = agent
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if state == "exited" {
            continue;
        }
        let pane = agent
            .get("pane")
            .or_else(|| agent.get("pane_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let name = agent
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&pane)
            .to_owned();
        let remote_id = agent
            .get("remote_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let server_socket = agent
            .get("server_socket")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let provider = agent
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let perm = agent
            .get("permission_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let q = agent
            .get("question_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let breaker_tripped = agent
            .get("breaker")
            .and_then(|b| b.get("tripped"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let breaker_reason = agent
            .get("breaker")
            .and_then(|b| b.get("tripped_reason"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let coll_sev = agent
            .get("collision_severity")
            .and_then(Value::as_str)
            .unwrap_or("clean");
        let audit_verdict = agent
            .get("audit_summary")
            .and_then(|v| v.get("verdict"))
            .and_then(Value::as_str);
        let audit_risk = agent
            .get("audit_summary")
            .and_then(|v| v.get("risk_score"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let crit_count = agent
            .get("audit_summary")
            .and_then(|v| v.get("critical_count"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let high_count = agent
            .get("audit_summary")
            .and_then(|v| v.get("high_count"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let summary = agent
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();

        // Bucket 1: Needs Decision (blocked approval/question, breaker tripped, hard conflict)
        if breaker_tripped
            || state == "blocked"
            || perm > 0
            || q > 0
            || matches!(coll_sev, "hard_conflict" | "same_directory_race")
        {
            let (badge, subtitle, action) = if breaker_tripped {
                (
                    "LOOP PAUSED".into(),
                    format!("Circuit breaker tripped ({breaker_reason})"),
                    "recover_breaker".into(),
                )
            } else if matches!(coll_sev, "hard_conflict" | "same_directory_race") {
                (
                    "CONFLICT".into(),
                    format!("Worktree collision ({coll_sev})"),
                    "resolve_collision".into(),
                )
            } else if perm > 0 {
                (
                    format!("APPROVAL ×{perm}"),
                    if summary.is_empty() {
                        "Waiting for permission approval".into()
                    } else {
                        summary.to_owned()
                    },
                    "answer".into(),
                )
            } else if q > 0 {
                (
                    format!("QUESTION ×{q}"),
                    if summary.is_empty() {
                        "Waiting for your answer".into()
                    } else {
                        summary.to_owned()
                    },
                    "answer".into(),
                )
            } else {
                (
                    "NEEDS INPUT".into(),
                    if summary.is_empty() {
                        "Blocked awaiting user input".into()
                    } else {
                        summary.to_owned()
                    },
                    "answer".into(),
                )
            };
            needs_decision.push(BriefingItem {
                id: format!("dec:{remote_id}:{pane}"),
                agent_name: name.clone(),
                remote_id,
                pane,
                server_socket,
                provider: provider.clone(),
                title: format!("{name} ({provider})"),
                subtitle,
                badge,
                action,
                bucket: "needs_decision".into(),
            });
            continue;
        }

        // Bucket 3: Review Recommended (flagged/warnings audit or high risk score or non-hard overlap)
        if matches!(audit_verdict, Some("flagged" | "warnings"))
            || audit_risk >= 40
            || crit_count > 0
            || high_count > 0
            || matches!(coll_sev, "semantic_risk" | "overlap")
        {
            let badge = if crit_count > 0 {
                format!("BS CRIT ×{crit_count}")
            } else if high_count > 0 {
                format!("RISK {audit_risk}")
            } else if coll_sev != "clean" {
                coll_sev.to_uppercase()
            } else {
                format!("RISK {audit_risk}")
            };
            review_recommended.push(BriefingItem {
                id: format!("rev:{remote_id}:{pane}"),
                agent_name: name.clone(),
                remote_id,
                pane,
                server_socket,
                provider: provider.clone(),
                title: format!("{name} ({provider})"),
                subtitle: if summary.is_empty() {
                    format!(
                        "Audit verdict: {} · risk {audit_risk}/100",
                        audit_verdict.unwrap_or("review")
                    )
                } else {
                    summary.to_owned()
                },
                badge,
                action: "review_risk_cards".into(),
                bucket: "review_recommended".into(),
            });
            continue;
        }

        // Bucket 2: Ready to Merge (idle/finished + audit verified + collision clean)
        if state == "idle" && audit_verdict == Some("verified") && coll_sev == "clean" {
            ready_to_merge.push(BriefingItem {
                id: format!("mrg:{remote_id}:{pane}"),
                agent_name: name.clone(),
                remote_id,
                pane,
                server_socket,
                provider: provider.clone(),
                title: format!("{name} ({provider})"),
                subtitle: if summary.is_empty() {
                    "BS Detector verified · Collision Radar clean".into()
                } else {
                    summary.to_owned()
                },
                badge: "VERIFIED".into(),
                action: "view_diff".into(),
                bucket: "ready_to_merge".into(),
            });
        }
    }

    (needs_decision, ready_to_merge, review_recommended)
}

/// Builds a validated Masil UI Spec v1 `UiDocument` for the 90-second briefing.
pub fn build_briefing_ui_document(
    window_label: &str,
    metrics: &BriefingMetrics,
    needs_decision: &[BriefingItem],
    ready_to_merge: &[BriefingItem],
    review_recommended: &[BriefingItem],
    focus: Option<&FocusShieldStatus>,
) -> UiDocument {
    let mut blocks: Vec<UiBlock> = Vec::new();

    let focus_badge = match focus {
        Some(f) if f.active => Some("FOCUS ON".to_owned()),
        _ => Some(format!("LAST {window_label}")),
    };
    blocks.push(UiBlock::Header {
        title: "90-Second Fleet Briefing".into(),
        subtitle: Some(format!(
            "{} active agents · {} decisions · {} merge-ready · {} review",
            metrics.active_agents,
            needs_decision.len(),
            ready_to_merge.len(),
            review_recommended.len()
        )),
        badge: focus_badge,
        tone: if !needs_decision.is_empty() {
            UiTone::Warning
        } else {
            UiTone::Success
        },
    });

    blocks.push(UiBlock::MetricRow {
        items: vec![
            UiMetric {
                label: "Turns".into(),
                value: metrics.completed_turns.to_string(),
                delta: None,
                tone: UiTone::Accent,
            },
            UiMetric {
                label: "Files / LOC".into(),
                value: format!("{}/{}", metrics.files_touched, metrics.lines_changed),
                delta: None,
                tone: UiTone::Normal,
            },
            UiMetric {
                label: "Breakers".into(),
                value: metrics.breaker_interventions.to_string(),
                delta: None,
                tone: if metrics.breaker_interventions > 0 {
                    UiTone::Danger
                } else {
                    UiTone::Success
                },
            },
            UiMetric {
                label: "Collisions".into(),
                value: metrics.active_collisions.to_string(),
                delta: if metrics.held_notifications > 0 {
                    Some(format!("{} held", metrics.held_notifications))
                } else {
                    None
                },
                tone: if metrics.active_collisions > 0 {
                    UiTone::Warning
                } else {
                    UiTone::Success
                },
            },
        ],
    });

    let to_list_items = |items: &[BriefingItem], tone: UiTone| -> Vec<UiListItem> {
        items
            .iter()
            .map(|it| UiListItem {
                id: Some(it.id.clone()),
                title: it.title.clone(),
                subtitle: Some(it.subtitle.clone()),
                badge: Some(it.badge.clone()),
                tone,
                action: Some(it.action.clone()),
                payload: Some(json!({
                    "remote_id": it.remote_id,
                    "pane": it.pane,
                    "agent_name": it.agent_name,
                    "server_socket": it.server_socket,
                    "bucket": it.bucket,
                })),
            })
            .collect()
    };

    if !needs_decision.is_empty() {
        blocks.push(UiBlock::List {
            title: Some(format!("🔴 Needs Decision ({})", needs_decision.len())),
            items: to_list_items(needs_decision, UiTone::Danger),
        });
    }

    if !ready_to_merge.is_empty() {
        blocks.push(UiBlock::List {
            title: Some(format!("🟢 Ready to Merge ({})", ready_to_merge.len())),
            items: to_list_items(ready_to_merge, UiTone::Success),
        });
    }

    if !review_recommended.is_empty() {
        blocks.push(UiBlock::List {
            title: Some(format!(
                "🟡 Review Recommended ({})",
                review_recommended.len()
            )),
            items: to_list_items(review_recommended, UiTone::Warning),
        });
    }

    if needs_decision.is_empty() && ready_to_merge.is_empty() && review_recommended.is_empty() {
        blocks.push(UiBlock::Empty {
            title: "All quiet across your fleet".into(),
            body: Some(
                "No blocked decisions, warnings, or unreviewed turns in this window.".into(),
            ),
        });
    }

    UiDocument {
        v: UI_SPEC_VERSION,
        title: Some("90-Second Fleet Briefing".into()),
        subtitle: Some(format!("Window: {window_label}")),
        blocks,
        custom_surface: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::ui_spec::validate_ui_document;

    #[test]
    fn classifies_agents_into_three_buckets_and_produces_valid_ui_document() {
        let agents = vec![
            json!({
                "remote_id": "d::%1",
                "pane": "%1",
                "name": "auth-fix",
                "provider": "claude",
                "state": "blocked",
                "permission_count": 1,
                "collision_severity": "clean"
            }),
            json!({
                "remote_id": "d::%2",
                "pane": "%2",
                "name": "ui-polish",
                "provider": "codex",
                "state": "idle",
                "collision_severity": "clean",
                "audit_summary": { "verdict": "verified", "risk_score": 12, "critical_count": 0, "high_count": 0 }
            }),
            json!({
                "remote_id": "d::%3",
                "pane": "%3",
                "name": "refactor",
                "provider": "agy",
                "state": "idle",
                "collision_severity": "clean",
                "audit_summary": { "verdict": "flagged", "risk_score": 82, "critical_count": 1, "high_count": 1 }
            }),
        ];

        let (dec, ready, review) = classify_agents_into_buckets(&agents);
        assert_eq!(dec.len(), 1);
        assert_eq!(dec[0].agent_name, "auth-fix");
        assert_eq!(dec[0].action, "answer");

        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].agent_name, "ui-polish");
        assert_eq!(ready[0].action, "view_diff");

        assert_eq!(review.len(), 1);
        assert_eq!(review[0].agent_name, "refactor");
        assert_eq!(review[0].action, "review_risk_cards");

        let metrics = BriefingMetrics {
            completed_turns: 5,
            files_touched: 8,
            lines_changed: 320,
            breaker_interventions: 1,
            active_collisions: 0,
            held_notifications: 4,
            active_agents: 3,
        };
        let doc = build_briefing_ui_document("8h", &metrics, &dec, &ready, &review, None);
        let val = serde_json::to_value(&doc).unwrap();
        assert!(validate_ui_document(&val).is_ok());
    }
}
