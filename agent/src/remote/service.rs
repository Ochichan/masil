//! Opt-in OS user service registration (`launchd` on macOS, `systemd --user`
//! on Linux) for `masil-agent remote daemon`.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const LAUNCHD_LABEL: &str = "dev.masil.remote";
const SYSTEMD_UNIT: &str = "masil-remote.service";

fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".into())
}

pub fn service_file_path() -> Result<PathBuf, String> {
    let home = home_dir()?;
    if cfg!(target_os = "macos") {
        Ok(home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist")))
    } else {
        Ok(home
            .join(".config")
            .join("systemd")
            .join("user")
            .join(SYSTEMD_UNIT))
    }
}

pub fn is_installed() -> bool {
    service_file_path().is_ok_and(|p| p.is_file())
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn captured_service_env() -> Vec<(String, String)> {
    let mut vars = Vec::new();
    for key in ["PATH", "HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME"] {
        if let Ok(val) = std::env::var(key)
            && !val.trim().is_empty()
        {
            vars.push((key.to_owned(), val));
        }
    }
    vars
}

pub fn install() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate masil-agent: {e}"))?;
    let exe_str = exe.to_string_lossy().into_owned();
    let path = service_file_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let env_vars = captured_service_env();

    if cfg!(target_os = "macos") {
        let log_dir = crate::remote::config::state_dir()?;
        crate::remote::config::ensure_private_dir(&log_dir)?;
        let log_path = log_dir.join("remote.log");
        let env_block = if env_vars.is_empty() {
            String::new()
        } else {
            let entries: String = env_vars
                .iter()
                .map(|(k, v)| {
                    format!(
                        "    <key>{}</key>\n    <string>{}</string>\n",
                        xml_escape(k),
                        xml_escape(v)
                    )
                })
                .collect();
            format!("  <key>EnvironmentVariables</key>\n  <dict>\n{entries}  </dict>\n")
        };
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>remote</string>
    <string>daemon</string>
  </array>
{env_block}  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{}</string>
  <key>StandardErrorPath</key>
  <string>{}</string>
</dict>
</plist>
"#,
            xml_escape(&exe_str),
            xml_escape(&log_path.display().to_string()),
            xml_escape(&log_path.display().to_string())
        );
        fs::write(&path, plist).map_err(|e| format!("cannot write plist: {e}"))?;
        let uid = unsafe { libc::getuid() };
        let domain = format!("gui/{uid}");
        let _ = Command::new("launchctl")
            .args(["bootout", &domain, &path.to_string_lossy()])
            .output();
        let status = Command::new("launchctl")
            .args(["bootstrap", &domain, &path.to_string_lossy()])
            .status()
            .map_err(|e| format!("launchctl bootstrap failed: {e}"))?;
        if !status.success() {
            return Ok(format!(
                "Wrote {} (run `launchctl bootstrap gui/{uid} {}` if GUI session is inactive)",
                path.display(),
                path.display()
            ));
        }
        Ok(format!(
            "Installed and started launchd agent {}",
            path.display()
        ))
    } else {
        let env_lines: String = env_vars
            .iter()
            .map(|(k, v)| {
                let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
                format!("Environment=\"{k}={escaped}\"\n")
            })
            .collect();
        let unit = format!(
            "[Unit]\nDescription=masil remote gateway daemon\nAfter=network-online.target\n\n\
             [Service]\n{env_lines}ExecStart={exe_str} remote daemon\nRestart=on-failure\nRestartSec=3\n\n\
             [Install]\nWantedBy=default.target\n"
        );
        fs::write(&path, unit).map_err(|e| format!("cannot write systemd unit: {e}"))?;
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        let _ = Command::new("systemctl")
            .args(["--user", "enable", "--now", SYSTEMD_UNIT])
            .status();
        Ok(format!("Installed systemd user unit {}", path.display()))
    }
}

pub fn uninstall() -> Result<String, String> {
    let path = service_file_path()?;
    if cfg!(target_os = "macos") {
        let uid = unsafe { libc::getuid() };
        let domain = format!("gui/{uid}");
        let _ = Command::new("launchctl")
            .args(["bootout", &domain, &path.to_string_lossy()])
            .output();
    } else {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", SYSTEMD_UNIT])
            .status();
    }
    if path.exists() {
        fs::remove_file(&path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
    }
    Ok(format!("Removed user service {}", path.display()))
}
