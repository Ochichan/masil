//! Live interactive prompt extraction for mobile remote control.
//!
//! Combines three sources of truth so mobile users can inspect and answer
//! approvals, questions, plan confirmations, and free-text dialogs across all
//! 24 providers:
//! 1. Structured provider HTTP API (`masil-agent agent requests`) for OpenCode
//!    and Kilo runs started with `--answers`.
//! 2. Live visible pane screen parsing (`capture-pane -p -J`) for TUI dialogs
//!    across Antigravity (`agy`), Claude Code, Codex, Gemini CLI, Cursor, OMP,
//!    Cline, Kiro, Qwen, OpenCode/Kilo without `--answers`, and all others.
//! 3. Pending (unanswered) interactive tool calls from the transcript
//!    (`ask_question`, `AskUserQuestion`, `request_user_input`) to enrich
//!    truncated screen labels with full untruncated question text and options.

use crate::native_ui::native_executable;
use crate::remote::discovery::run_self_agent_json;
use masil_wire::rpc::{
    InteractiveOption, InteractivePrompt, InteractivePromptKind, InteractivePromptMode,
    StructuredQuestion, StructuredQuestionOption, TimelineItem,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::process::Command;

const API_METADATA_KEYS: &[&str] = &[
    "description",
    "command",
    "filePath",
    "filepath",
    "path",
    "url",
    "query",
    "pattern",
];

/// Captures only the currently visible pane viewport (no deep scrollback) so
/// previous conversation output cannot impersonate an active TUI dialog.
pub fn capture_visible_pane(server_socket: &str, pane_id: &str) -> Option<String> {
    if server_socket.is_empty() || pane_id.is_empty() {
        return None;
    }
    let masil = native_executable().ok()?;
    let out = Command::new(masil)
        .arg("-S")
        .arg(server_socket)
        .args(["capture-pane", "-p", "-J", "-t", pane_id])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).into_owned();
    if raw.trim().is_empty() {
        None
    } else {
        Some(raw)
    }
}

/// Collects all currently active interactive prompts for a pane and attaches
/// Phase 3A deterministic Blast-Radius risk assessments & policy matches (D-10).
pub fn collect_live_prompts_with_cwd(
    server_socket: &str,
    pane_id: &str,
    provider: &str,
    state: &str,
    cwd: Option<&str>,
    transcript_items: &[TimelineItem],
) -> Vec<InteractivePrompt> {
    let mut prompts =
        collect_live_prompts_raw(server_socket, pane_id, provider, state, transcript_items);
    if !prompts.is_empty() {
        let sock_path = (!server_socket.is_empty()).then(|| std::path::Path::new(server_socket));
        let (_, rules) = crate::policy::load_effective_rules(sock_path);
        for p in &mut prompts {
            crate::policy::enrich_prompt(p, cwd, &rules);
        }
    }
    prompts
}

fn collect_live_prompts_raw(
    server_socket: &str,
    pane_id: &str,
    provider: &str,
    state: &str,
    transcript_items: &[TimelineItem],
) -> Vec<InteractivePrompt> {
    // 1. Prefer structured HTTP API for OpenCode / Kilo runs with `--answers`.
    // When the structured channel is live, its empty list is authoritative so
    // stale coordinator `blocked` snapshots never synthesize a bogus TUI fallback.
    if matches!(provider, "opencode" | "kilo")
        && !server_socket.is_empty()
        && !pane_id.is_empty()
        && let Some(api_prompts) =
            collect_structured_api_prompts_authoritative(server_socket, pane_id)
    {
        return api_prompts;
    }

    let pending_questions = extract_pending_transcript_questions(transcript_items);

    // Avoid spawning capture-pane on quiescent idle/working turns unless there
    // is a pending unanswered interactive tool call or unknown/blocked state.
    let should_inspect_screen =
        state == "blocked" || state == "unknown" || !pending_questions.is_empty();
    if !should_inspect_screen {
        return Vec::new();
    }

    let Some(screen) = capture_visible_pane(server_socket, pane_id) else {
        if state == "blocked" {
            return vec![generic_blocked_fallback(provider, pane_id, None)];
        }
        return Vec::new();
    };

    if let Some(prompt) = parse_live_tui_screen(provider, state, &screen, &pending_questions) {
        return vec![prompt];
    }

    if state == "blocked" {
        return vec![generic_blocked_fallback(
            provider,
            pane_id,
            Some(clean_screen_excerpt(&screen, 18)),
        )];
    }

    Vec::new()
}

/// Returns `Some(prompts)` when the pane has an active structured `--answers`
/// channel (even if currently empty), or `None` when no channel is configured
/// and callers should fall back to TUI screen parsing.
fn collect_structured_api_prompts_authoritative(
    server_socket: &str,
    pane_id: &str,
) -> Option<Vec<InteractivePrompt>> {
    let val =
        run_self_agent_json(Some(server_socket), &["requests".into(), pane_id.into()]).ok()?;
    let channel = val.get("channel").and_then(Value::as_str).unwrap_or("none");
    if channel == "none" || val.get("requests").and_then(Value::as_array).is_none() {
        return None;
    }
    Some(parse_structured_api_requests(&val))
}

pub fn parse_structured_api_requests(val: &Value) -> Vec<InteractivePrompt> {
    if val.get("channel").and_then(Value::as_str) == Some("none") {
        return Vec::new();
    }
    let Some(requests) = val.get("requests").and_then(Value::as_array) else {
        return Vec::new();
    };
    if requests.is_empty() {
        return Vec::new();
    }

    let mut perm_counts_by_session: HashMap<String, usize> = HashMap::new();
    for req in requests {
        if req.get("kind").and_then(Value::as_str) == Some("permission")
            && let Some(sid) = req.get("session_id").and_then(Value::as_str)
        {
            *perm_counts_by_session.entry(sid.to_owned()).or_insert(0) += 1;
        }
    }

    let mut out = Vec::new();
    for req in requests {
        let Some(req_id) = req.get("id").and_then(Value::as_str) else {
            continue;
        };
        let kind = req.get("kind").and_then(Value::as_str).unwrap_or("");
        let is_subagent = req.get("root").and_then(Value::as_bool) == Some(false);
        let session_id = req.get("session_id").and_then(Value::as_str).unwrap_or("");

        if kind == "permission" {
            let perm_name = req
                .get("permission")
                .and_then(Value::as_str)
                .unwrap_or("permission");
            let patterns: Vec<String> = req
                .get("patterns")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            let always_patterns: Vec<String> = req
                .get("always")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();

            let mut body_lines = Vec::new();
            if !patterns.is_empty() {
                body_lines.push(format!("{perm_name}: {}", patterns.join(", ")));
            }
            if let Some(meta) = req.get("metadata").and_then(Value::as_object) {
                for &key in API_METADATA_KEYS {
                    if let Some(v) = meta.get(key).and_then(Value::as_str)
                        && !v.trim().is_empty()
                    {
                        body_lines.push(format!("{key}: {}", v.trim()));
                    }
                }
                if let Some(diff) = meta.get("diff").and_then(Value::as_str) {
                    let preview: Vec<&str> = diff.lines().take(16).collect();
                    if !preview.is_empty() {
                        body_lines.push(preview.join("\n"));
                    }
                }
            }
            if body_lines.is_empty() {
                body_lines.push(perm_name.to_owned());
            }

            let peers = perm_counts_by_session
                .get(session_id)
                .copied()
                .unwrap_or(0)
                .saturating_sub(1);
            let always_disabled = (peers > 0).then(|| {
                "Another permission in this session is waiting; answer it first or allow once."
                    .to_owned()
            });

            out.push(InteractivePrompt {
                id: format!("api-perm-{req_id}"),
                kind: InteractivePromptKind::Permission,
                mode: InteractivePromptMode::StructuredApi,
                request_id: Some(req_id.to_owned()),
                title: format!("Permission · {perm_name}"),
                body: body_lines.join("\n"),
                step_label: None,
                options: vec![
                    InteractiveOption {
                        id: "once".into(),
                        index: 0,
                        label: "Allow once".into(),
                        description: None,
                        selected: true,
                        checked: None,
                        hotkey: Some("1".into()),
                        is_custom_input: false,
                        is_destructive: false,
                        disabled_reason: None,
                    },
                    InteractiveOption {
                        id: "always".into(),
                        index: 1,
                        label: "Always allow".into(),
                        description: if always_patterns.is_empty() {
                            None
                        } else {
                            Some(always_patterns.join(", "))
                        },
                        selected: false,
                        checked: None,
                        hotkey: Some("2".into()),
                        is_custom_input: false,
                        is_destructive: false,
                        disabled_reason: always_disabled,
                    },
                    InteractiveOption {
                        id: "reject".into(),
                        index: 2,
                        label: "Reject".into(),
                        description: None,
                        selected: false,
                        checked: None,
                        hotkey: Some("3".into()),
                        is_custom_input: false,
                        is_destructive: true,
                        disabled_reason: None,
                    },
                ],
                selected_index: 0,
                multi_select: false,
                allows_free_text: true,
                free_text_only: false,
                allows_cancel: true,
                can_navigate_tabs: false,
                always_patterns,
                session_peers_count: peers,
                is_subagent,
                questions: Vec::new(),
                screen_excerpt: None,
                hint_line: None,
                risk: None,
            });
        } else if kind == "question" {
            let mut questions = Vec::new();
            if let Some(arr) = req.get("questions").and_then(Value::as_array) {
                for q in arr {
                    let question_text = q
                        .get("question")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    let header = q
                        .get("header")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    let custom = q.get("custom").and_then(Value::as_bool).unwrap_or(true);
                    let multiple = q.get("multiple").and_then(Value::as_bool).unwrap_or(false);
                    let opts: Vec<StructuredQuestionOption> = q
                        .get("options")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|o| {
                            let label = o.get("label").and_then(Value::as_str)?.trim().to_owned();
                            if label.is_empty() {
                                return None;
                            }
                            let description = o
                                .get("description")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned);
                            Some(StructuredQuestionOption { label, description })
                        })
                        .collect();
                    questions.push(StructuredQuestion {
                        header,
                        question: question_text,
                        options: opts,
                        custom,
                        multiple,
                    });
                }
            }
            let first = questions.first();
            let title = first
                .and_then(|q| (!q.header.is_empty()).then(|| q.header.clone()))
                .unwrap_or_else(|| "Question".into());
            let body = first.map(|q| q.question.clone()).unwrap_or_default();
            let step_label = (questions.len() > 1).then(|| format!("1/{}", questions.len()));
            let options: Vec<InteractiveOption> = first
                .map(|q| {
                    q.options
                        .iter()
                        .enumerate()
                        .map(|(i, o)| InteractiveOption {
                            id: format!("opt-{i}"),
                            index: i,
                            label: o.label.clone(),
                            description: o.description.clone(),
                            selected: i == 0,
                            checked: q.multiple.then_some(false),
                            hotkey: (i < 9).then(|| (i + 1).to_string()),
                            is_custom_input: false,
                            is_destructive: false,
                            disabled_reason: None,
                        })
                        .collect()
                })
                .unwrap_or_default();

            out.push(InteractivePrompt {
                id: format!("api-q-{req_id}"),
                kind: InteractivePromptKind::Question,
                mode: InteractivePromptMode::StructuredApi,
                request_id: Some(req_id.to_owned()),
                title,
                body,
                step_label,
                options,
                selected_index: 0,
                multi_select: first.is_some_and(|q| q.multiple),
                allows_free_text: first.is_none_or(|q| q.custom),
                free_text_only: first.is_some_and(|q| q.options.is_empty() && q.custom),
                allows_cancel: true,
                can_navigate_tabs: questions.len() > 1,
                always_patterns: Vec::new(),
                session_peers_count: 0,
                is_subagent,
                questions,
                screen_excerpt: None,
                hint_line: None,
                risk: None,
            });
        }
    }
    out
}

/// Checks whether the tail of the transcript has an unanswered interactive
/// question tool call (`ask_question`, `AskUserQuestion`, `request_user_input`).
pub fn extract_pending_transcript_questions(items: &[TimelineItem]) -> Vec<StructuredQuestion> {
    for item in items.iter().rev() {
        match item {
            TimelineItem::Thinking { .. }
            | TimelineItem::ApprovalBanner { .. }
            | TimelineItem::PluginCard { .. }
            | TimelineItem::SystemNotice { .. } => continue,
            TimelineItem::ToolCall {
                name,
                args_json,
                output,
                ..
            } => {
                if output.is_some() {
                    return Vec::new();
                }
                if !matches!(
                    name.as_str(),
                    "ask_question" | "AskUserQuestion" | "request_user_input"
                ) {
                    return Vec::new();
                }
                return parse_transcript_question_args(args_json.as_ref());
            }
            _ => return Vec::new(),
        }
    }
    Vec::new()
}

fn parse_transcript_question_args(args: Option<&Value>) -> Vec<StructuredQuestion> {
    let Some(args) = args else {
        return Vec::new();
    };
    let parsed_val: Value;
    let obj = match args {
        Value::Object(_) => args,
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(v) => {
                parsed_val = v;
                &parsed_val
            }
            Err(_) => return Vec::new(),
        },
        _ => return Vec::new(),
    };

    let questions_val = match obj.get("questions") {
        Some(Value::Array(arr)) => Value::Array(arr.clone()),
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        _ => return Vec::new(),
    };
    let Some(arr) = questions_val.as_array() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for q in arr {
        let question = q
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if question.is_empty() {
            continue;
        }
        let header = q
            .get("header")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        let multiple = q
            .get("is_multi_select")
            .or_else(|| q.get("multiSelect"))
            .or_else(|| q.get("multiple"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let custom = q.get("custom").and_then(Value::as_bool).unwrap_or(true);
        let mut options = Vec::new();
        if let Some(raw_opts) = q.get("options").and_then(Value::as_array) {
            for opt in raw_opts {
                match opt {
                    Value::String(s) if !s.trim().is_empty() => {
                        options.push(StructuredQuestionOption {
                            label: s.trim().to_owned(),
                            description: None,
                        });
                    }
                    Value::Object(map) => {
                        if let Some(label) = map
                            .get("label")
                            .or_else(|| map.get("title"))
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            let description = map
                                .get("description")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned);
                            options.push(StructuredQuestionOption {
                                label: label.to_owned(),
                                description,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        out.push(StructuredQuestion {
            header,
            question,
            options,
            custom,
            multiple,
        });
    }
    out
}

/// Parses a captured live terminal viewport into a structured
/// `InteractivePrompt` when an interactive approval, question, confirmation, or
/// free-text instruction prompt is currently visible.
pub fn parse_live_tui_screen(
    provider: &str,
    state: &str,
    screen: &str,
    pending_questions: &[StructuredQuestion],
) -> Option<InteractivePrompt> {
    let raw_lines: Vec<String> = screen.lines().map(|l| l.trim_end().to_owned()).collect();
    let non_empty: Vec<&str> = raw_lines
        .iter()
        .map(String::as_str)
        .filter(|l| !l.trim().is_empty())
        .collect();
    if non_empty.is_empty() {
        return None;
    }

    let bottom_tail = &non_empty[non_empty.len().saturating_sub(28)..];
    let bottom_joined_lower = bottom_tail
        .iter()
        .map(|l| l.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("\n");

    let hints = extract_footer_hints(bottom_tail);

    // Guard against false positives when the provider is NOT known to be blocked.
    if state != "blocked" {
        if looks_like_idle_composer(bottom_tail) && !hints.strong_dialog_signal {
            return None;
        }
        if state == "working" && !hints.strong_dialog_signal && pending_questions.is_empty() {
            return None;
        }
    }

    let dialog_lines = extract_dialog_region(bottom_tail);
    let cleaned_excerpt = clean_screen_excerpt(screen, 18);

    // 1. Try parsing interactive options (numbered, cursor-marked, checkbox, or bracket hotkeys).
    if let Some(parsed) = parse_options_from_region(&dialog_lines, &hints, state) {
        let mut options = parsed.options;
        let selected_index = parsed.selected_index;
        let mut multi_select = parsed.multi_select || hints.has_space_toggle;
        let mut title = parsed.title;
        let mut body = parsed.body;
        let mut step_label = detect_step_label(&dialog_lines);

        // Enrich with full untruncated transcript question details when available.
        let matched_q = match_pending_question(
            &title,
            &body,
            &options,
            step_label.as_deref(),
            pending_questions,
        );
        if let Some((q_idx, q)) = matched_q {
            if step_label.is_none() && pending_questions.len() > 1 {
                step_label = Some(format!("{}/{}", q_idx + 1, pending_questions.len()));
            }
            if !q.header.is_empty() && (title.is_empty() || title.eq_ignore_ascii_case("question"))
            {
                title = q.header.clone();
            }
            if !q.question.is_empty() && q.question.chars().count() >= body.chars().count() {
                body = q.question.clone();
            }
            if q.multiple {
                multi_select = true;
            }
            for opt in &mut options {
                if opt.is_custom_input {
                    continue;
                }
                if let Some(full) = q.options.get(opt.index) {
                    let screen_prefix: String = opt.label.chars().take(18).collect();
                    if full.label.starts_with(&screen_prefix)
                        || opt.label.ends_with('…')
                        || opt.label.ends_with("...")
                    {
                        opt.label = full.label.clone();
                    }
                    if opt.description.is_none() && full.description.is_some() {
                        opt.description = full.description.clone();
                    }
                }
            }
        }

        let allows_free_text = options.iter().any(|o| o.is_custom_input)
            || bottom_joined_lower.contains("type to deny")
            || bottom_joined_lower.contains("type your own");
        let kind = classify_prompt_kind(
            &title,
            &body,
            &options,
            multi_select,
            !pending_questions.is_empty(),
            false,
        );
        if title.is_empty() {
            title = default_title_for_kind(kind, provider);
        }

        let id = prompt_digest(
            "tui",
            kind,
            None,
            &title,
            &body,
            &options,
            selected_index,
            false,
        );
        let can_navigate_tabs = hints.has_tab || step_label.is_some();
        return Some(InteractivePrompt {
            id,
            kind,
            mode: InteractivePromptMode::TerminalTui,
            request_id: None,
            title,
            body,
            step_label,
            options,
            selected_index,
            multi_select,
            allows_free_text,
            free_text_only: false,
            allows_cancel: hints.has_esc || state == "blocked",
            can_navigate_tabs,
            always_patterns: Vec::new(),
            session_peers_count: 0,
            is_subagent: false,
            questions: pending_questions.to_vec(),
            screen_excerpt: Some(cleaned_excerpt),
            hint_line: hints.hint_line,
            risk: None,
        });
    }

    // 2. Check for pure free-text instruction / reject-reason dialogs.
    if let Some((title, body)) = parse_free_text_dialog(&dialog_lines, &bottom_joined_lower) {
        let id = prompt_digest(
            "tui",
            InteractivePromptKind::Input,
            None,
            &title,
            &body,
            &[],
            0,
            true,
        );
        return Some(InteractivePrompt {
            id,
            kind: InteractivePromptKind::Input,
            mode: InteractivePromptMode::TerminalTui,
            request_id: None,
            title,
            body,
            step_label: detect_step_label(&dialog_lines),
            options: Vec::new(),
            selected_index: 0,
            multi_select: false,
            allows_free_text: true,
            free_text_only: true,
            allows_cancel: true,
            can_navigate_tabs: false,
            always_patterns: Vec::new(),
            session_peers_count: 0,
            is_subagent: false,
            questions: pending_questions.to_vec(),
            screen_excerpt: Some(cleaned_excerpt),
            hint_line: hints.hint_line,
            risk: None,
        });
    }

    // 3. Check for inline [y/n], Press Enter, or Press any key confirmations.
    if let Some(prompt) = parse_inline_confirmation(
        provider,
        &dialog_lines,
        &bottom_joined_lower,
        &hints,
        &cleaned_excerpt,
    ) {
        return Some(prompt);
    }

    // 4. If `state == "blocked"` (confirmed by manifest or hooks) but screen layout
    //    is novel, return an honest screen-backed confirmation card.
    if state == "blocked" {
        return Some(generic_blocked_fallback(
            provider,
            "",
            Some(cleaned_excerpt),
        ));
    }

    None
}

#[derive(Default)]
struct FooterHints {
    hint_line: Option<String>,
    has_arrows: bool,
    has_enter: bool,
    has_esc: bool,
    has_space_toggle: bool,
    has_tab: bool,
    strong_dialog_signal: bool,
}

fn extract_footer_hints(lines: &[&str]) -> FooterHints {
    let mut out = FooterHints::default();
    let tail = &lines[lines.len().saturating_sub(5)..];
    let mut hint_parts = Vec::new();

    for raw in tail {
        let trimmed = strip_box_edges(raw);
        let lower = trimmed.to_ascii_lowercase();
        if trimmed.is_empty() || is_horizontal_rule(&trimmed) {
            continue;
        }
        let is_hint_line = (lower.contains("esc")
            && (lower.contains("cancel")
                || lower.contains("dismiss")
                || lower.contains("close")
                || lower.contains("deny")
                || lower.contains("exit")
                || lower.contains("skip")))
            || ((lower.contains("↑") || lower.contains("↓") || lower.contains("arrow"))
                && (lower.contains("navigate")
                    || lower.contains("select")
                    || lower.contains("choose")
                    || lower.contains("confirm")
                    || lower.contains("submit")
                    || lower.contains("move")))
            || (lower.contains("enter")
                && (lower.contains("confirm")
                    || lower.contains("select")
                    || lower.contains("submit")
                    || lower.contains("continue")
                    || lower.contains("toggle")))
            || (lower.contains("space") && (lower.contains("toggle") || lower.contains("select")))
            || (lower.contains("tab") && lower.contains("amend"))
            || lower.contains("press the key shown");

        if is_hint_line {
            if lower.contains("↑")
                || lower.contains("↓")
                || lower.contains("arrow")
                || lower.contains("navigate")
                || lower.contains("choose")
            {
                out.has_arrows = true;
            }
            if lower.contains("enter") || lower.contains("return") {
                out.has_enter = true;
            }
            if lower.contains("esc") {
                out.has_esc = true;
            }
            if lower.contains("space") && (lower.contains("toggle") || lower.contains("select")) {
                out.has_space_toggle = true;
            }
            if lower.contains("tab") || lower.contains("←/→") || lower.contains("←→") {
                out.has_tab = true;
            }
            hint_parts.push(trimmed);
        }
    }

    if !hint_parts.is_empty() {
        out.hint_line = Some(hint_parts.join(" · "));
    }
    out.strong_dialog_signal = ((out.has_arrows || out.has_esc) && out.has_enter)
        || (out.has_space_toggle && (out.has_enter || out.has_esc));
    out
}

fn looks_like_idle_composer(bottom_tail: &[&str]) -> bool {
    let last4 = &bottom_tail[bottom_tail.len().saturating_sub(4)..];
    let joined = last4.join("\n").to_ascii_lowercase();
    joined.contains("? for shortcuts")
        || joined.contains("ask codex to do anything")
        || joined.contains("type your message")
        || joined.contains("add a follow-up")
        || joined.contains("plan, search, build anything")
        || (joined.contains("context ") && (joined.contains("% used") || joined.contains("% left")))
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.chars().count() < 6 {
        return false;
    }
    let rule_chars = trimmed
        .chars()
        .filter(|c| {
            matches!(
                c,
                '-' | '─'
                    | '━'
                    | '═'
                    | '▀'
                    | '▄'
                    | '╭'
                    | '╮'
                    | '╰'
                    | '╯'
                    | '├'
                    | '┤'
                    | '┼'
                    | ' '
            )
        })
        .count();
    rule_chars * 10 >= trimmed.chars().count() * 8
}

fn strip_box_edges(line: &str) -> String {
    let mut s = line.trim();
    for ch in ['│', '┃', '║'] {
        if let Some(rest) = s.strip_prefix(ch) {
            s = rest.trim_start();
        }
        if let Some(rest) = s.strip_suffix(ch) {
            s = rest.trim_end();
        }
    }
    s.trim().to_owned()
}

fn extract_dialog_region(bottom_tail: &[&str]) -> Vec<String> {
    // 1. If there is a rounded/lined box modal near the bottom (╭─...─╮ ... ╰─...─╯),
    //    extract its inner lines (plus footer lines right below).
    let mut box_top = None;
    let mut box_bottom = None;
    for (i, line) in bottom_tail.iter().enumerate() {
        let t = line.trim();
        if t.starts_with('╭') || t.starts_with('┌') {
            box_top = Some(i);
        }
        if t.starts_with('╰') || t.starts_with('└') {
            box_bottom = Some(i);
        }
    }
    if let (Some(top), Some(bot)) = (box_top, box_bottom)
        && top < bot
        && bottom_tail.len() - bot <= 8
    {
        let mut out = Vec::new();
        // Keep box title if embedded in the top border (`╭─ Allow tool: bash ──╮`).
        let top_title = bottom_tail[top]
            .trim()
            .trim_matches(|c: char| matches!(c, '╭' | '╮' | '┌' | '┐' | '─' | '━' | ' '));
        if !top_title.is_empty() {
            out.push(top_title.to_owned());
        }
        for line in &bottom_tail[top + 1..bot] {
            let cleaned = strip_box_edges(line);
            if !cleaned.is_empty() && !is_horizontal_rule(&cleaned) {
                out.push(cleaned);
            }
        }
        return out;
    }

    // 2. Otherwise check horizontal rules (Claude / Agy / Kiro style).
    let rules: Vec<usize> = bottom_tail
        .iter()
        .enumerate()
        .filter(|(_, l)| is_horizontal_rule(l))
        .map(|(i, _)| i)
        .collect();
    if let Some(&last_rule) = rules.last() {
        let after_last = &bottom_tail[last_rule + 1..];
        let slice = if after_last.len() >= 3 {
            after_last
        } else if rules.len() >= 2 {
            let prev_rule = rules[rules.len() - 2];
            &bottom_tail[prev_rule + 1..]
        } else {
            bottom_tail
        };
        return slice
            .iter()
            .filter(|l| !is_horizontal_rule(l))
            .map(|l| strip_box_edges(l))
            .filter(|l| !l.is_empty())
            .collect();
    }

    bottom_tail
        .iter()
        .filter(|l| !is_horizontal_rule(l))
        .map(|l| strip_box_edges(l))
        .filter(|l| !l.is_empty())
        .collect()
}

struct ParsedOptionsRegion {
    title: String,
    body: String,
    options: Vec<InteractiveOption>,
    selected_index: usize,
    multi_select: bool,
}

#[derive(Debug)]
struct RawParsedOption {
    line_idx: usize,
    label: String,
    description: Option<String>,
    selected: bool,
    checked: Option<bool>,
    hotkey: Option<String>,
    numbered: bool,
}

fn is_footer_or_status_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    (lower.contains("esc")
        && (lower.contains("cancel")
            || lower.contains("dismiss")
            || lower.contains("close")
            || lower.contains("exit")
            || lower.contains("interrupt")))
        || ((lower.contains("↑") || lower.contains("↓") || lower.contains("arrow"))
            && (lower.contains("navigate")
                || lower.contains("select")
                || lower.contains("choose")
                || lower.contains("confirm")
                || lower.contains("submit")))
        || (lower.contains("enter")
            && (lower.contains("confirm")
                || lower.contains("select")
                || lower.contains("submit")
                || lower.contains("toggle")))
        || (lower.contains("tab") && lower.contains("amend"))
        || lower.contains("ctrl+e to explain")
        || lower.contains("ctrl+p commands")
        || lower.starts_with("• opencode ")
        || lower.starts_with("• kilo ")
        || lower.contains("use arrow keys to navigate")
}

fn strip_cursor_prefix(s: &str) -> (&str, bool) {
    let trimmed = s.trim_start();
    for marker in ['❯', '›', '●', '▶', '▸', '→', '>'] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            // Require whitespace after cursor marker (avoid `>foo` or `->`).
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                return (rest.trim_start(), true);
            }
        }
    }
    (trimmed, false)
}

fn strip_checkbox_prefix(s: &str) -> (&str, Option<bool>) {
    let t = s.trim_start();
    for (tok, state) in [
        ("[ ]", false),
        ("[x]", true),
        ("[X]", true),
        ("[✓]", true),
        ("[✔]", true),
        ("☐", false),
        ("☑", true),
        ("◯", false),
        ("◉", true),
    ] {
        if let Some(rest) = t.strip_prefix(tok)
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return (rest.trim_start(), Some(state));
        }
    }
    (t, None)
}

fn strip_number_prefix(s: &str) -> Option<(String, &str)> {
    let t = s.trim_start();
    let digits_len = t.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits_len == 0 || digits_len > 2 {
        return None;
    }
    let after_digits = &t[digits_len..];
    let rest = after_digits
        .strip_prefix('.')
        .or_else(|| after_digits.strip_prefix(')'))?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some((t[..digits_len].to_owned(), rest.trim_start()))
}

fn strip_bracket_hotkey(s: &str) -> Option<(String, &str)> {
    let t = s.trim_start();
    let bytes = t.as_bytes();
    if bytes.len() >= 4
        && bytes[0] == b'['
        && bytes[2] == b']'
        && bytes[1].is_ascii_alphanumeric()
        && bytes[3].is_ascii_whitespace()
    {
        let key = (bytes[1] as char).to_ascii_lowercase().to_string();
        return Some((key, t[3..].trim_start()));
    }
    None
}

fn extract_trailing_hotkey(label: &str) -> Option<String> {
    let lower = label.to_ascii_lowercase();
    if lower.ends_with("(y)") || lower.contains(" (y)") {
        return Some("y".into());
    }
    if lower.ends_with("(n)") || lower.contains(" (n)") {
        return Some("n".into());
    }
    if lower.ends_with("(p)") {
        return Some("p".into());
    }
    if lower.ends_with("(esc)") || lower.contains("(esc or n)") {
        return Some("\x1b".into());
    }
    None
}

/// Parses inline bracketed hotkey pairs on one line, e.g. `[y] Approve  [n] Deny`.
fn parse_inline_bracket_options(line: &str) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut rest = line.trim();
    while let Some(idx) = rest.find('[') {
        let slice = &rest[idx..];
        let Some((key, after)) = strip_bracket_hotkey(slice) else {
            break;
        };
        let next_bracket = after.find('[').unwrap_or(after.len());
        let label = after[..next_bracket]
            .trim()
            .trim_end_matches(['·', '|', '/', ','])
            .trim();
        if !label.is_empty() {
            out.push((key, label.to_owned()));
        }
        rest = &after[next_bracket..];
    }
    (out.len() >= 2).then_some(out)
}

fn parse_single_option_line(line: &str, idx: usize) -> Option<RawParsedOption> {
    if is_footer_or_status_line(line) {
        return None;
    }
    let (after_cursor, has_cursor) = strip_cursor_prefix(line);
    let (after_cb1, cb1) = strip_checkbox_prefix(after_cursor);

    // Form 1: Numbered option (`1. Yes`, `❯ 2. Always`, `❯ [ ] 1. Auth`)
    if let Some((num, rest)) = strip_number_prefix(after_cb1) {
        let (label_str, cb2) = strip_checkbox_prefix(rest);
        let label = label_str.trim();
        if label.is_empty() {
            return None;
        }
        let hotkey = extract_trailing_hotkey(label).or(Some(num));
        return Some(RawParsedOption {
            line_idx: idx,
            label: label.to_owned(),
            description: None,
            selected: has_cursor,
            checked: cb1.or(cb2),
            hotkey,
            numbered: true,
        });
    }

    // Form 2: Bracketed hotkey option (`▶ [a] Trust this workspace`, `[q] Quit`)
    if let Some((key, rest)) = strip_bracket_hotkey(after_cb1) {
        // Don't treat a multi-option single line (`[y] Approve  [n] Deny`) as 1 option.
        if parse_inline_bracket_options(after_cb1).is_some() {
            return None;
        }
        let label = rest.trim();
        if label.is_empty() {
            return None;
        }
        return Some(RawParsedOption {
            line_idx: idx,
            label: label.to_owned(),
            description: None,
            selected: has_cursor,
            checked: cb1,
            hotkey: Some(key),
            numbered: false,
        });
    }

    // Form 3: Checkbox option without number (`❯ [ ] Option A`, `[✓] Option B`)
    if let Some(checked) = cb1 {
        let label = after_cb1.trim();
        if label.is_empty() {
            return None;
        }
        return Some(RawParsedOption {
            line_idx: idx,
            label: label.to_owned(),
            description: None,
            selected: has_cursor,
            checked: Some(checked),
            hotkey: extract_trailing_hotkey(label),
            numbered: false,
        });
    }

    // Form 4: Unnumbered cursor-marked choice (`> Yes, I trust this folder`, `❯ Accept`)
    if has_cursor {
        let label = after_cursor.trim();
        // Exclude empty composer or headings ending with `?` / `:` (except "Other:").
        if label.is_empty()
            || label.ends_with('?')
            || label.to_ascii_lowercase().starts_with("you are in ")
        {
            return None;
        }
        return Some(RawParsedOption {
            line_idx: idx,
            label: label.to_owned(),
            description: None,
            selected: true,
            checked: None,
            hotkey: extract_trailing_hotkey(label),
            numbered: false,
        });
    }

    None
}

fn is_likely_unnumbered_sibling(line: &str) -> bool {
    if is_footer_or_status_line(line) || is_horizontal_rule(line) {
        return false;
    }
    let t = line.trim();
    if t.is_empty() || t.chars().count() > 96 {
        return false;
    }
    if t.ends_with('?') || t.ends_with(':') {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("command:")
        || lower.starts_with("file:")
        || lower.starts_with("path:")
        || lower.starts_with("accessing ")
    {
        return false;
    }
    true
}

fn is_known_unnumbered_choice_label(line: &str) -> bool {
    let lower = line.trim().to_ascii_lowercase();
    if lower.contains(':') || lower.ends_with('?') {
        return false;
    }
    matches!(
        lower.as_str(),
        "yes"
            | "no"
            | "allow"
            | "allow once"
            | "always allow"
            | "approve"
            | "deny"
            | "reject"
            | "accept"
            | "decline"
            | "cancel"
            | "skip"
            | "quit"
            | "other"
            | "no, exit"
            | "don't trust"
            | "trust this folder"
    ) || lower.starts_with("yes,")
        || lower.starts_with("no,")
        || lower.starts_with("approve ")
        || lower.starts_with("allow ")
        || lower.starts_with("deny ")
        || lower.starts_with("trust ")
        || lower.starts_with("run ")
        || lower.starts_with("proceed ")
        || lower.starts_with("skip ")
        || lower.starts_with("keep ")
}

fn parse_options_from_region(
    lines: &[String],
    hints: &FooterHints,
    state: &str,
) -> Option<ParsedOptionsRegion> {
    // Check for inline bracketed options like `[y] Approve  [n] Deny` first.
    for (idx, line) in lines.iter().enumerate().rev() {
        if let Some(pairs) = parse_inline_bracket_options(line) {
            let context = &lines[..idx];
            let (title, body) = split_title_and_body(context);
            let options: Vec<InteractiveOption> = pairs
                .into_iter()
                .enumerate()
                .map(|(i, (key, label))| InteractiveOption {
                    id: format!("opt-{i}"),
                    index: i,
                    is_destructive: is_destructive_label(&label),
                    is_custom_input: is_custom_input_label(&label),
                    label,
                    description: None,
                    selected: i == 0,
                    checked: None,
                    hotkey: Some(key),
                    disabled_reason: None,
                })
                .collect();
            return Some(ParsedOptionsRegion {
                title,
                body,
                options,
                selected_index: 0,
                multi_select: false,
            });
        }
    }

    // Parse line-by-line options.
    let mut parsed: Vec<Option<RawParsedOption>> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| parse_single_option_line(l, i))
        .collect();

    // Expand unnumbered siblings around a cursor-marked unnumbered option
    // (e.g., `> Yes, I trust this folder` + `No, exit`, or `❯ Accept` + `Decline`,
    // or OMP's `Approve` / `Approve bash for this session` / `Deny`).
    let has_numbered_or_cb = parsed
        .iter()
        .filter_map(Option::as_ref)
        .filter(|o| o.numbered || o.checked.is_some() || o.hotkey.is_some())
        .count()
        >= 2;

    if !has_numbered_or_cb {
        // Case A: Cursor-marked unnumbered option with plain siblings adjacent to it.
        if let Some(cursor_pos) = parsed
            .iter()
            .rposition(|o| o.as_ref().is_some_and(|x| x.selected && !x.numbered))
        {
            // Expand downwards for up to 5 sibling options.
            let mut j = cursor_pos + 1;
            while j < lines.len() && j <= cursor_pos + 5 {
                if parsed[j].is_none() && is_likely_unnumbered_sibling(&lines[j]) {
                    // Check if the next line looks like a description of the previous choice
                    // (e.g. Kimi folder trust: `Trust this folder` / `Enable project MCP...` / `Don't trust`).
                    if is_known_unnumbered_choice_label(&lines[j]) || lines[j].chars().count() <= 48
                    {
                        parsed[j] = Some(RawParsedOption {
                            line_idx: j,
                            label: lines[j].trim().to_owned(),
                            description: None,
                            selected: false,
                            checked: None,
                            hotkey: extract_trailing_hotkey(&lines[j]),
                            numbered: false,
                        });
                    } else if let Some(prev) = parsed[j - 1].as_mut()
                        && prev.description.is_none()
                    {
                        prev.description = Some(lines[j].trim().to_owned());
                    }
                    j += 1;
                } else {
                    break;
                }
            }
            // Expand upwards for siblings before the cursor (when cursor moved down to option 2/3).
            if cursor_pos > 0 {
                let mut k = cursor_pos - 1;
                loop {
                    if parsed[k].is_none() && is_known_unnumbered_choice_label(&lines[k]) {
                        parsed[k] = Some(RawParsedOption {
                            line_idx: k,
                            label: lines[k].trim().to_owned(),
                            description: None,
                            selected: false,
                            checked: None,
                            hotkey: extract_trailing_hotkey(&lines[k]),
                            numbered: false,
                        });
                        if k == 0 {
                            break;
                        }
                        k -= 1;
                    } else {
                        break;
                    }
                }
            }
        } else {
            // Case B: No ASCII cursor marker (e.g., highlight rendered only by ANSI bg color
            // as in OMP's `Approve / Approve bash for this session / Deny`).
            for (i, line) in lines.iter().enumerate() {
                if parsed[i].is_none() && is_known_unnumbered_choice_label(line) {
                    parsed[i] = Some(RawParsedOption {
                        line_idx: i,
                        label: line.trim().to_owned(),
                        description: None,
                        selected: false,
                        checked: None,
                        hotkey: extract_trailing_hotkey(line),
                        numbered: false,
                    });
                }
            }
        }
    }

    // Collect contiguous option groups (allowing at most 1 non-option description line between items).
    let mut groups: Vec<Vec<RawParsedOption>> = Vec::new();
    let mut cur: Vec<RawParsedOption> = Vec::new();
    let mut gap = 0usize;

    for (idx, item) in parsed.into_iter().enumerate() {
        if let Some(opt) = item {
            cur.push(opt);
            gap = 0;
        } else if !cur.is_empty() && !is_footer_or_status_line(&lines[idx]) && gap == 0 {
            // Treat a single intervening descriptive line as the previous option's description
            // if another option follows immediately after.
            gap = 1;
        } else {
            if !cur.is_empty() {
                groups.push(std::mem::take(&mut cur));
            }
            gap = 0;
        }
    }
    if !cur.is_empty() {
        groups.push(cur);
    }

    // Pick the bottom-most group with >= 2 options (or 1 option if strong blocker signal).
    let mut chosen = groups.into_iter().rev().find(|g| {
        g.len() >= 2 || (g.len() == 1 && (state == "blocked" || hints.strong_dialog_signal))
    })?;

    // Attach 1-line descriptions that sat directly between option rows.
    for i in 0..chosen.len() {
        let start = chosen[i].line_idx;
        let next = chosen.get(i + 1).map(|o| o.line_idx).unwrap_or(start + 1);
        if next == start + 2 && chosen[i].description.is_none() {
            let mid = lines[start + 1].trim();
            if !mid.is_empty() && !is_footer_or_status_line(mid) {
                chosen[i].description = Some(mid.to_owned());
            }
        }
    }

    // Require strong dialog context when there are no cursor/checkbox/hotkey hints
    // (avoid turning ordinary numbered prose lists into buttons!).
    let has_cursor = chosen.iter().any(|o| o.selected);
    let has_checkbox = chosen.iter().any(|o| o.checked.is_some());
    let has_non_num_hotkey = chosen.iter().any(|o| {
        o.hotkey
            .as_deref()
            .is_some_and(|h| !h.chars().all(|c| c.is_ascii_digit()))
    });
    let all_known_choices = chosen
        .iter()
        .all(|o| is_known_unnumbered_choice_label(&o.label));
    if !has_cursor
        && !has_checkbox
        && !has_non_num_hotkey
        && !all_known_choices
        && !hints.strong_dialog_signal
        && state != "blocked"
    {
        return None;
    }

    let first_opt_line = chosen[0].line_idx;
    let context_lines: Vec<String> = lines[..first_opt_line]
        .iter()
        .filter(|l| !is_footer_or_status_line(l))
        .cloned()
        .collect();
    let (title, body) = split_title_and_body(&context_lines);

    let selected_index = chosen.iter().position(|o| o.selected).unwrap_or(0);
    let multi_select = has_checkbox;

    let options: Vec<InteractiveOption> = chosen
        .into_iter()
        .enumerate()
        .map(|(i, raw)| InteractiveOption {
            id: format!("opt-{i}"),
            index: i,
            is_destructive: is_destructive_label(&raw.label),
            is_custom_input: is_custom_input_label(&raw.label),
            label: raw.label,
            description: raw.description,
            selected: i == selected_index,
            checked: raw.checked,
            hotkey: raw.hotkey,
            disabled_reason: None,
        })
        .collect();

    Some(ParsedOptionsRegion {
        title,
        body,
        options,
        selected_index,
        multi_select,
    })
}

fn parse_free_text_dialog(lines: &[String], bottom_lower: &str) -> Option<(String, String)> {
    let is_free_text = bottom_lower.contains("what to do differently")
        || bottom_lower.contains("what to do instead")
        || bottom_lower.contains("your answer:")
        || bottom_lower.contains("직접 입력:")
        || bottom_lower.contains("enter your feedback")
        || bottom_lower.contains("type your response");
    if !is_free_text {
        return None;
    }
    let content: Vec<String> = lines
        .iter()
        .filter(|l| !is_footer_or_status_line(l))
        .cloned()
        .collect();
    let (title, body) = split_title_and_body(&content);
    Some((
        if title.is_empty() {
            "Input requested".into()
        } else {
            title
        },
        body,
    ))
}

fn parse_inline_confirmation(
    provider: &str,
    lines: &[String],
    bottom_lower: &str,
    hints: &FooterHints,
    cleaned_excerpt: &str,
) -> Option<InteractivePrompt> {
    let content: Vec<String> = lines
        .iter()
        .filter(|l| !is_footer_or_status_line(l))
        .cloned()
        .collect();
    let (title, body) = split_title_and_body(&content);

    if bottom_lower.contains("[y/n]")
        || bottom_lower.contains("(y/n)")
        || bottom_lower.contains("yes (y)")
    {
        let options = vec![
            InteractiveOption {
                id: "yes".into(),
                index: 0,
                label: "Yes (y)".into(),
                description: None,
                selected: true,
                checked: None,
                hotkey: Some("y\r".into()),
                is_custom_input: false,
                is_destructive: false,
                disabled_reason: None,
            },
            InteractiveOption {
                id: "no".into(),
                index: 1,
                label: "No (n)".into(),
                description: None,
                selected: false,
                checked: None,
                hotkey: Some("n\r".into()),
                is_custom_input: false,
                is_destructive: true,
                disabled_reason: None,
            },
        ];
        let eff_title = if title.is_empty() {
            format!("Confirmation · {provider}")
        } else {
            title
        };
        let id = prompt_digest(
            "tui",
            InteractivePromptKind::Confirmation,
            None,
            &eff_title,
            &body,
            &options,
            0,
            false,
        );
        return Some(InteractivePrompt {
            id,
            kind: InteractivePromptKind::Confirmation,
            mode: InteractivePromptMode::TerminalTui,
            request_id: None,
            title: eff_title,
            body,
            step_label: None,
            options,
            selected_index: 0,
            multi_select: false,
            allows_free_text: false,
            free_text_only: false,
            allows_cancel: true,
            can_navigate_tabs: false,
            always_patterns: Vec::new(),
            session_peers_count: 0,
            is_subagent: false,
            questions: Vec::new(),
            screen_excerpt: Some(cleaned_excerpt.to_owned()),
            hint_line: hints.hint_line.clone(),
            risk: None,
        });
    }

    if bottom_lower.contains("press enter to continue") || bottom_lower.contains("press any key") {
        let options = vec![InteractiveOption {
            id: "continue".into(),
            index: 0,
            label: "Continue (Enter)".into(),
            description: None,
            selected: true,
            checked: None,
            hotkey: Some("\r".into()),
            is_custom_input: false,
            is_destructive: false,
            disabled_reason: None,
        }];
        let eff_title = if title.is_empty() {
            format!("Action required · {provider}")
        } else {
            title
        };
        let id = prompt_digest(
            "tui",
            InteractivePromptKind::Confirmation,
            None,
            &eff_title,
            &body,
            &options,
            0,
            false,
        );
        return Some(InteractivePrompt {
            id,
            kind: InteractivePromptKind::Confirmation,
            mode: InteractivePromptMode::TerminalTui,
            request_id: None,
            title: eff_title,
            body,
            step_label: None,
            options,
            selected_index: 0,
            multi_select: false,
            allows_free_text: false,
            free_text_only: false,
            allows_cancel: true,
            can_navigate_tabs: false,
            always_patterns: Vec::new(),
            session_peers_count: 0,
            is_subagent: false,
            questions: Vec::new(),
            screen_excerpt: Some(cleaned_excerpt.to_owned()),
            hint_line: hints.hint_line.clone(),
            risk: None,
        });
    }

    None
}

fn generic_blocked_fallback(
    provider: &str,
    pane_id: &str,
    screen_excerpt: Option<String>,
) -> InteractivePrompt {
    let title = format!("Input required · {provider}");
    let body = screen_excerpt
        .clone()
        .unwrap_or_else(|| "Agent is waiting for approval or input in the live terminal.".into());
    let options = vec![
        InteractiveOption {
            id: "confirm".into(),
            index: 0,
            label: "Confirm (Enter)".into(),
            description: None,
            selected: true,
            checked: None,
            hotkey: Some("\r".into()),
            is_custom_input: false,
            is_destructive: false,
            disabled_reason: None,
        },
        InteractiveOption {
            id: "cancel".into(),
            index: 1,
            label: "Cancel (Esc)".into(),
            description: None,
            selected: false,
            checked: None,
            hotkey: Some("\x1b".into()),
            is_custom_input: false,
            is_destructive: true,
            disabled_reason: None,
        },
    ];
    let id = prompt_digest(
        "fallback",
        InteractivePromptKind::Confirmation,
        (!pane_id.is_empty()).then_some(pane_id),
        &title,
        &body,
        &options,
        0,
        false,
    );
    InteractivePrompt {
        id,
        kind: InteractivePromptKind::Confirmation,
        mode: InteractivePromptMode::TerminalTui,
        request_id: None,
        title,
        body,
        step_label: None,
        options,
        selected_index: 0,
        multi_select: false,
        allows_free_text: true,
        free_text_only: false,
        allows_cancel: true,
        can_navigate_tabs: false,
        always_patterns: Vec::new(),
        session_peers_count: 0,
        is_subagent: false,
        questions: Vec::new(),
        screen_excerpt,
        hint_line: None,
        risk: None,
    }
}

fn split_title_and_body(lines: &[String]) -> (String, String) {
    let useful: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|l| !l.is_empty() && !is_horizontal_rule(l))
        .collect();
    if useful.is_empty() {
        return (String::new(), String::new());
    }
    let slice = &useful[useful.len().saturating_sub(10)..];
    let title = slice[0].to_owned();
    let body = if slice.len() > 1 {
        slice[1..].join("\n")
    } else {
        String::new()
    };
    (title, body)
}

fn detect_step_label(lines: &[String]) -> Option<String> {
    for line in lines {
        let lower = line.to_ascii_lowercase();
        for marker in ["question ", "질문 ", "step "] {
            if let Some(pos) = lower.find(marker) {
                let after = &line[pos + marker.len()..];
                let token = after.split_whitespace().next().unwrap_or("");
                if let Some((a, b)) = token.split_once('/')
                    && a.bytes().all(|c| c.is_ascii_digit())
                    && !a.is_empty()
                    && b.trim_end_matches([':', ')', ']'])
                        .bytes()
                        .all(|c| c.is_ascii_digit())
                {
                    let b_clean = b.trim_end_matches([':', ')', ']']);
                    if !b_clean.is_empty() {
                        return Some(format!("{a}/{b_clean}"));
                    }
                }
            }
        }
    }
    None
}

fn match_pending_question<'a>(
    title: &str,
    body: &str,
    options: &[InteractiveOption],
    step_label: Option<&str>,
    pending: &'a [StructuredQuestion],
) -> Option<(usize, &'a StructuredQuestion)> {
    if pending.is_empty() {
        return None;
    }
    if let Some(step) = step_label
        && let Some((cur, _)) = step.split_once('/')
        && let Ok(n) = cur.parse::<usize>()
        && n >= 1
        && n <= pending.len()
    {
        return Some((n - 1, &pending[n - 1]));
    }
    let screen_text = format!("{title}\n{body}");
    for (idx, q) in pending.iter().enumerate() {
        let q_prefix: String = q.question.chars().take(24).collect();
        if !q_prefix.is_empty() && screen_text.contains(&q_prefix) {
            return Some((idx, q));
        }
        if let Some(first_opt) = options.first()
            && !first_opt.is_custom_input
            && q.options.iter().any(|o| {
                let p: String = o.label.chars().take(16).collect();
                !p.is_empty()
                    && (first_opt.label.contains(&p) || o.label.contains(&first_opt.label))
            })
        {
            return Some((idx, q));
        }
    }
    if pending.len() == 1 {
        return Some((0, &pending[0]));
    }
    None
}

fn is_custom_input_label(label: &str) -> bool {
    let lower = label.trim().to_ascii_lowercase();
    lower == "other"
        || lower.starts_with("other ")
        || lower.starts_with("other(")
        || lower.starts_with("other:")
        || lower.contains("type something")
        || lower.contains("type your own")
        || lower.contains("type here")
        || lower.contains("your answer")
        || (lower.contains("tell ") && lower.contains("what to do"))
        || lower.contains("직접 입력")
        || lower == "기타"
}

fn is_destructive_label(label: &str) -> bool {
    let lower = label.trim().to_ascii_lowercase();
    lower == "no"
        || lower.starts_with("no,")
        || lower.starts_with("no ")
        || lower.starts_with("no(")
        || lower.starts_with("deny")
        || lower.starts_with("reject")
        || lower.starts_with("cancel")
        || lower.starts_with("decline")
        || lower.starts_with("skip")
        || lower.starts_with("quit")
        || lower.starts_with("exit")
        || lower.starts_with("don't trust")
        || lower.starts_with("do not")
        || lower.starts_with("거절")
        || lower.starts_with("취소")
        || lower.starts_with("아니오")
}

fn classify_prompt_kind(
    title: &str,
    body: &str,
    options: &[InteractiveOption],
    multi_select: bool,
    has_pending_questions: bool,
    free_text_only: bool,
) -> InteractivePromptKind {
    if free_text_only {
        return InteractivePromptKind::Input;
    }
    if has_pending_questions {
        return InteractivePromptKind::Question;
    }
    let text = format!("{title}\n{body}").to_ascii_lowercase();
    let has_permission_option = options.iter().any(|o| {
        let l = o.label.trim().to_ascii_lowercase();
        l == "allow"
            || l == "allow once"
            || l == "always allow"
            || l == "approve"
            || l.starts_with("allow once")
            || l.starts_with("always allow")
            || l.starts_with("allow for this")
            || l.starts_with("approve ")
            || l.starts_with("yes, allow")
            || l.starts_with("yes, and don't ask")
    });
    let is_perm = text.contains("permission")
        || text.contains("allow command")
        || text.contains("allow execution")
        || text.contains("allow tool")
        || text.contains("approve tool")
        || text.contains("do you want to proceed")
        || text.contains("bash command")
        || text.contains("write to this file")
        || text.contains("apply this change")
        || text.contains("run this command")
        || text.contains("run command")
        || text.contains("권한")
        || text.contains("승인")
        || has_permission_option;
    if is_perm {
        return InteractivePromptKind::Permission;
    }
    let has_other_option = options.iter().any(|o| o.is_custom_input);
    if multi_select
        || text.contains("question")
        || text.contains("질문")
        || (has_other_option && !text.contains("trust") && !text.contains("update available"))
    {
        return InteractivePromptKind::Question;
    }
    if text.contains("trust") || text.contains("update available") || text.contains("log in") {
        return InteractivePromptKind::Confirmation;
    }
    InteractivePromptKind::Confirmation
}

fn default_title_for_kind(kind: InteractivePromptKind, provider: &str) -> String {
    match kind {
        InteractivePromptKind::Permission => format!("Permission · {provider}"),
        InteractivePromptKind::Question => format!("Question · {provider}"),
        InteractivePromptKind::Input => format!("Input · {provider}"),
        InteractivePromptKind::Confirmation => format!("Confirmation · {provider}"),
    }
}

pub fn clean_screen_excerpt(screen: &str, max_lines: usize) -> String {
    let non_empty: Vec<&str> = screen
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();
    let tail = &non_empty[non_empty.len().saturating_sub(max_lines)..];
    tail.join("\n")
}

#[allow(clippy::too_many_arguments)]
fn prompt_digest(
    prefix: &str,
    kind: InteractivePromptKind,
    req_id: Option<&str>,
    title: &str,
    body: &str,
    options: &[InteractiveOption],
    selected_index: usize,
    free_text_only: bool,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prefix.as_bytes());
    hasher.update(
        format!("|{kind:?}|{req_id:?}|{title}|{body}|{selected_index}|{free_text_only}").as_bytes(),
    );
    for opt in options {
        hasher.update(
            format!(
                "|{}:{}:{}:{:?}:{:?}",
                opt.index, opt.label, opt.selected, opt.checked, opt.hotkey
            )
            .as_bytes(),
        );
    }
    let hex: String = hasher.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{prefix}-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_opencode_api_permissions_and_questions() {
        let payload = json!({
            "channel": "opencode_port",
            "requests": [
                {
                    "id": "per_1",
                    "kind": "permission",
                    "session_id": "ses_1",
                    "root": true,
                    "permission": "bash",
                    "patterns": ["git push origin main"],
                    "always": ["git push *"],
                    "metadata": {"command": "git push origin main", "description": "Push branch"}
                },
                {
                    "id": "per_2",
                    "kind": "permission",
                    "session_id": "ses_1",
                    "root": false,
                    "permission": "edit",
                    "patterns": ["src/main.rs"]
                },
                {
                    "id": "que_1",
                    "kind": "question",
                    "session_id": "ses_1",
                    "root": true,
                    "questions": [
                        {
                            "header": "Color",
                            "question": "Which theme color?",
                            "custom": true,
                            "multiple": false,
                            "options": [
                                {"label": "Teal", "description": "Default accent"},
                                {"label": "Amber", "description": "Warm tone"}
                            ]
                        }
                    ]
                }
            ]
        });

        let prompts = parse_structured_api_requests(&payload);
        assert_eq!(prompts.len(), 3);
        assert_eq!(prompts[0].kind, InteractivePromptKind::Permission);
        assert_eq!(prompts[0].mode, InteractivePromptMode::StructuredApi);
        assert_eq!(prompts[0].request_id.as_deref(), Some("per_1"));
        assert_eq!(prompts[0].session_peers_count, 1);
        assert!(prompts[0].options[1].disabled_reason.is_some());
        assert!(prompts[0].body.contains("git push origin main"));
        assert!(prompts[1].is_subagent);

        assert_eq!(prompts[2].kind, InteractivePromptKind::Question);
        assert_eq!(prompts[2].request_id.as_deref(), Some("que_1"));
        assert_eq!(prompts[2].questions.len(), 1);
        assert_eq!(prompts[2].questions[0].options.len(), 2);
        assert!(prompts[2].allows_free_text);
    }

    #[test]
    fn parses_claude_bash_permission_and_selected_cursor() {
        let screen = "\
────────────────────────────────────────────────────────────
Bash command
  cargo test --workspace
  Run Rust test suite
Do you want to proceed?
  1. Yes
❯ 2. Yes, and don't ask again for: cargo test
  3. No
Esc to cancel · Tab to amend · ctrl+e to explain
";
        let prompt = parse_live_tui_screen("claude", "blocked", screen, &[]).unwrap();
        assert_eq!(prompt.kind, InteractivePromptKind::Permission);
        assert_eq!(prompt.mode, InteractivePromptMode::TerminalTui);
        assert_eq!(prompt.options.len(), 3);
        assert_eq!(prompt.selected_index, 1);
        assert!(prompt.options[1].selected);
        assert!(prompt.options[2].is_destructive);
        assert!(prompt.body.contains("cargo test --workspace"));
    }

    #[test]
    fn parses_agy_ask_question_and_enriches_from_transcript() {
        let screen = "\
Question 1/2
Which database engine should we use?
> 1. (Recommended) SQLite local…
  2. PostgreSQL remote
  3. Other
  ↑/↓ Navigate · enter Confirm · esc Cancel
";
        let pending = vec![
            StructuredQuestion {
                header: "Database".into(),
                question: "Which database engine should we use for local storage?".into(),
                options: vec![
                    StructuredQuestionOption {
                        label: "(Recommended) SQLite local embedded store".into(),
                        description: None,
                    },
                    StructuredQuestionOption {
                        label: "PostgreSQL remote".into(),
                        description: None,
                    },
                ],
                custom: true,
                multiple: false,
            },
            StructuredQuestion {
                header: "Auth".into(),
                question: "Enable OAuth?".into(),
                options: vec![],
                custom: true,
                multiple: false,
            },
        ];
        let prompt = parse_live_tui_screen("agy", "unknown", screen, &pending).unwrap();
        assert_eq!(prompt.kind, InteractivePromptKind::Question);
        assert_eq!(prompt.step_label.as_deref(), Some("1/2"));
        assert_eq!(prompt.options.len(), 3);
        assert_eq!(
            prompt.options[0].label,
            "(Recommended) SQLite local embedded store"
        );
        assert!(prompt.options[2].is_custom_input);
        assert!(prompt.allows_free_text);
        assert!(prompt.body.contains("local storage"));
    }

    #[test]
    fn parses_multi_select_checkbox_question() {
        let screen = "\
Select modules to enable:
❯ [ ] 1. Auth
  [✓] 2. Metrics
  [ ] 3. Cache
Space to toggle · Enter to submit · Esc to cancel
";
        let prompt = parse_live_tui_screen("claude", "blocked", screen, &[]).unwrap();
        assert_eq!(prompt.kind, InteractivePromptKind::Question);
        assert!(prompt.multi_select);
        assert_eq!(prompt.options.len(), 3);
        assert_eq!(prompt.options[0].checked, Some(false));
        assert_eq!(prompt.options[1].checked, Some(true));
        assert_eq!(prompt.options[2].checked, Some(false));
    }

    #[test]
    fn parses_agy_project_trust_unnumbered_choices() {
        let screen = "\
Accessing workspace:
/private/tmp/project
Do you trust the contents of this project?
> Yes, I trust this folder
  No, exit
  ↑/↓ Navigate · enter Confirm
";
        let prompt = parse_live_tui_screen("agy", "blocked", screen, &[]).unwrap();
        assert_eq!(prompt.kind, InteractivePromptKind::Confirmation);
        assert_eq!(prompt.options.len(), 2);
        assert_eq!(prompt.options[0].label, "Yes, I trust this folder");
        assert_eq!(prompt.options[1].label, "No, exit");
        assert!(prompt.options[1].is_destructive);
    }

    #[test]
    fn parses_kilo_reject_instruction_free_text_dialog() {
        let screen = "\
  ┃  △ Reject permission
  ┃
  ┃  Tell Kilo what to do differently
  ┃
  ┃                   enter confirm  esc cancel
  ┃                          • Kilo 7.8.17
";
        let prompt = parse_live_tui_screen("kilo", "blocked", screen, &[]).unwrap();
        assert_eq!(prompt.kind, InteractivePromptKind::Input);
        assert!(prompt.free_text_only);
        assert!(prompt.allows_free_text);
    }

    #[test]
    fn parses_cline_inline_bracket_hotkeys() {
        let screen = "\
Cline needs permission
Approve tool call?
[y] Approve  [n] Deny
";
        let prompt = parse_live_tui_screen("cline", "blocked", screen, &[]).unwrap();
        assert_eq!(prompt.kind, InteractivePromptKind::Permission);
        assert_eq!(prompt.options.len(), 2);
        assert_eq!(prompt.options[0].hotkey.as_deref(), Some("y"));
        assert_eq!(prompt.options[1].hotkey.as_deref(), Some("n"));
        assert!(prompt.options[1].is_destructive);
    }

    #[test]
    fn parses_omp_box_approval_without_cursor_char() {
        let screen = "\
╭─ Allow tool: bash ───────╮
│ Command: echo PINEAPPLE   │
│  Approve                  │
│   Approve bash for this session │
│   Deny                    │
├───────────────────────────┤
│ ↑/↓ choose · select · approve · deny · details │
╰───────────────────────────╯
";
        let prompt = parse_live_tui_screen("omp", "blocked", screen, &[]).unwrap();
        assert_eq!(prompt.kind, InteractivePromptKind::Permission);
        assert_eq!(prompt.options.len(), 3);
        assert_eq!(prompt.options[0].label, "Approve");
        assert_eq!(prompt.options[2].label, "Deny");
        assert!(prompt.options[2].is_destructive);
    }

    #[test]
    fn idle_composer_never_triggers_false_prompt() {
        let screen = "\
1. First item in assistant answer
2. Second item in assistant answer
────────────────────────────────────
>
────────────────────────────────────
? for shortcuts    Gemini 4 Neon · xhigh
";
        assert!(parse_live_tui_screen("agy", "idle", screen, &[]).is_none());
        assert!(parse_live_tui_screen("agy", "unknown", screen, &[]).is_none());
    }

    #[test]
    fn extracts_unanswered_agy_ask_question_from_transcript() {
        let items = vec![TimelineItem::ToolCall {
            id: "agy-tc-1".into(),
            seq: 1,
            ts_ms: 1000,
            name: "ask_question".into(),
            summary: "ask_question".into(),
            args_json: Some(json!({
                "questions": r#"[{"question":"Pick one","options":["A","B"],"is_multi_select":false}]"#
            })),
            output: None,
            status: masil_wire::rpc::ToolStatus::Completed,
        }];
        let qs = extract_pending_transcript_questions(&items);
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].question, "Pick one");
        assert_eq!(qs[0].options.len(), 2);

        // Once answered (`output: Some(...)`), no longer pending.
        let answered = vec![TimelineItem::ToolCall {
            id: "agy-tc-1".into(),
            seq: 1,
            ts_ms: 1000,
            name: "ask_question".into(),
            summary: "ask_question".into(),
            args_json: Some(json!({
                "questions": r#"[{"question":"Pick one","options":["A","B"],"is_multi_select":false}]"#
            })),
            output: Some("A1: 1".into()),
            status: masil_wire::rpc::ToolStatus::Completed,
        }];
        assert!(extract_pending_transcript_questions(&answered).is_empty());
    }
}
