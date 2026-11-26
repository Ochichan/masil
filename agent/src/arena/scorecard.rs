//! Masil UI Spec v1 live scorecard renderer for Phase 3B Agent Arena (D-12).

use crate::managed::{ArenaRacerRecord, ArenaRecord};
use crate::plugin::ui_spec::{
    UI_SPEC_VERSION, UiBlock, UiButton, UiDocument, UiListItem, UiMetric, UiTone,
};
use serde_json::json;

/// Ranks racers deterministically:
/// 1) verify_exit_code == Some(0) beats non-zero/none
/// 2) BS audit verdict `verified` > `warnings` > `flagged`
/// 3) smaller diff churn / faster elapsed time breaks ties.
pub fn rank_racers(racers: &[ArenaRacerRecord]) -> Vec<&ArenaRacerRecord> {
    let mut sorted: Vec<&ArenaRacerRecord> = racers.iter().collect();
    sorted.sort_by_key(|r| {
        let verify_rank = match r.verify_exit_code {
            Some(0) => 0u8,
            None => 1u8,
            Some(_) => 2u8,
        };
        let audit_rank = match r.audit_verdict.as_deref() {
            Some("verified") => 0u8,
            Some("warnings") => 1u8,
            None => 2u8,
            Some(_) => 3u8,
        };
        let churn = r.insertions.unwrap_or(0) + r.deletions.unwrap_or(0);
        let elapsed = r.elapsed_ms.unwrap_or(u64::MAX);
        (
            verify_rank,
            audit_rank,
            churn,
            elapsed,
            r.racer_name.clone(),
        )
    });
    sorted
}

pub fn build_scorecard_document(arena: &ArenaRecord, racers: &[ArenaRacerRecord]) -> UiDocument {
    let ranked = rank_racers(racers);
    let passing = racers
        .iter()
        .filter(|r| r.verify_exit_code == Some(0))
        .count();
    let verified_audit = racers
        .iter()
        .filter(|r| r.audit_verdict.as_deref() == Some("verified"))
        .count();
    let recommended = arena
        .winner_racer
        .as_deref()
        .or_else(|| ranked.first().map(|r| r.racer_name.as_str()));

    let mut blocks = vec![UiBlock::MetricRow {
        items: vec![
            UiMetric {
                label: "Racers".into(),
                value: racers.len().to_string(),
                delta: None,
                tone: UiTone::Accent,
            },
            UiMetric {
                label: "Verify Pass".into(),
                value: format!("{passing}/{}", racers.len()),
                delta: None,
                tone: if passing > 0 {
                    UiTone::Success
                } else {
                    UiTone::Warning
                },
            },
            UiMetric {
                label: "BS Clean".into(),
                value: format!("{verified_audit}/{}", racers.len()),
                delta: None,
                tone: UiTone::Accent,
            },
            UiMetric {
                label: "Status".into(),
                value: arena.status.to_uppercase(),
                delta: recommended.map(|w| format!("lead: {w}")),
                tone: match arena.status.as_str() {
                    "promoted" => UiTone::Success,
                    "running" => UiTone::Accent,
                    _ => UiTone::Normal,
                },
            },
        ],
    }];

    blocks.push(UiBlock::Table {
        columns: vec![
            "Racer".into(),
            "Provider".into(),
            "Status".into(),
            "Verify".into(),
            "BS Audit".into(),
            "Diff".into(),
        ],
        rows: ranked
            .iter()
            .map(|r| {
                let verify = match r.verify_exit_code {
                    Some(0) => "PASS (0)".into(),
                    Some(code) => format!("FAIL ({code})"),
                    None => "—".into(),
                };
                let diff = match (r.files_changed, r.insertions, r.deletions) {
                    (Some(f), Some(ins), Some(del)) => format!("{f}f +{ins}/-{del}"),
                    _ => "—".into(),
                };
                vec![
                    r.racer_name.clone(),
                    r.provider.clone(),
                    r.status.clone(),
                    verify,
                    r.audit_verdict.clone().unwrap_or_else(|| "pending".into()),
                    diff,
                ]
            })
            .collect(),
    });

    blocks.push(UiBlock::Header {
        title: "Racer Breakdown & Promotion".into(),
        subtitle: Some(format!(
            "Prompt: {}",
            arena.prompt.chars().take(120).collect::<String>()
        )),
        badge: None,
        tone: UiTone::Accent,
    });

    blocks.push(UiBlock::List {
        title: None,
        items: ranked
            .iter()
            .map(|r| {
                let is_winner = arena.winner_racer.as_deref() == Some(r.racer_name.as_str());
                let badge = if is_winner {
                    "WINNER".into()
                } else if r.verify_exit_code == Some(0) {
                    "PASS".into()
                } else if let Some(code) = r.verify_exit_code {
                    format!("EXIT {code}")
                } else {
                    r.status.to_uppercase()
                };
                let tone = if is_winner || r.verify_exit_code == Some(0) {
                    UiTone::Success
                } else if r.verify_exit_code.is_some() {
                    UiTone::Danger
                } else {
                    UiTone::Normal
                };
                UiListItem {
                    id: Some(r.racer_name.clone()),
                    title: format!("{} ({})", r.racer_name, r.provider),
                    subtitle: Some(format!(
                        "worktree: {} · audit: {}",
                        r.worktree_id,
                        r.audit_verdict.as_deref().unwrap_or("pending")
                    )),
                    badge: Some(badge),
                    tone,
                    action: (arena.status != "promoted")
                        .then(|| format!("arena:promote:{}:{}:branch", arena.id, r.racer_name)),
                    payload: Some(json!({
                        "arena_id": arena.id,
                        "winner": r.racer_name,
                        "mode": "branch",
                        "losers": "archive",
                    })),
                }
            })
            .collect(),
    });

    if arena.status != "promoted"
        && let Some(lead) = recommended
    {
        blocks.push(UiBlock::ButtonRow {
            buttons: vec![
                UiButton {
                    id: format!("arena-promote-branch-{lead}"),
                    label: format!("Promote `{lead}` as Branch"),
                    style: "primary".into(),
                    action: format!("arena:promote:{}:{lead}:branch", arena.id),
                    payload: Some(json!({
                        "arena_id": arena.id,
                        "winner": lead,
                        "mode": "branch",
                        "losers": "archive",
                    })),
                    confirm: Some(format!(
                        "Promote `{lead}` to a winner branch and archive other racers?"
                    )),
                },
                UiButton {
                    id: format!("arena-promote-squash-{lead}"),
                    label: format!("Squash `{lead}` into Current"),
                    style: "secondary".into(),
                    action: format!("arena:promote:{}:{lead}:squash", arena.id),
                    payload: Some(json!({
                        "arena_id": arena.id,
                        "winner": lead,
                        "mode": "squash",
                        "losers": "archive",
                    })),
                    confirm: Some(format!(
                        "Squash `{lead}` onto current branch (safety checkpoint taken if dirty)?"
                    )),
                },
            ],
        });
    }

    UiDocument {
        v: UI_SPEC_VERSION,
        title: Some(format!("Agent Arena · {}", arena.id)),
        subtitle: Some(format!(
            "Base {} · D-12 Best-of-N Scorecard",
            &arena.base_commit[..arena.base_commit.len().min(8)]
        )),
        blocks,
        custom_surface: None,
    }
}
