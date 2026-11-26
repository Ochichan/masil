//! Phase 2A (D-07): Host listening port auto-detection for E2EE `localhost` preview.
//!
//! Surfaces two tiers of loopback dev ports:
//! 1. **Recommended ports** detected from the target agent's `pane_pid` process
//!    subtree and recent PTY output (`http://localhost:<port>`, `127.0.0.1:<port>`,
//!    `[::1]:<port>`).
//! 2. **Host-wide loopback ports** listening on `127.0.0.1`, `::1`, `0.0.0.0`, or
//!    `::` (such as Docker containers or dev servers running in other tmux panes),
//!    excluding sensitive system ports configured in `TunnelConfig::denied_ports`.

use crate::native_ui::native_executable;
use crate::remote::config::TunnelConfig;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DetectedPort {
    pub port: u16,
    pub url: String,
    pub process: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// `"process_tree"` | `"pty_url"` | `"host_loopback"`
    pub source: String,
    pub recommended: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunnelPortDiscovery {
    pub recommended: Vec<DetectedPort>,
    pub host_ports: Vec<DetectedPort>,
    pub denied_ports: Vec<u16>,
}

#[derive(Debug, Clone)]
struct RawSocketEntry {
    pid: u32,
    process: String,
    port: u16,
    bind_addr: String,
}

/// Discovers recommended (agent process-tree + PTY URL) and host-wide loopback
/// listening ports while respecting `tunnel_cfg.denied_ports`.
pub fn discover_tunnel_ports(
    server_socket: Option<&str>,
    pane_id: Option<&str>,
    tunnel_cfg: &TunnelConfig,
) -> TunnelPortDiscovery {
    let mut recommended_map: BTreeMap<u16, DetectedPort> = BTreeMap::new();
    let mut host_map: BTreeMap<u16, DetectedPort> = BTreeMap::new();

    let pane_pid = match (server_socket, pane_id) {
        (Some(sock), Some(pane)) if !sock.trim().is_empty() && !pane.trim().is_empty() => {
            query_pane_pid(sock, pane)
        }
        _ => None,
    };

    let descendant_pids: HashSet<u32> = pane_pid.map(collect_descendant_pids).unwrap_or_default();

    // 1. Scan listening TCP sockets on host (macOS `lsof` / Linux `/proc`).
    for entry in scan_listening_tcp_sockets() {
        if tunnel_cfg.is_port_denied(entry.port) {
            continue;
        }
        if !is_loopback_or_wildcard_bind(&entry.bind_addr) {
            continue;
        }
        let is_agent_proc = descendant_pids.contains(&entry.pid);
        let url = format!("http://localhost:{}", entry.port);
        let label = guess_port_label(entry.port, &entry.process);
        if is_agent_proc {
            recommended_map.entry(entry.port).or_insert(DetectedPort {
                port: entry.port,
                url,
                process: entry.process.clone(),
                pid: Some(entry.pid),
                source: "process_tree".into(),
                recommended: true,
                label,
            });
        } else {
            host_map.entry(entry.port).or_insert(DetectedPort {
                port: entry.port,
                url,
                process: entry.process,
                pid: Some(entry.pid),
                source: "host_loopback".into(),
                recommended: false,
                label,
            });
        }
    }

    // 2. Scan recent PTY tail for explicit localhost URLs (e.g. Vite / Next.js / Expo banners).
    if let (Some(sock), Some(pane)) = (server_socket, pane_id)
        && !sock.trim().is_empty()
        && !pane.trim().is_empty()
        && let Some(pty_text) = capture_pane_tail(sock, pane)
    {
        for (port, full_url) in extract_localhost_urls(&pty_text) {
            if tunnel_cfg.is_port_denied(port) {
                continue;
            }
            // If this port was already discovered on the host, promote it to recommended
            // while retaining its richer process name and discovered URL path.
            if let Some(existing) = host_map.remove(&port) {
                recommended_map.entry(port).or_insert(DetectedPort {
                    port,
                    url: full_url,
                    process: existing.process,
                    pid: existing.pid,
                    source: "pty_url".into(),
                    recommended: true,
                    label: existing.label,
                });
            } else {
                recommended_map
                    .entry(port)
                    .and_modify(|e| {
                        // Prefer a richer URL path from PTY output over bare root.
                        if full_url.len() > e.url.len() {
                            e.url = full_url.clone();
                        }
                    })
                    .or_insert(DetectedPort {
                        port,
                        url: full_url,
                        process: "dev-server".into(),
                        pid: None,
                        source: "pty_url".into(),
                        recommended: true,
                        label: guess_port_label(port, "dev-server"),
                    });
            }
        }
    }

    // Remove any port from host_ports that is already promoted to recommended.
    for port in recommended_map.keys() {
        host_map.remove(port);
    }

    TunnelPortDiscovery {
        recommended: recommended_map.into_values().collect(),
        host_ports: host_map.into_values().collect(),
        denied_ports: tunnel_cfg.denied_ports.clone(),
    }
}

fn is_loopback_or_wildcard_bind(addr: &str) -> bool {
    let trimmed = addr.trim();
    matches!(
        trimmed,
        "*" | "0.0.0.0" | "::" | "[::]" | "127.0.0.1" | "::1" | "[::1]" | "localhost"
    ) || trimmed.starts_with("127.")
}

fn guess_port_label(port: u16, process: &str) -> Option<String> {
    let p = process.to_ascii_lowercase();
    if p.contains("vite") || port == 5173 || port == 4173 {
        return Some("Vite".into());
    }
    if p.contains("next") || port == 3000 {
        return Some("Web / Next".into());
    }
    if port == 8081 || p.contains("metro") || p.contains("expo") {
        return Some("Expo / Metro".into());
    }
    if port == 4200 {
        return Some("Angular".into());
    }
    if port == 6006 {
        return Some("Storybook".into());
    }
    if port == 8000 || port == 8080 || port == 8888 {
        return Some("HTTP Dev".into());
    }
    None
}

fn query_pane_pid(socket: &str, pane: &str) -> Option<u32> {
    let masil = native_executable().ok()?;
    let out = Command::new(masil)
        .args([
            "-S",
            socket,
            "display-message",
            "-p",
            "-t",
            pane,
            "#{pane_pid}",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn capture_pane_tail(socket: &str, pane: &str) -> Option<String> {
    let masil = native_executable().ok()?;
    let out = Command::new(masil)
        .args(["-S", socket, "capture-pane", "-p", "-t", pane, "-S", "-120"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn collect_descendant_pids(root_pid: u32) -> HashSet<u32> {
    let mut set = HashSet::new();
    set.insert(root_pid);
    let Ok(out) = Command::new("ps").args(["-axo", "pid=,ppid="]).output() else {
        return set;
    };
    if !out.status.success() {
        return set;
    }
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut cols = line.split_whitespace();
        let Some(pid) = cols.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(ppid) = cols.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        children.entry(ppid).or_default().push(pid);
    }
    let mut stack = vec![root_pid];
    while let Some(cur) = stack.pop() {
        if let Some(kids) = children.get(&cur) {
            for &kid in kids {
                if set.insert(kid) {
                    stack.push(kid);
                }
            }
        }
    }
    set
}

fn scan_listening_tcp_sockets() -> Vec<RawSocketEntry> {
    #[cfg(target_os = "macos")]
    {
        scan_via_lsof()
    }
    #[cfg(target_os = "linux")]
    {
        let proc_entries = scan_via_proc_net();
        if !proc_entries.is_empty() {
            return proc_entries;
        }
        scan_via_lsof()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        scan_via_lsof()
    }
}

fn scan_via_lsof() -> Vec<RawSocketEntry> {
    let Ok(out) = Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pcn"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() && out.stdout.is_empty() {
        return Vec::new();
    }
    parse_lsof_pcn(&String::from_utf8_lossy(&out.stdout))
}

/// Parses `lsof -nP -iTCP -sTCP:LISTEN -F pcn` machine-readable output.
fn parse_lsof_pcn(stdout: &str) -> Vec<RawSocketEntry> {
    let mut out = Vec::new();
    let mut cur_pid: Option<u32> = None;
    let mut cur_cmd = String::new();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            cur_pid = rest.trim().parse::<u32>().ok();
            cur_cmd.clear();
        } else if let Some(rest) = line.strip_prefix('c') {
            cur_cmd = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix('n')
            && let Some(pid) = cur_pid
        {
            // Examples: `127.0.0.1:5173`, `[::1]:3000`, `*:8080`, `localhost:4000`
            let cleaned = rest.split_whitespace().next().unwrap_or(rest).trim();
            if let Some((host, port_str)) = cleaned.rsplit_once(':')
                && let Ok(port) = port_str.parse::<u16>()
                && port > 0
            {
                out.push(RawSocketEntry {
                    pid,
                    process: if cur_cmd.is_empty() {
                        "process".into()
                    } else {
                        cur_cmd.clone()
                    },
                    port,
                    bind_addr: host.to_owned(),
                });
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn scan_via_proc_net() -> Vec<RawSocketEntry> {
    use std::fs;
    // Map listening socket inode -> (bind_addr, port)
    let mut inode_to_addr: HashMap<u64, (String, u16)> = HashMap::new();
    for (path, is_v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let Ok(body) = fs::read_to_string(path) else {
            continue;
        };
        for line in body.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 10 {
                continue;
            }
            // State "0A" == TCP_LISTEN
            if cols[3] != "0A" {
                continue;
            }
            let Some((hex_ip, hex_port)) = cols[1].split_once(':') else {
                continue;
            };
            let Ok(port) = u16::from_str_radix(hex_port, 16) else {
                continue;
            };
            let Ok(inode) = cols[9].parse::<u64>() else {
                continue;
            };
            let bind_addr = if is_v6 {
                if hex_ip == "00000000000000000000000000000000" {
                    "::".to_owned()
                } else if hex_ip == "00000000000000000000000001000000" {
                    "::1".to_owned()
                } else {
                    "non-loopback".to_owned()
                }
            } else if let Ok(v) = u32::from_str_radix(hex_ip, 16) {
                let b = v.to_le_bytes();
                format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
            } else {
                continue;
            };
            inode_to_addr.insert(inode, (bind_addr, port));
        }
    }
    if inode_to_addr.is_empty() {
        return Vec::new();
    }
    let mut entries = Vec::new();
    let Ok(proc_dir) = fs::read_dir("/proc") else {
        return entries;
    };
    for ent in proc_dir.flatten() {
        let Ok(pid) = ent.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let fd_dir = ent.path().join("fd");
        let Ok(fds) = fs::read_dir(&fd_dir) else {
            continue;
        };
        let comm = fs::read_to_string(ent.path().join("comm"))
            .map(|s| s.trim().to_owned())
            .unwrap_or_else(|_| "process".into());
        for fd in fds.flatten() {
            let Ok(link) = fs::read_link(fd.path()) else {
                continue;
            };
            let s = link.to_string_lossy();
            if let Some(inner) = s.strip_prefix("socket:[").and_then(|x| x.strip_suffix(']'))
                && let Ok(inode) = inner.parse::<u64>()
                && let Some((bind_addr, port)) = inode_to_addr.get(&inode)
            {
                entries.push(RawSocketEntry {
                    pid,
                    process: comm.clone(),
                    port: *port,
                    bind_addr: bind_addr.clone(),
                });
            }
        }
    }
    entries
}

/// Extracts `(port, normalized_url)` pairs from PTY output lines mentioning
/// `http(s)://localhost:<port>...`, `127.0.0.1:<port>...`, or `[::1]:<port>...`.
pub fn extract_localhost_urls(text: &str) -> Vec<(u16, String)> {
    let mut found: BTreeMap<u16, String> = BTreeMap::new();
    for raw_token in text.split_whitespace() {
        let token = raw_token
            .trim_matches(|c: char| {
                matches!(
                    c,
                    '"' | '\'' | '(' | ')' | '[' | ']' | ',' | ';' | '<' | '>' | '`'
                )
            })
            .trim_end_matches('.');
        let lowered = token.to_ascii_lowercase();
        let after_scheme = if let Some(rest) = lowered.strip_prefix("http://") {
            Some(("http", &token[7..], rest))
        } else if let Some(rest) = lowered.strip_prefix("https://") {
            Some(("https", &token[8..], rest))
        } else if lowered.starts_with("localhost:")
            || lowered.starts_with("127.0.0.1:")
            || lowered.starts_with("[::1]:")
        {
            Some(("http", token, lowered.as_str()))
        } else {
            None
        };
        let Some((scheme, orig_rest, low_rest)) = after_scheme else {
            continue;
        };
        let host_prefix_len = if low_rest.starts_with("localhost:") {
            "localhost:".len()
        } else if low_rest.starts_with("127.0.0.1:") {
            "127.0.0.1:".len()
        } else if low_rest.starts_with("[::1]:") {
            "[::1]:".len()
        } else {
            continue;
        };
        let after_colon = &orig_rest[host_prefix_len..];
        let port_digits: String = after_colon
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let Ok(port) = port_digits.parse::<u16>() else {
            continue;
        };
        if port == 0 {
            continue;
        }
        let path_part = &after_colon[port_digits.len()..];
        let normalized_path = if path_part.is_empty() || path_part.starts_with('/') {
            path_part
        } else {
            ""
        };
        let url = format!("{scheme}://localhost:{port}{normalized_path}");
        found
            .entry(port)
            .and_modify(|existing| {
                if url.len() > existing.len() {
                    *existing = url.clone();
                }
            })
            .or_insert(url);
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_localhost_urls_from_dev_server_banners() {
        let banner = r#"
          VITE v5.4.0  ready in 140 ms
          ➜  Local:   http://localhost:5173/dashboard
          ➜  API:     http://127.0.0.1:8000/api
          ➜  IPv6:    http://[::1]:3000/
          ➜  Bare:    localhost:4173
          ➜  Remote:  https://example.com:8443/ignore
        "#;
        let urls = extract_localhost_urls(banner);
        assert_eq!(
            urls,
            vec![
                (3000, "http://localhost:3000/".to_owned()),
                (4173, "http://localhost:4173".to_owned()),
                (5173, "http://localhost:5173/dashboard".to_owned()),
                (8000, "http://localhost:8000/api".to_owned()),
            ]
        );
    }

    #[test]
    fn parses_lsof_pcn_output_and_filters_loopback() {
        let sample = "\
p1234
cnode
n127.0.0.1:5173
n[::1]:3000
p5678
cpostgres
n127.0.0.1:5432
p9012
cnginx
n10.0.0.5:8080
n*:8081
";
        let rows = parse_lsof_pcn(sample);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].pid, 1234);
        assert_eq!(rows[0].process, "node");
        assert_eq!(rows[0].port, 5173);
        assert!(is_loopback_or_wildcard_bind(&rows[0].bind_addr));
        assert!(is_loopback_or_wildcard_bind(&rows[1].bind_addr));
        assert!(!is_loopback_or_wildcard_bind(&rows[3].bind_addr));
        assert!(is_loopback_or_wildcard_bind(&rows[4].bind_addr));

        let cfg = TunnelConfig::default();
        assert!(!cfg.is_port_denied(5173));
        assert!(cfg.is_port_denied(5432));
        assert!(cfg.is_port_denied(22));
        assert!(cfg.is_port_denied(7767));
        assert!(cfg.is_port_denied(0));
    }
}
