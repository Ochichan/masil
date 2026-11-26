//! Phase 2B: Attention Shield (`masil-agent focus`) & 90-Second Morning/Return
//! Briefing (`masil-agent briefing`), plus shared deterministic secret redaction.

pub mod digest;
pub mod redact;
pub mod shield;

use crate::remote::discovery::default_masil_socket;
use serde_json::json;
use std::path::PathBuf;

/// CLI entrypoint for `masil-agent focus on <25m|60m|until-09:00> | off | status`.
pub fn run_focus(args: &[String]) -> Result<i32, String> {
    let mut socket: Option<PathBuf> = None;
    let mut reason: Option<String> = None;
    let mut json_out = false;
    let mut positional: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" if i + 1 < args.len() => {
                socket = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--reason" if i + 1 < args.len() => {
                reason = Some(args[i + 1].clone());
                i += 2;
            }
            "--json" => {
                json_out = true;
                i += 1;
            }
            "-h" | "--help" | "help" => {
                println!(
                    "masil-agent focus — Deep-work Attention Shield\n\n\
                     Usage:\n  \
                       masil-agent focus on <25m|60m|until-09:00> [--reason TEXT] [--socket PATH] [--json]\n  \
                       masil-agent focus off [--socket PATH] [--json]\n  \
                       masil-agent focus status [--socket PATH] [--json]"
                );
                return Ok(0);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown focus flag: {other}"));
            }
            other => {
                positional.push(other.to_owned());
                i += 1;
            }
        }
    }

    let sock = match socket {
        Some(p) => p,
        None => default_masil_socket()?,
    };
    let sub = positional.first().map(String::as_str).unwrap_or("status");

    match sub {
        "on" => {
            let duration = positional.get(1).map(String::as_str).unwrap_or("25m");
            let status = shield::enable(&sock, duration, reason)?;
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"focus": status}))
                        .map_err(|e| e.to_string())?
                );
            } else {
                let mins = status.remaining_ms / 60_000;
                println!(
                    "Focus Shield ON ({mins}m remaining). Breakthroughs: circuit breaker, hard conflict, critical permission."
                );
            }
            Ok(0)
        }
        "off" => {
            let (status, held) = shield::disable(&sock)?;
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "focus": status,
                        "held_events": held,
                    }))
                    .map_err(|e| e.to_string())?
                );
            } else {
                println!(
                    "Focus Shield OFF ({} held notification(s) ready for briefing).",
                    held.len()
                );
            }
            Ok(0)
        }
        "status" => {
            let status = shield::get_status(&sock)?;
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"focus": status}))
                        .map_err(|e| e.to_string())?
                );
            } else if status.active {
                let mins = status.remaining_ms / 60_000;
                println!(
                    "Focus Shield ON ({mins}m remaining, {} held).",
                    status.held_count
                );
            } else {
                println!("Focus Shield OFF ({} held).", status.held_count);
            }
            Ok(0)
        }
        other => Err(format!(
            "unknown focus subcommand '{other}'; expected on, off, or status"
        )),
    }
}

/// CLI entrypoint for `masil-agent briefing [--since 8h] [--socket PATH] [--json]`.
pub fn run_briefing(args: &[String]) -> Result<i32, String> {
    let mut socket: Option<PathBuf> = None;
    let mut since = "8h".to_owned();
    let mut json_out = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" if i + 1 < args.len() => {
                socket = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--since" if i + 1 < args.len() => {
                since = args[i + 1].clone();
                i += 2;
            }
            "--json" => {
                json_out = true;
                i += 1;
            }
            "-h" | "--help" | "help" => {
                println!(
                    "masil-agent briefing — 90-second deterministic fleet briefing\n\n\
                     Usage:\n  \
                       masil-agent briefing [--since 8h] [--socket PATH] [--json]"
                );
                return Ok(0);
            }
            other => return Err(format!("unknown briefing option: {other}")),
        }
    }

    let report = digest::generate_briefing(socket.as_deref(), &since)?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        println!(
            "=== 90-Second Fleet Briefing (last {}) ===",
            report.window_label
        );
        println!(
            "Turns: {} | Files/LOC: {}/{} | Breakers: {} | Collisions: {} | Held: {}",
            report.metrics.completed_turns,
            report.metrics.files_touched,
            report.metrics.lines_changed,
            report.metrics.breaker_interventions,
            report.metrics.active_collisions,
            report.metrics.held_notifications
        );
        if !report.needs_decision.is_empty() {
            println!("\n🔴 Needs Decision ({}):", report.needs_decision.len());
            for item in &report.needs_decision {
                println!("  - [{}] {} — {}", item.badge, item.title, item.subtitle);
            }
        }
        if !report.ready_to_merge.is_empty() {
            println!("\n🟢 Ready to Merge ({}):", report.ready_to_merge.len());
            for item in &report.ready_to_merge {
                println!("  - [{}] {} — {}", item.badge, item.title, item.subtitle);
            }
        }
        if !report.review_recommended.is_empty() {
            println!(
                "\n🟡 Review Recommended ({}):",
                report.review_recommended.len()
            );
            for item in &report.review_recommended {
                println!("  - [{}] {} — {}", item.badge, item.title, item.subtitle);
            }
        }
        if report.needs_decision.is_empty()
            && report.ready_to_merge.is_empty()
            && report.review_recommended.is_empty()
        {
            println!("\nAll quiet across your fleet.");
        }
    }
    Ok(0)
}
