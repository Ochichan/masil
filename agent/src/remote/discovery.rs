//! Multi-server masil socket discovery and external CLI session discovery
//! across Claude Code, Codex, Antigravity CLI (`agy`), and OpenCode.

use crate::worktree::servers;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredServer {
    pub name: String,
    pub socket: String,
    pub alive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportableSession {
    pub provider: String,
    pub session_id: String,
    pub title: String,
    pub preview: String,
    pub cwd: String,
    pub last_active_ms: i64,
    pub source_path: Option<String>,
}

pub fn discover_masil_servers() -> Vec<DiscoveredServer> {
    let mut out = Vec::new();
    let Some(dir) = servers::socket_dir() else {
        return out;
    };
    for sock in servers::sockets_in(&dir) {
        let alive = servers::listening(&sock).unwrap_or(false);
        if !alive {
            continue;
        }
        let name = sock
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("default")
            .to_owned();
        out.push(DiscoveredServer {
            name,
            socket: sock.to_string_lossy().into_owned(),
            alive,
        });
    }
    out
}

pub fn default_masil_socket() -> Result<PathBuf, String> {
    let dir = servers::socket_dir().ok_or("cannot resolve masil socket directory")?;
    Ok(dir.join("default"))
}

pub fn run_self_agent_json(socket: Option<&str>, args: &[String]) -> Result<Value, String> {
    run_self_agent_json_env(socket, args, &[])
}

pub fn run_self_agent_json_env(
    socket: Option<&str>,
    args: &[String],
    envs: &[(&str, &str)],
) -> Result<Value, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find masil-agent: {e}"))?;
    let mut cmd = Command::new(exe);
    cmd.arg("agent");
    if let Some(sock) = socket {
        cmd.arg("--socket").arg(sock);
    }
    cmd.args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let output = cmd
        .output()
        .map_err(|e| format!("failed spawning masil-agent: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if output.status.success() {
        if stdout.is_empty() {
            return Ok(serde_json::json!({"ok": true}));
        }
        serde_json::from_str(&stdout).or_else(|_| Ok(serde_json::json!({"raw": stdout})))
    } else if !stdout.is_empty()
        && let Ok(val) = serde_json::from_str::<Value>(&stdout)
    {
        Err(val
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or(stdout))
    } else {
        Err(if stderr.is_empty() {
            format!("masil-agent exited with {}", output.status)
        } else {
            stderr
        })
    }
}

/// Lists all agents across every running masil server on this host, tagging each
/// row with `server_name`, `server_socket`, and canonical `remote_id`.
pub fn list_all_host_agents() -> Vec<Value> {
    let mut combined = Vec::new();
    for server in discover_masil_servers() {
        combined.extend(list_agents_for_server(&server.name, &server.socket));
    }
    combined
}

/// Reads `@masil-sandbox` for every pane on `server_socket` in a single batched
/// `list-panes` invocation so agent list enrichment stays O(1) subprocesses.
fn read_server_sandbox_options(server_socket: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Ok(masil) = crate::native_ui::native_executable() else {
        return out;
    };
    let Ok(res) = Command::new(masil)
        .arg("-S")
        .arg(server_socket)
        .args(["list-panes", "-a", "-F", "#{pane_id}\t#{@masil-sandbox}"])
        .output()
    else {
        return out;
    };
    if !res.status.success() {
        return out;
    }
    for line in String::from_utf8_lossy(&res.stdout).lines() {
        if let Some((pane, val)) = line.split_once('\t')
            && !pane.is_empty()
            && !val.trim().is_empty()
        {
            out.insert(pane.to_owned(), val.trim().to_owned());
        }
    }
    out
}

/// Lists all agents for one explicit masil server socket, enriched with Phase 1
/// SQLite intelligence snapshots (`audit_summary`, `breaker`, `cache_warmth`, collisions).
pub fn list_agents_for_server(server_name: &str, server_socket: &str) -> Vec<Value> {
    let mut combined = Vec::new();
    let Ok(val) = run_self_agent_json(Some(server_socket), &["list".into()]) else {
        return combined;
    };
    let Some(rows) = val.get("agents").and_then(Value::as_array) else {
        return combined;
    };
    // Load Phase 1 read-only intelligence snapshots once per server (zero extra subprocesses).
    let now = crate::observation::now_ms();
    let store = crate::managed::Store::open(Path::new(server_socket)).ok();
    let breakers_map: std::collections::HashMap<String, crate::managed::CircuitBreakerRecord> =
        store
            .as_ref()
            .and_then(|s| s.list_circuit_breakers().ok())
            .unwrap_or_default()
            .into_iter()
            .map(|r| (r.run_id.clone(), r))
            .collect();
    let warmth_map: std::collections::HashMap<String, crate::managed::CacheWarmthRecord> = store
        .as_ref()
        .and_then(|s| s.list_cache_warmth().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.run_id.clone(), r))
        .collect();
    let collisions: Vec<crate::collision::CollisionPair> = store
        .as_ref()
        .and_then(|s| s.list_collision_snapshots(None).ok())
        .unwrap_or_default()
        .into_iter()
        .map(crate::collision::CollisionPair::from_record)
        .collect();
    // Phase 3A: read `@masil-sandbox` pane options in a single batched call per server.
    let sandbox_map = read_server_sandbox_options(server_socket);

    for row in rows {
        let mut item = row.clone();
        if let Some(obj) = item.as_object_mut() {
            let pane = obj
                .get("pane_id")
                .or_else(|| obj.get("pane"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let run_id = obj
                .get("run")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let agent_name = obj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let id_fallback = obj
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let key_part = if !pane.is_empty() {
                pane.clone()
            } else {
                id_fallback
            };
            let bound_sid = obj
                .get("session_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    obj.get("binding")
                        .and_then(|b| b.get("session_id"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                });
            obj.insert("pane".into(), Value::String(pane.clone()));
            obj.insert("pane_id".into(), Value::String(pane.clone()));
            if let Some(sid) = bound_sid {
                obj.insert("session_id".into(), Value::String(sid));
            }
            obj.insert("server_name".into(), Value::String(server_name.to_owned()));
            obj.insert(
                "server_socket".into(),
                Value::String(server_socket.to_owned()),
            );
            obj.insert(
                "remote_id".into(),
                Value::String(format!("{server_name}::{key_part}")),
            );

            // Phase 1A: latest audit summary badge.
            if let Some(ref s) = store
                && let Ok(Some(rec)) = s.latest_turn_audit(
                    (!run_id.is_empty()).then_some(run_id.as_str()),
                    (!pane.is_empty()).then_some(pane.as_str()),
                )
            {
                let report = crate::audit::AuditReport::from_record(rec);
                obj.insert("audit_summary".into(), report.to_summary_json());
            }

            // Phase 1C: circuit breaker status & cache warmth gauge.
            if let Some(br) = breakers_map.get(&run_id) {
                obj.insert(
                    "breaker".into(),
                    crate::guard::breaker::record_to_json(br, None, false, None),
                );
            }
            if let Some(cw) = warmth_map.get(&run_id) {
                let snap = crate::guard::cache_warmth::CacheWarmthSnapshot::from_record(cw, now);
                obj.insert("cache_warmth".into(), snap.to_json());
            }

            // Phase 3A/3B: honest OS sandbox badge, DVR turn count, and latest policy audit pill.
            if let Some(sb_badge) = sandbox_map.get(&pane) {
                obj.insert("sandbox".into(), Value::String(sb_badge.clone()));
            }
            if let Some(ref s) = store
                && !run_id.is_empty()
            {
                if let Ok(cnt) = s.count_dvr_turns(&run_id)
                    && cnt > 0
                {
                    obj.insert("dvr_turn_count".into(), serde_json::json!(cnt));
                }
                if let Ok(logs) = s.list_permission_audit_log(Some(&run_id), 1)
                    && let Some(last) = logs.into_iter().next()
                {
                    obj.insert(
                        "last_policy_audit".into(),
                        serde_json::to_value(last).unwrap_or(Value::Null),
                    );
                }
            }

            // Phase 1B: highest active collision severity involving this agent.
            let mut worst = crate::collision::CollisionSeverity::Clean;
            let mut peers = Vec::new();
            for pair in &collisions {
                if pair.severity == crate::collision::CollisionSeverity::Clean {
                    continue;
                }
                let peer = if pair.agent_a == agent_name
                    || (!pane.is_empty() && pair.pane_a == pane)
                {
                    Some(pair.agent_b.clone())
                } else if pair.agent_b == agent_name || (!pane.is_empty() && pair.pane_b == pane) {
                    Some(pair.agent_a.clone())
                } else {
                    None
                };
                if let Some(peer_name) = peer {
                    worst = worst.max(pair.severity);
                    if !peers.contains(&peer_name) {
                        peers.push(peer_name);
                    }
                }
            }
            if worst != crate::collision::CollisionSeverity::Clean {
                obj.insert(
                    "collision_severity".into(),
                    Value::String(worst.as_str().to_owned()),
                );
                obj.insert("collision_peers".into(), serde_json::json!(peers));
            }
        }
        combined.push(item);
    }
    combined
}

fn file_mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Scans recent external sessions on disk across Claude, Codex, Antigravity,
/// and OpenCode so the mobile app can import (`resume`) them into `masil` with 1 tap.
pub fn discover_importable_sessions(limit: usize) -> Vec<ImportableSession> {
    let mut all = Vec::new();
    let per_provider = limit.clamp(10, 60);
    if let Some(home) = home_dir() {
        all.extend(discover_claude_sessions(&home, per_provider));
        all.extend(discover_codex_sessions(&home, per_provider));
        all.extend(discover_agy_sessions(&home, per_provider));
        all.extend(discover_opencode_sessions(&home, per_provider));
    }
    all.sort_by_key(|item| std::cmp::Reverse(item.last_active_ms));
    all.truncate(limit);
    all
}

pub fn discover_claude_sessions(home: &Path, limit: usize) -> Vec<ImportableSession> {
    let root = home.join(".claude").join("projects");
    let mut files = Vec::new();
    if let Ok(projects) = fs::read_dir(&root) {
        for proj in projects.flatten() {
            if !proj.path().is_dir() {
                continue;
            }
            if let Ok(entries) = fs::read_dir(proj.path()) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                        files.push((file_mtime_ms(&p), p));
                    }
                }
            }
        }
    }
    files.sort_by_key(|item| std::cmp::Reverse(item.0));
    files.truncate(limit);

    let mut out = Vec::new();
    for (mtime, path) in files {
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let (cwd, preview) = sniff_claude_header(&path);
        out.push(ImportableSession {
            provider: "claude".into(),
            session_id: stem.to_owned(),
            title: if preview.is_empty() {
                format!("Claude {}", &stem[..8.min(stem.len())])
            } else {
                preview.chars().take(64).collect()
            },
            preview,
            cwd,
            last_active_ms: mtime,
            source_path: Some(path.to_string_lossy().into_owned()),
        });
    }
    out
}

fn sniff_claude_header(path: &Path) -> (String, String) {
    let Ok(f) = File::open(path) else {
        return (String::new(), String::new());
    };
    let reader = BufReader::new(f);
    let mut cwd = String::new();
    let mut preview = String::new();
    for line in reader.lines().take(60).flatten() {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if cwd.is_empty()
            && let Some(c) = v.get("cwd").and_then(Value::as_str)
        {
            cwd = c.to_owned();
        }
        if v.get("type").and_then(Value::as_str) == Some("last-prompt")
            && let Some(lp) = v.get("lastPrompt").and_then(Value::as_str)
        {
            preview = lp.trim().chars().take(140).collect();
        } else if preview.is_empty()
            && v.get("type").and_then(Value::as_str) == Some("user")
            && let Some(msg) = v.get("message")
        {
            let text = extract_claude_content_preview(msg.get("content"));
            if !text.is_empty() {
                preview = text.chars().take(140).collect();
            }
        }
    }
    (cwd, preview)
}

fn extract_claude_content_preview(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    item.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_owned(),
        _ => String::new(),
    }
}

pub fn discover_codex_sessions(home: &Path, limit: usize) -> Vec<ImportableSession> {
    let root = home.join(".codex").join("sessions");
    let mut files = Vec::new();
    collect_jsonl_recursive(&root, 4, &mut files);
    files.sort_by_key(|item| std::cmp::Reverse(item.0));
    files.truncate(limit);

    let mut out = Vec::new();
    for (mtime, path) in files {
        let (session_id, cwd, preview) = sniff_codex_header(&path);
        if session_id.is_empty() {
            continue;
        }
        out.push(ImportableSession {
            provider: "codex".into(),
            session_id: session_id.clone(),
            title: if preview.is_empty() {
                format!("Codex {}", &session_id[..8.min(session_id.len())])
            } else {
                preview.chars().take(64).collect()
            },
            preview,
            cwd,
            last_active_ms: mtime,
            source_path: Some(path.to_string_lossy().into_owned()),
        });
    }
    out
}

fn collect_jsonl_recursive(dir: &Path, depth: usize, out: &mut Vec<(i64, PathBuf)>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_jsonl_recursive(&p, depth - 1, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push((file_mtime_ms(&p), p));
        }
    }
}

fn sniff_codex_header(path: &Path) -> (String, String, String) {
    let Ok(f) = File::open(path) else {
        return (String::new(), String::new(), String::new());
    };
    let reader = BufReader::new(f);
    let mut id = String::new();
    let mut cwd = String::new();
    let mut preview = String::new();
    for line in reader.lines().take(50).flatten() {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                if let Some(p) = v.get("payload") {
                    if id.is_empty() {
                        id = p
                            .get("id")
                            .or_else(|| p.get("session_id"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                    }
                    if cwd.is_empty() {
                        cwd = p
                            .get("cwd")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                    }
                }
            }
            Some("response_item") if preview.is_empty() => {
                if let Some(p) = v.get("payload")
                    && p.get("type").and_then(Value::as_str) == Some("message")
                    && p.get("role").and_then(Value::as_str) == Some("user")
                    && let Some(arr) = p.get("content").and_then(Value::as_array)
                {
                    let txt = arr
                        .iter()
                        .filter_map(|c| c.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(" ");
                    let trimmed = txt.trim();
                    if !trimmed.is_empty() {
                        preview = trimmed.chars().take(140).collect();
                    }
                }
            }
            _ => {}
        }
    }
    (id, cwd, preview)
}

pub fn discover_agy_sessions(home: &Path, limit: usize) -> Vec<ImportableSession> {
    let db_path = home
        .join(".gemini")
        .join("antigravity-cli")
        .join("conversation_summaries.db");
    let brain_dir = home.join(".gemini").join("antigravity-cli").join("brain");
    let Ok(conn) = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Vec::new();
    };
    let _ = conn.execute_batch("PRAGMA query_only = ON; PRAGMA busy_timeout = 500;");
    let Ok(mut stmt) = conn.prepare(
        "SELECT conversation_id, title, preview, workspace_uris, \
         CAST(strftime('%s', last_modified_time) AS INTEGER) * 1000 \
         FROM conversation_summaries \
         WHERE parent_conversation_id = '' \
         ORDER BY last_modified_time DESC LIMIT ?1",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([limit as i64], |row| {
        let id: String = row.get(0)?;
        let title: String = row.get(1).unwrap_or_default();
        let preview: String = row.get(2).unwrap_or_default();
        let uris: String = row.get(3).unwrap_or_default();
        let ts_ms: i64 = row.get(4).unwrap_or(0);
        Ok((id, title, preview, uris, ts_ms))
    }) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for row in rows.flatten() {
        let (id, title, preview, uris, ts_ms) = row;
        let cwd = parse_first_workspace_uri(&uris);
        let transcript = brain_dir
            .join(&id)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl");
        let source_path = transcript
            .exists()
            .then(|| transcript.to_string_lossy().into_owned());
        out.push(ImportableSession {
            provider: "agy".into(),
            session_id: id.clone(),
            title: if title.trim().is_empty() {
                format!("Antigravity {}", &id[..8.min(id.len())])
            } else {
                title
            },
            preview: preview.chars().take(140).collect(),
            cwd,
            last_active_ms: ts_ms,
            source_path,
        });
    }
    out
}

fn parse_first_workspace_uri(raw: &str) -> String {
    if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(raw)
        && let Some(first) = arr.first().and_then(Value::as_str)
    {
        return first.strip_prefix("file://").unwrap_or(first).to_owned();
    }
    let first = raw.split(',').next().unwrap_or("").trim();
    first.strip_prefix("file://").unwrap_or(first).to_owned()
}

pub fn discover_opencode_sessions(home: &Path, limit: usize) -> Vec<ImportableSession> {
    let db_path = home
        .join(".local")
        .join("share")
        .join("opencode")
        .join("opencode.db");
    let Ok(conn) = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Vec::new();
    };
    // Strictly read-only + indexed LIMIT query because opencode.db can exceed 2 GiB.
    let _ = conn.execute_batch("PRAGMA query_only = ON; PRAGMA busy_timeout = 500;");
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, title, directory, time_updated \
         FROM session \
         WHERE parent_id IS NULL AND time_archived IS NULL \
         ORDER BY time_updated DESC LIMIT ?1",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([limit as i64], |row| {
        let id: String = row.get(0)?;
        let title: String = row.get(1).unwrap_or_default();
        let cwd: String = row.get(2).unwrap_or_default();
        let ts_ms: i64 = row.get(3).unwrap_or(0);
        Ok((id, title, cwd, ts_ms))
    }) else {
        return Vec::new();
    };
    rows.flatten()
        .map(|(id, title, cwd, ts_ms)| ImportableSession {
            provider: "opencode".into(),
            session_id: id.clone(),
            title: if title.trim().is_empty() {
                format!("OpenCode {}", &id[..8.min(id.len())])
            } else {
                title
            },
            preview: String::new(),
            cwd,
            last_active_ms: ts_ms,
            source_path: Some(db_path.to_string_lossy().into_owned()),
        })
        .collect()
}
