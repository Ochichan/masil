//! Opt-in per-user remote gateway (`masil-agent remote`), E2EE pairing,
//! multi-socket discovery, `-C` terminal streaming, and 4-provider live
//! transcripts.

pub mod config;
pub mod daemon;
pub mod discovery;
pub mod files;
pub mod hooks;
pub mod rpc;
pub mod service;
pub mod terminal_stream;
pub mod transcripts;
pub mod voice;

use crate::remote::config::{
    DeviceStore, HostIdentity, RemoteConfig, render_terminal_qr, state_dir, write_qr_html_preview,
};
use serde_json::json;
use std::fs;

pub fn run(args: &[String]) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("help");
    match sub {
        "help" | "--help" | "-h" => {
            println!(
                "masil-agent remote — opt-in mobile gateway, E2EE pairing, and TUI onboarding\n\n\
                 Commands:\n  \
                   masil-agent remote setup [--lang en|ko] [--theme dark|light|terminal]\n  \
                   masil-agent remote daemon\n  \
                   masil-agent remote status\n  \
                   masil-agent remote pair [--relay WSS_URL] [--direct HOST:PORT] [--listen HOST:PORT|none] [--open] [--json]\n  \
                   masil-agent remote devices\n  \
                   masil-agent remote revoke DEVICE_ID\n  \
                   masil-agent remote setup-hooks [--install claude|codex|opencode|agy|all] [--remove PROVIDER]\n  \
                   masil-agent remote install-service | uninstall-service"
            );
            Ok(0)
        }
        "daemon" => daemon::run_daemon(),
        "setup" => crate::ui::remote_onboarding::run(&args[1..]),
        "status" => {
            let cfg = RemoteConfig::load()?;
            let id = HostIdentity::load_or_create()?;
            let devs = DeviceStore::load()?;
            let pid = read_daemon_pid();
            let servers = discovery::discover_masil_servers();
            let hook_list = hooks::inspect_all();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "enabled": cfg.enabled,
                    "host_id": id.host_id,
                    "host_name": cfg.effective_host_name(),
                    "relay_url": cfg.relay_url,
                    "listen_addr": cfg.listen_addr,
                    "advertise_direct": cfg.advertise_direct,
                    "daemon_pid": pid,
                    "daemon_running": pid.is_some(),
                    "service_installed": service::is_installed(),
                    "paired_devices": devs.devices.len(),
                    "masil_servers": servers,
                    "hooks": hook_list,
                }))
                .map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        "pair" => {
            let mut cfg = RemoteConfig::load()?;
            let mut json_only = false;
            let mut open_browser = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--relay" if i + 1 < args.len() => {
                        cfg.relay_url = Some(args[i + 1].clone());
                        cfg.enabled = true;
                        cfg.save()?;
                        i += 2;
                    }
                    "--direct" if i + 1 < args.len() => {
                        cfg.advertise_direct = Some(args[i + 1].clone());
                        cfg.enabled = true;
                        cfg.save()?;
                        i += 2;
                    }
                    "--listen" if i + 1 < args.len() => {
                        let val = args[i + 1].trim();
                        cfg.listen_addr = if val.is_empty() || val.eq_ignore_ascii_case("none") {
                            None
                        } else {
                            Some(val.to_owned())
                        };
                        cfg.enabled = true;
                        cfg.save()?;
                        i += 2;
                    }
                    "--open" => {
                        open_browser = true;
                        i += 1;
                    }
                    "--json" => {
                        json_only = true;
                        i += 1;
                    }
                    other => return Err(format!("unknown pair flag: {other}")),
                }
            }
            let identity = HostIdentity::load_or_create()?;
            let (offer, uri) = DeviceStore::create_pairing_offer(&identity, &cfg)?;
            let qr_html = write_qr_html_preview(&uri, &offer.host_name).ok();
            if open_browser && let Some(ref html_path) = qr_html {
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("open").arg(html_path).spawn();
                #[cfg(target_os = "linux")]
                let _ = std::process::Command::new("xdg-open")
                    .arg(html_path)
                    .spawn();
            }
            if json_only {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "uri": uri,
                        "host_id": offer.host_id,
                        "host_name": offer.host_name,
                        "relay_url": offer.relay_url,
                        "direct_addr": offer.direct_addr,
                        "qr_html": qr_html.as_ref().map(|p| p.display().to_string()),
                        "expires_in_seconds": 300,
                    }))
                    .map_err(|e| e.to_string())?
                );
            } else {
                for line in render_terminal_qr(&uri)? {
                    println!("\x1b[97;40m{line}\x1b[0m");
                }
                println!("\nPairing URI (valid 5 minutes):\n{uri}");
                if let Some(ref html_path) = qr_html {
                    println!(
                        "High-res SVG QR saved: {} (use --open to view in browser)",
                        html_path.display()
                    );
                }
            }
            Ok(0)
        }
        "devices" => {
            let devs = DeviceStore::load()?;
            let rows: Vec<_> = devs
                .devices
                .iter()
                .map(|d| {
                    json!({
                        "device_id": d.device_id,
                        "name": d.name,
                        "platform": d.platform,
                        "push_registered": d.push_token.is_some(),
                        "paired_at_ms": d.paired_at_ms,
                        "last_seen_ms": d.last_seen_ms,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"devices": rows}))
                    .map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        "revoke" => {
            let Some(dev_id) = args.get(1) else {
                return Err("usage: masil-agent remote revoke DEVICE_ID".into());
            };
            let removed = DeviceStore::revoke(dev_id)?;
            println!(
                "{}",
                serde_json::to_string(&json!({"device_id": dev_id, "revoked": removed}))
                    .map_err(|e| e.to_string())?
            );
            Ok(if removed { 0 } else { 3 })
        }
        "setup-hooks" => {
            if args.len() == 1 {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"hooks": hooks::inspect_all()}))
                        .map_err(|e| e.to_string())?
                );
                return Ok(0);
            }
            let mut results = Vec::new();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--install" if i + 1 < args.len() => {
                        let target = args[i + 1].as_str();
                        let providers: Vec<&str> = if target == "all" {
                            vec!["claude", "codex", "opencode", "agy"]
                        } else {
                            vec![target]
                        };
                        for p in providers {
                            results.push(hooks::install_provider(p)?);
                        }
                        i += 2;
                    }
                    "--remove" if i + 1 < args.len() => {
                        let target = args[i + 1].as_str();
                        let providers: Vec<&str> = if target == "all" {
                            vec!["claude", "codex", "opencode", "agy"]
                        } else {
                            vec![target]
                        };
                        for p in providers {
                            results.push(hooks::uninstall_provider(p)?);
                        }
                        i += 2;
                    }
                    other => return Err(format!("unknown setup-hooks option: {other}")),
                }
            }
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"results": results, "hooks": hooks::inspect_all()})
                )
                .map_err(|e| e.to_string())?
            );
            Ok(0)
        }
        "install-service" => {
            let msg = service::install()?;
            println!("{msg}");
            Ok(0)
        }
        "uninstall-service" => {
            let msg = service::uninstall()?;
            println!("{msg}");
            Ok(0)
        }
        other => Err(format!(
            "unknown remote subcommand: {other}; see `masil-agent remote --help`"
        )),
    }
}

pub fn read_daemon_pid() -> Option<u32> {
    let path = state_dir().ok()?.join("daemon.pid");
    let pid = fs::read_to_string(path).ok()?.trim().parse::<u32>().ok()?;
    // SAFETY: kill(pid, 0) only probes process existence.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 { Some(pid) } else { None }
}
