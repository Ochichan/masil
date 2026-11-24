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

pub fn install() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate masil-agent: {e}"))?;
    let exe_str = exe.to_string_lossy().into_owned();
    let path = service_file_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }

    if cfg!(target_os = "macos") {
        let log_dir = crate::remote::config::state_dir()?;
        crate::remote::config::ensure_private_dir(&log_dir)?;
        let log_path = log_dir.join("remote.log");
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe_str}</string>
    <string>remote</string>
    <string>daemon</string>
  </array>
  <key>RunAtLoad</key>
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
            log_path.display(),
            log_path.display()
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
        let unit = format!(
            "[Unit]\nDescription=masil remote gateway daemon\nAfter=network-online.target\n\n\
             [Service]\nExecStart={exe_str} remote daemon\nRestart=on-failure\nRestartSec=3\n\n\
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
