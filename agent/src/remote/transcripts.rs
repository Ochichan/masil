//! Live conversation timeline projection for Claude Code, Codex, Antigravity
//! CLI (`agy`), OpenCode, and universal pane output fallback for all 24
//! providers.

use crate::native_ui::native_executable;
use crate::remote::config::DeviceStore;
use crate::remote::discovery::{
    ImportableSession, discover_agy_sessions, discover_claude_sessions, discover_codex_sessions,
    discover_opencode_sessions,
};
use masil_wire::rpc::{ApprovalMode, ApprovalOption, BindingConfidence, TimelineItem, ToolStatus};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

const DEFAULT_TIMELINE_LIMIT: usize = 200;
const TAIL_READ_MAX_BYTES: u64 = 2 * 1024 * 1024;
const MODEL_SNIFF_TAIL_BYTES: u64 = 256 * 1024;
const BINDING_CACHE_TTL: Duration = Duration::from_secs(4);

#[derive(Clone)]
struct CachedBindingEntry {
    cwd: String,
    reported_sid: Option<String>,
    resolved: ResolvedBinding,
    fetched_at: Instant,
}

#[derive(Clone)]
struct CachedTimelineEntry {
    source_path: PathBuf,
    file_len: u64,
    mtime_ms: i64,
    session_id: Option<String>,
    confidence: BindingConfidence,
    page: TimelinePage,
}

static BINDING_CACHE: OnceLock<Mutex<HashMap<String, CachedBindingEntry>>> = OnceLock::new();
static TIMELINE_CACHE: OnceLock<Mutex<HashMap<String, CachedTimelineEntry>>> = OnceLock::new();

pub fn invalidate_agent_cache(agent_key: &str) {
    if let Ok(mut guard) = BINDING_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        guard.retain(|k, _| !k.starts_with(agent_key));
    }
    if let Ok(mut guard) = TIMELINE_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        guard.retain(|k, _| !k.starts_with(agent_key));
    }
}

fn file_stat_sig(path: &Path) -> Option<(u64, i64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some((meta.len(), mtime_ms))
}

/// Reads complete lines from the tail of a potentially huge JSONL file without
/// scanning from byte 0. Safe across multi-byte UTF-8 boundaries because it
/// discards up to the first `\n` when seeking into the middle of the file.
fn read_tail_lines(path: &Path, max_bytes: u64) -> Vec<String> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let seeked = len > max_bytes;
    if seeked && file.seek(SeekFrom::Start(len - max_bytes)).is_err() {
        return Vec::new();
    }
    let mut reader = BufReader::new(file);
    let mut buf = Vec::with_capacity(4096);
    if seeked {
        let _ = reader.read_until(b'\n', &mut buf);
        buf.clear();
    }
    let mut lines = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {
                while matches!(buf.last(), Some(b'\n' | b'\r')) {
                    buf.pop();
                }
                if !buf.is_empty() {
                    lines.push(String::from_utf8_lossy(&buf).into_owned());
                }
            }
            Err(_) => break,
        }
    }
    lines
}

fn find_latest_claude_session_for_cwd(home: &Path, cwd: &str) -> Option<(String, PathBuf)> {
    let norm = cwd.trim_end_matches('/');
    if norm.is_empty() {
        return None;
    }
    let projects_root = home.join(".claude").join("projects");
    let slug_slash = norm.replace('/', "-");
    let slug_dot = norm.replace(['/', '.'], "-");
    let candidates: Vec<String> = if slug_slash == slug_dot {
        vec![slug_slash]
    } else {
        vec![slug_slash, slug_dot]
    };

    let mut best: Option<(i64, String, PathBuf)> = None;
    for slug in candidates {
        let dir = projects_root.join(&slug);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let mtime = file_stat_sig(&p).map(|(_, m)| m).unwrap_or(0);
            if best.as_ref().is_none_or(|(b_mtime, _, _)| mtime > *b_mtime) {
                best = Some((mtime, stem.to_owned(), p));
            }
        }
        if best.is_some() {
            break;
        }
    }
    best.map(|(_, sid, path)| (sid, path))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelinePage {
    pub binding_confidence: BindingConfidence,
    pub session_id: Option<String>,
    pub source_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_model: Option<String>,
    pub items: Vec<TimelineItem>,
    pub has_more: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedBinding {
    pub confidence: BindingConfidence,
    pub session_id: Option<String>,
    pub source_path: Option<PathBuf>,
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn candidate_sessions_for_agent(
    provider: &str,
    cwd: &str,
    limit: usize,
) -> Vec<ImportableSession> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let mut list = match provider {
        "claude" => discover_claude_sessions(&home, limit * 2),
        "codex" => discover_codex_sessions(&home, limit * 2),
        "agy" | "antigravity" => discover_agy_sessions(&home, limit * 2),
        "opencode" => discover_opencode_sessions(&home, limit * 2),
        _ => Vec::new(),
    };
    if !cwd.is_empty() {
        let norm_cwd = cwd.trim_end_matches('/');
        list.sort_by(|a, b| {
            let a_match = a.cwd.trim_end_matches('/') == norm_cwd;
            let b_match = b.cwd.trim_end_matches('/') == norm_cwd;
            b_match
                .cmp(&a_match)
                .then_with(|| b.last_active_ms.cmp(&a.last_active_ms))
        });
    }
    list.truncate(limit);
    list
}

pub fn resolve_binding(
    agent_key: &str,
    provider: &str,
    cwd: &str,
    reported_session_id: Option<&str>,
) -> ResolvedBinding {
    let Some(home) = home_dir() else {
        return ResolvedBinding {
            confidence: BindingConfidence::None,
            session_id: None,
            source_path: None,
        };
    };

    let cache_key = format!("{agent_key}::{provider}");
    let reported_owned = reported_session_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if let Ok(guard) = BINDING_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        && let Some(entry) = guard.get(&cache_key)
        && entry.fetched_at.elapsed() < BINDING_CACHE_TTL
        && entry.cwd == cwd
        && entry.reported_sid == reported_owned
    {
        return entry.resolved.clone();
    }

    let resolved =
        resolve_binding_uncached(&home, agent_key, provider, cwd, reported_owned.as_deref());
    if let Ok(mut guard) = BINDING_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        guard.insert(
            cache_key,
            CachedBindingEntry {
                cwd: cwd.to_owned(),
                reported_sid: reported_owned,
                resolved: resolved.clone(),
                fetched_at: Instant::now(),
            },
        );
    }
    resolved
}

fn resolve_binding_uncached(
    home: &Path,
    agent_key: &str,
    provider: &str,
    cwd: &str,
    reported_session_id: Option<&str>,
) -> ResolvedBinding {
    if let Some(manual_id) = DeviceStore::get_manual_binding(agent_key)
        && let Some(path) = locate_session_source(home, provider, &manual_id)
    {
        return ResolvedBinding {
            confidence: BindingConfidence::ManuallySelected,
            session_id: Some(manual_id),
            source_path: Some(path),
        };
    }

    if let Some(sid) = reported_session_id.filter(|s| !s.trim().is_empty())
        && let Some(path) = locate_session_source(home, provider, sid)
    {
        return ResolvedBinding {
            confidence: BindingConfidence::ReportedByHook,
            session_id: Some(sid.to_owned()),
            source_path: Some(path),
        };
    }

    // Fast direct-directory lookup for Claude before scanning all project folders.
    if provider == "claude"
        && !cwd.is_empty()
        && let Some((sid, path)) = find_latest_claude_session_for_cwd(home, cwd)
    {
        return ResolvedBinding {
            confidence: BindingConfidence::UnverifiedHeuristic,
            session_id: Some(sid),
            source_path: Some(path),
        };
    }

    // Heuristic fallback: match most recently active session in the exact same cwd.
    // Always tagged UnverifiedHeuristic so the mobile UI shows an honest badge.
    if !cwd.is_empty() {
        let norm_cwd = cwd.trim_end_matches('/');
        let candidates = candidate_sessions_for_agent(provider, cwd, 10);
        if let Some(best) = candidates
            .into_iter()
            .find(|c| c.cwd.trim_end_matches('/') == norm_cwd)
        {
            let source_path = best
                .source_path
                .map(PathBuf::from)
                .or_else(|| locate_session_source(home, provider, &best.session_id));
            if source_path.is_some() {
                return ResolvedBinding {
                    confidence: BindingConfidence::UnverifiedHeuristic,
                    session_id: Some(best.session_id),
                    source_path,
                };
            }
        }
    }

    ResolvedBinding {
        confidence: BindingConfidence::None,
        session_id: None,
        source_path: None,
    }
}

fn locate_session_source(home: &Path, provider: &str, session_id: &str) -> Option<PathBuf> {
    match provider {
        "claude" => {
            let root = home.join(".claude").join("projects");
            let target = format!("{session_id}.jsonl");
            for entry in fs::read_dir(root).ok()?.flatten() {
                let candidate = entry.path().join(&target);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
            None
        }
        "codex" => {
            let sessions = discover_codex_sessions(home, 80);
            sessions
                .into_iter()
                .find(|s| s.session_id == session_id)
                .and_then(|s| s.source_path.map(PathBuf::from))
        }
        "agy" | "antigravity" => {
            let p = home
                .join(".gemini")
                .join("antigravity-cli")
                .join("brain")
                .join(session_id)
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            p.is_file().then_some(p)
        }
        "opencode" => {
            let p = home
                .join(".local")
                .join("share")
                .join("opencode")
                .join("opencode.db");
            p.is_file().then_some(p)
        }
        _ => None,
    }
}

pub struct TimelineQuery<'a> {
    pub server_socket: &'a str,
    pub pane_id: &'a str,
    pub agent_key: &'a str,
    pub provider: &'a str,
    pub cwd: &'a str,
    pub reported_session_id: Option<&'a str>,
    pub state: &'a str,
    pub limit: Option<usize>,
}

pub fn fetch_timeline(query: TimelineQuery<'_>) -> TimelinePage {
    let TimelineQuery {
        server_socket,
        pane_id,
        agent_key,
        provider,
        cwd,
        reported_session_id,
        state,
        limit,
    } = query;
    let cap = limit.unwrap_or(DEFAULT_TIMELINE_LIMIT).clamp(20, 500);
    let binding = resolve_binding(agent_key, provider, cwd, reported_session_id);

    let cache_key = format!("{agent_key}::{provider}::{state}::{cap}");
    let source_stat = binding.source_path.as_deref().and_then(file_stat_sig);
    if let (Some(src_path), Some((len, mtime_ms))) = (binding.source_path.as_deref(), source_stat)
        && let Ok(guard) = TIMELINE_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
        && let Some(entry) = guard.get(&cache_key)
        && entry.source_path == src_path
        && entry.file_len == len
        && entry.mtime_ms == mtime_ms
        && entry.session_id == binding.session_id
        && entry.confidence == binding.confidence
    {
        return attach_plugin_cards(entry.page.clone(), agent_key);
    }

    let mut items = match (binding.confidence, binding.source_path.as_deref()) {
        (BindingConfidence::None, _) | (_, None) => Vec::new(),
        (_, Some(path)) => match provider {
            "claude" => parse_claude_jsonl(path, cap),
            "codex" => parse_codex_jsonl(path, cap),
            "agy" | "antigravity" => parse_agy_jsonl(path, cap),
            "opencode" => binding
                .session_id
                .as_deref()
                .map(|sid| parse_opencode_sqlite(path, sid, cap))
                .unwrap_or_default(),
            _ => Vec::new(),
        },
    };

    let (confidence, source_label) = if items.is_empty() {
        items = build_pane_fallback_items(server_socket, pane_id);
        (BindingConfidence::None, "pane_capture".to_owned())
    } else {
        (binding.confidence, format!("{provider}_transcript"))
    };

    // Append an interactive approval banner whenever the live pane is blocked so
    // the mobile user sees an actionable card right inside the conversation view.
    if state == "blocked" {
        let next_seq = items.len() as u64 + 1;
        let is_structured_api = matches!(provider, "opencode" | "kilo");
        items.push(TimelineItem::ApprovalBanner {
            id: format!("blocked-{pane_id}-{next_seq}"),
            seq: next_seq,
            ts_ms: crate::remote::config::now_ms(),
            request_id: None,
            prompt: "Agent is waiting for approval or input in the live terminal.".into(),
            options: vec![
                ApprovalOption {
                    id: "allow".into(),
                    label: "Allow / Enter".into(),
                    key_sequence: Some("\r".into()),
                },
                ApprovalOption {
                    id: "yes".into(),
                    label: "Yes (y + Enter)".into(),
                    key_sequence: Some("y\r".into()),
                },
                ApprovalOption {
                    id: "deny".into(),
                    label: "Cancel (Esc)".into(),
                    key_sequence: Some("\x1b".into()),
                },
            ],
            mode: if is_structured_api {
                ApprovalMode::StructuredApi
            } else {
                ApprovalMode::TerminalConfirm
            },
        });
    }

    let detected_model = detect_latest_model(&items, binding.source_path.as_deref(), provider);
    let has_more = items.len() >= cap;
    let page = TimelinePage {
        binding_confidence: confidence,
        session_id: binding.session_id.clone(),
        source_label,
        detected_model,
        items,
        has_more,
    };
    if let (Some(src_path), Some((len, mtime_ms))) = (binding.source_path, source_stat)
        && let Ok(mut guard) = TIMELINE_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
    {
        guard.insert(
            cache_key,
            CachedTimelineEntry {
                source_path: src_path,
                file_len: len,
                mtime_ms,
                session_id: binding.session_id,
                confidence: binding.confidence,
                page: page.clone(),
            },
        );
    }
    attach_plugin_cards(page, agent_key)
}

fn attach_plugin_cards(mut page: TimelinePage, agent_key: &str) -> TimelinePage {
    let cards = crate::plugin::runner::load_timeline_cards(agent_key);
    if cards.is_empty() {
        return page;
    }
    // Keep any trailing ApprovalBanner at the very bottom so input prompts stay prominent.
    let trailing_banner = if matches!(page.items.last(), Some(TimelineItem::ApprovalBanner { .. }))
    {
        page.items.pop()
    } else {
        None
    };
    page.items.extend(cards);
    if let Some(banner) = trailing_banner {
        page.items.push(banner);
    }
    page
}

fn detect_latest_model(
    items: &[TimelineItem],
    source_path: Option<&Path>,
    provider: &str,
) -> Option<String> {
    for item in items.iter().rev() {
        if let TimelineItem::AssistantMessage { model: Some(m), .. } = item
            && !m.trim().is_empty()
        {
            return Some(m.clone());
        }
    }
    // For active agy turns where PLANNER_RESPONSE only has tool_calls so far,
    // sniff the latest `model` field from the transcript tail.
    if matches!(provider, "agy" | "antigravity")
        && let Some(path) = source_path
    {
        let mut last_model = None;
        for line in read_tail_lines(path, MODEL_SNIFF_TAIL_BYTES) {
            if line.contains("\"model\"")
                && let Ok(v) = serde_json::from_str::<Value>(&line)
                && let Some(m) = v.get("model").and_then(Value::as_str)
                && !m.trim().is_empty()
            {
                last_model = Some(m.to_owned());
            }
        }
        return last_model;
    }
    None
}

fn clean_claude_user_text(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.contains("<command-name>") {
        let name = extract_xml_tag(trimmed, "command-name").unwrap_or_default();
        let args = extract_xml_tag(trimmed, "command-args").unwrap_or_default();
        let combined = format!("{name} {args}").trim().to_owned();
        if !combined.is_empty() {
            return combined;
        }
    }
    strip_xml_block(trimmed, "system-reminder")
}

fn clean_agy_user_text(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(inner) = extract_xml_tag(trimmed, "USER_REQUEST") {
        return inner.trim().to_owned();
    }
    trimmed.to_owned()
}

fn extract_xml_tag(input: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = input.find(&open)? + open.len();
    let rest = &input[start..];
    let end = rest.find(&close)?;
    Some(rest[..end].trim().to_owned())
}

fn strip_xml_block(input: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut cur = input.to_owned();
    while let Some(s) = cur.find(&open) {
        if let Some(e_rel) = cur[s..].find(&close) {
            cur.replace_range(s..s + e_rel + close.len(), "");
        } else {
            break;
        }
    }
    cur.trim().to_owned()
}

pub fn parse_claude_jsonl(path: &Path, limit: usize) -> Vec<TimelineItem> {
    let lines = read_tail_lines(path, TAIL_READ_MAX_BYTES);
    let mut out = Vec::new();
    let mut seq = 0u64;
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let ts_ms = parse_ts_ms(v.get("timestamp"));
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                if v.get("isMeta").and_then(Value::as_bool).unwrap_or(false) {
                    continue;
                }
                let Some(msg) = v.get("message") else {
                    continue;
                };
                match msg.get("content") {
                    Some(Value::String(s)) => {
                        let cleaned = clean_claude_user_text(s);
                        if !cleaned.is_empty() {
                            seq += 1;
                            out.push(TimelineItem::UserMessage {
                                id: v
                                    .get("uuid")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| format!("u-{seq}")),
                                seq,
                                ts_ms,
                                text: cleaned,
                            });
                        }
                    }
                    Some(Value::Array(arr)) => {
                        let mut text_parts = Vec::new();
                        for part in arr {
                            match part.get("type").and_then(Value::as_str) {
                                Some("text") => {
                                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                                        let cleaned = clean_claude_user_text(t);
                                        if !cleaned.is_empty() {
                                            text_parts.push(cleaned);
                                        }
                                    }
                                }
                                Some("tool_result") => {
                                    let output = extract_value_text(part.get("content"));
                                    if !output.is_empty() {
                                        let is_err = part
                                            .get("is_error")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false);
                                        let status = if is_err {
                                            ToolStatus::Failed
                                        } else {
                                            ToolStatus::Completed
                                        };
                                        let tool_use_id =
                                            part.get("tool_use_id").and_then(Value::as_str);
                                        let mut merged = false;
                                        if let Some(tuid) = tool_use_id {
                                            for prev in out.iter_mut().rev() {
                                                if let TimelineItem::ToolCall {
                                                    id,
                                                    output: prev_out,
                                                    status: prev_status,
                                                    ..
                                                } = prev
                                                    && id == tuid
                                                {
                                                    *prev_out = Some(truncate_chars(&output, 4000));
                                                    *prev_status = status;
                                                    merged = true;
                                                    break;
                                                }
                                            }
                                        }
                                        if !merged {
                                            seq += 1;
                                            out.push(TimelineItem::ToolCall {
                                                id: tool_use_id
                                                    .map(|id| format!("res-{id}"))
                                                    .unwrap_or_else(|| format!("tr-{seq}")),
                                                seq,
                                                ts_ms,
                                                name: "tool_result".into(),
                                                summary: first_line_preview(&output, 80),
                                                args_json: None,
                                                output: Some(truncate_chars(&output, 4000)),
                                                status,
                                            });
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        let joined = text_parts.join("\n").trim().to_owned();
                        if !joined.is_empty() {
                            seq += 1;
                            out.push(TimelineItem::UserMessage {
                                id: format!("u-{seq}"),
                                seq,
                                ts_ms,
                                text: joined,
                            });
                        }
                    }
                    _ => {}
                }
            }
            Some("assistant") => {
                let Some(msg) = v.get("message") else {
                    continue;
                };
                let model = msg.get("model").and_then(Value::as_str).map(str::to_owned);
                let Some(arr) = msg.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for part in arr {
                    match part.get("type").and_then(Value::as_str) {
                        Some("thinking") => {
                            if let Some(t) = part.get("thinking").and_then(Value::as_str)
                                && !t.trim().is_empty()
                            {
                                seq += 1;
                                out.push(TimelineItem::Thinking {
                                    id: format!("th-{seq}"),
                                    seq,
                                    ts_ms,
                                    text: truncate_chars(t.trim(), 4000),
                                    collapsed_default: true,
                                });
                            }
                        }
                        Some("text") => {
                            if let Some(t) = part.get("text").and_then(Value::as_str)
                                && !t.trim().is_empty()
                            {
                                seq += 1;
                                out.push(TimelineItem::AssistantMessage {
                                    id: format!("a-{seq}"),
                                    seq,
                                    ts_ms,
                                    markdown: t.trim().to_owned(),
                                    model: model.clone(),
                                });
                            }
                        }
                        Some("tool_use") => {
                            let name = part
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_owned();
                            let input = part.get("input").cloned();
                            let summary = summarize_tool_args(&name, input.as_ref());
                            seq += 1;
                            out.push(TimelineItem::ToolCall {
                                id: part
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| format!("tc-{seq}")),
                                seq,
                                ts_ms,
                                name,
                                summary,
                                args_json: input,
                                output: None,
                                status: ToolStatus::Completed,
                            });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    keep_tail(out, limit)
}

pub fn parse_codex_jsonl(path: &Path, limit: usize) -> Vec<TimelineItem> {
    let lines = read_tail_lines(path, TAIL_READ_MAX_BYTES);
    let mut out = Vec::new();
    let mut seq = 0u64;
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let ts_ms = parse_ts_ms(v.get("timestamp"));
        if v.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(p) = v.get("payload") else {
            continue;
        };
        match p.get("type").and_then(Value::as_str) {
            Some("message") => {
                let role = p.get("role").and_then(Value::as_str).unwrap_or("");
                let text = p
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|c| c.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                seq += 1;
                if role == "user" {
                    out.push(TimelineItem::UserMessage {
                        id: format!("cu-{seq}"),
                        seq,
                        ts_ms,
                        text: trimmed.to_owned(),
                    });
                } else if role == "assistant" {
                    out.push(TimelineItem::AssistantMessage {
                        id: format!("ca-{seq}"),
                        seq,
                        ts_ms,
                        markdown: trimmed.to_owned(),
                        model: None,
                    });
                }
            }
            Some("reasoning") => {
                let summary = p
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|s| {
                                s.get("text").and_then(Value::as_str).or_else(|| s.as_str())
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if !summary.trim().is_empty() {
                    seq += 1;
                    out.push(TimelineItem::Thinking {
                        id: format!("cth-{seq}"),
                        seq,
                        ts_ms,
                        text: truncate_chars(summary.trim(), 4000),
                        collapsed_default: true,
                    });
                }
            }
            Some("custom_tool_call" | "function_call") => {
                let name = p
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_owned();
                let args = p.get("input").or_else(|| p.get("arguments")).cloned();
                let summary = summarize_tool_args(&name, args.as_ref());
                seq += 1;
                out.push(TimelineItem::ToolCall {
                    id: p
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("ctc-{seq}")),
                    seq,
                    ts_ms,
                    name,
                    summary,
                    args_json: args,
                    output: None,
                    status: ToolStatus::Completed,
                });
            }
            Some("custom_tool_call_output" | "function_call_output") => {
                let output = extract_value_text(p.get("output"));
                if !output.is_empty() {
                    seq += 1;
                    out.push(TimelineItem::ToolCall {
                        id: p
                            .get("call_id")
                            .and_then(Value::as_str)
                            .map(|id| format!("out-{id}"))
                            .unwrap_or_else(|| format!("cto-{seq}")),
                        seq,
                        ts_ms,
                        name: "tool_output".into(),
                        summary: first_line_preview(&output, 80),
                        args_json: None,
                        output: Some(truncate_chars(&output, 4000)),
                        status: ToolStatus::Completed,
                    });
                }
            }
            _ => {}
        }
    }
    keep_tail(out, limit)
}

pub fn parse_agy_jsonl(path: &Path, limit: usize) -> Vec<TimelineItem> {
    let lines = read_tail_lines(path, TAIL_READ_MAX_BYTES);
    let mut out = Vec::new();
    let mut seq = 0u64;
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let ts_ms = parse_ts_ms(v.get("created_at"));
        match v.get("type").and_then(Value::as_str) {
            Some("USER_INPUT") => {
                let text = clean_agy_user_text(&extract_value_text(v.get("content")));
                if !text.is_empty() {
                    seq += 1;
                    out.push(TimelineItem::UserMessage {
                        id: format!("agy-u-{seq}"),
                        seq,
                        ts_ms,
                        text,
                    });
                }
            }
            Some("GENERIC") => {
                let output = extract_value_text(v.get("content"));
                if !output.is_empty() {
                    for prev in out.iter_mut().rev() {
                        if let TimelineItem::ToolCall {
                            output: prev_out, ..
                        } = prev
                        {
                            if prev_out.is_none() {
                                *prev_out = Some(truncate_chars(&output, 4000));
                            }
                            break;
                        }
                    }
                }
            }
            Some("PLANNER_RESPONSE") => {
                let model = v.get("model").and_then(Value::as_str).map(str::to_owned);
                if let Some(thinking) = v.get("thinking").and_then(Value::as_str)
                    && !thinking.trim().is_empty()
                {
                    seq += 1;
                    out.push(TimelineItem::Thinking {
                        id: format!("agy-th-{seq}"),
                        seq,
                        ts_ms,
                        text: truncate_chars(thinking.trim(), 4000),
                        collapsed_default: true,
                    });
                }
                if let Some(calls) = v.get("tool_calls").and_then(Value::as_array) {
                    for tc in calls {
                        let name = tc
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool")
                            .to_owned();
                        let args = tc.get("args").cloned();
                        let summary = summarize_tool_args(&name, args.as_ref());
                        seq += 1;
                        out.push(TimelineItem::ToolCall {
                            id: format!("agy-tc-{seq}"),
                            seq,
                            ts_ms,
                            name,
                            summary,
                            args_json: args,
                            output: None,
                            status: ToolStatus::Completed,
                        });
                    }
                }
                let resp = v
                    .get("response")
                    .or_else(|| v.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if !resp.is_empty() {
                    seq += 1;
                    out.push(TimelineItem::AssistantMessage {
                        id: format!("agy-a-{seq}"),
                        seq,
                        ts_ms,
                        markdown: resp.to_owned(),
                        model,
                    });
                }
            }
            Some("CHECKPOINT") => {
                let text = extract_value_text(v.get("content"));
                if !text.is_empty() {
                    seq += 1;
                    out.push(TimelineItem::SystemNotice {
                        id: format!("agy-cp-{seq}"),
                        seq,
                        ts_ms,
                        level: "info".into(),
                        text: truncate_chars(&text, 1000),
                    });
                }
            }
            _ => {}
        }
    }
    keep_tail(out, limit)
}

pub fn parse_opencode_sqlite(db_path: &Path, session_id: &str, limit: usize) -> Vec<TimelineItem> {
    let Ok(conn) = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Vec::new();
    };
    let _ = conn.execute_batch("PRAGMA query_only = ON; PRAGMA busy_timeout = 500;");
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, time_created, data FROM part \
         WHERE session_id = ?1 \
         ORDER BY time_created DESC LIMIT ?2",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map((session_id, limit as i64), |row| {
        let id: String = row.get(0)?;
        let ts_ms: i64 = row.get(1).unwrap_or(0);
        let data: String = row.get(2).unwrap_or_default();
        Ok((id, ts_ms, data))
    }) else {
        return Vec::new();
    };
    let mut collected: Vec<_> = rows.flatten().collect();
    collected.reverse();

    let mut out = Vec::new();
    let mut seq = 0u64;
    for (id, ts_ms, raw) in collected {
        let Ok(v) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = v.get("text").and_then(Value::as_str).unwrap_or("").trim();
                if text.is_empty() {
                    continue;
                }
                seq += 1;
                if v.get("synthetic").and_then(Value::as_bool).unwrap_or(false) {
                    continue;
                }
                let is_user = v.get("role").and_then(Value::as_str) == Some("user");
                if is_user {
                    out.push(TimelineItem::UserMessage {
                        id,
                        seq,
                        ts_ms,
                        text: text.to_owned(),
                    });
                } else {
                    out.push(TimelineItem::AssistantMessage {
                        id,
                        seq,
                        ts_ms,
                        markdown: text.to_owned(),
                        model: None,
                    });
                }
            }
            Some("reasoning") => {
                let text = v.get("text").and_then(Value::as_str).unwrap_or("").trim();
                if !text.is_empty() {
                    seq += 1;
                    out.push(TimelineItem::Thinking {
                        id,
                        seq,
                        ts_ms,
                        text: truncate_chars(text, 4000),
                        collapsed_default: true,
                    });
                }
            }
            Some("tool") => {
                let name = v
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_owned();
                let state = v.get("state");
                let input = state.and_then(|s| s.get("input")).cloned();
                let output = state
                    .and_then(|s| s.get("output"))
                    .and_then(Value::as_str)
                    .map(|s| truncate_chars(s, 4000));
                let status = match state.and_then(|s| s.get("status")).and_then(Value::as_str) {
                    Some("running" | "pending") => ToolStatus::Running,
                    Some("error") => ToolStatus::Failed,
                    _ => ToolStatus::Completed,
                };
                let summary = summarize_tool_args(&name, input.as_ref());
                seq += 1;
                out.push(TimelineItem::ToolCall {
                    id,
                    seq,
                    ts_ms,
                    name,
                    summary,
                    args_json: input,
                    output,
                    status,
                });
            }
            _ => {}
        }
    }
    keep_tail(out, limit)
}

fn build_pane_fallback_items(server_socket: &str, pane_id: &str) -> Vec<TimelineItem> {
    let Ok(masil) = native_executable() else {
        return Vec::new();
    };
    let Ok(out) = Command::new(masil)
        .arg("-S")
        .arg(server_socket)
        .args(["capture-pane", "-p", "-J", "-S", "-200", "-t", pane_id])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if text.is_empty() {
        return Vec::new();
    }
    vec![TimelineItem::SystemNotice {
        id: format!("pane-capture-{pane_id}"),
        seq: 1,
        ts_ms: crate::remote::config::now_ms(),
        level: "pane_capture".into(),
        text: format!("```text\n{}\n```", truncate_chars(&text, 12_000)),
    }]
}

fn keep_tail(mut items: Vec<TimelineItem>, limit: usize) -> Vec<TimelineItem> {
    if items.len() > limit {
        let drain_end = items.len() - limit;
        items.drain(..drain_end);
    }
    items
}

fn parse_ts_ms(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(_)) => crate::remote::config::now_ms(),
        _ => crate::remote::config::now_ms(),
    }
}

fn extract_value_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .or_else(|| item.get("text").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn first_line_preview(text: &str, max_chars: usize) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    truncate_chars(first.trim(), max_chars)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        let mut s: String = text.chars().take(max_chars).collect();
        s.push('…');
        s
    }
}

fn summarize_tool_args(name: &str, args: Option<&Value>) -> String {
    let Some(v) = args else {
        return name.to_owned();
    };
    if let Some(obj) = v.as_object() {
        for key in [
            "command",
            "CommandLine",
            "path",
            "Path",
            "file_path",
            "pattern",
            "query",
            "toolSummary",
        ] {
            if let Some(s) = obj.get(key).and_then(Value::as_str)
                && !s.trim().is_empty()
            {
                return format!("{name}: {}", first_line_preview(s, 72));
            }
        }
    }
    if let Some(s) = v.as_str()
        && !s.trim().is_empty()
    {
        return format!("{name}: {}", first_line_preview(s, 72));
    }
    let _ = json!({});
    name.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_claude_codex_and_agy_transcripts() {
        let dir = std::env::temp_dir().join(format!("masil-tr-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);

        // Claude
        let claude_path = dir.join("claude.jsonl");
        let mut f = File::create(&claude_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","uuid":"u1","timestamp":1000,"message":{{"role":"user","content":"fix bug"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","timestamp":1001,"message":{{"model":"claude-opus","content":[{{"type":"thinking","thinking":"checking"}},{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"cargo test"}}}},{{"type":"text","text":"Done."}}]}}}}"#
        )
        .unwrap();
        let claude_items = parse_claude_jsonl(&claude_path, 50);
        assert_eq!(claude_items.len(), 4);

        // Codex
        let codex_path = dir.join("codex.jsonl");
        let mut f = File::create(&codex_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"response_item","timestamp":2000,"payload":{{"type":"message","role":"user","content":[{{"text":"run checks"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"response_item","timestamp":2001,"payload":{{"type":"custom_tool_call","call_id":"c1","name":"shell","input":{{"command":"ls"}}}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"response_item","timestamp":2002,"payload":{{"type":"message","role":"assistant","content":[{{"text":"All green"}}]}}}}"#
        )
        .unwrap();
        let codex_items = parse_codex_jsonl(&codex_path, 50);
        assert_eq!(codex_items.len(), 3);

        // Antigravity (agy)
        let agy_path = dir.join("agy.jsonl");
        let mut f = File::create(&agy_path).unwrap();
        writeln!(
            f,
            r#"{{"type":"USER_INPUT","created_at":3000,"step_index":1,"content":"build mobile"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"PLANNER_RESPONSE","created_at":3001,"step_index":2,"model":"gemini-4","thinking":"planning","tool_calls":[{{"name":"run_command","args":{{"CommandLine":"cargo test"}}}}],"response":"Built cleanly"}}"#
        )
        .unwrap();
        let agy_items = parse_agy_jsonl(&agy_path, 50);
        assert_eq!(agy_items.len(), 4);

        let _ = fs::remove_dir_all(&dir);
    }
}
