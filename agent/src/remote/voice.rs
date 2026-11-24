//! Mobile E2EE uploaded voice audio transcription through the host's configured
//! STT command (`remote.toml` `stt_command` or `whisper-cli` fallback).

use crate::remote::config::{RemoteConfig, state_dir};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

pub const MAX_AUDIO_BYTES: usize = 10 * 1024 * 1024;
const STT_TIMEOUT: Duration = Duration::from_secs(90);

pub struct AudioUploadBuffer {
    bytes: Vec<u8>,
}

impl AudioUploadBuffer {
    pub fn new(_channel_id: u16) -> Self {
        Self { bytes: Vec::new() }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<(), String> {
        if self.bytes.len().saturating_add(chunk.len()) > MAX_AUDIO_BYTES {
            return Err("voice audio exceeds 10 MiB limit".into());
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

struct TempAudioFile {
    path: PathBuf,
}

impl Drop for TempAudioFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub async fn transcribe_audio(
    config: &RemoteConfig,
    audio_bytes: &[u8],
    mime: &str,
) -> Result<String, String> {
    if audio_bytes.is_empty() {
        return Err("empty audio upload".into());
    }
    if audio_bytes.len() > MAX_AUDIO_BYTES {
        return Err("audio exceeds 10 MiB limit".into());
    }
    let Some(cmd_vec) = config.stt_command.as_ref().filter(|v| !v.is_empty()) else {
        return Err(
            "host_stt_not_configured: set stt_command in ~/.config/masil/remote.toml".into(),
        );
    };

    let ext = match mime {
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/m4a" | "audio/mp4" => "m4a",
        "audio/ogg" | "audio/opus" => "ogg",
        _ => "bin",
    };
    let dir = state_dir()?.join("voice-tmp");
    crate::remote::config::ensure_private_dir(&dir)?;
    let path = dir.join(format!(
        "rec-{}-{}.{ext}",
        std::process::id(),
        crate::remote::config::now_ms()
    ));
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("cannot stage audio file: {e}"))?;
        f.write_all(audio_bytes)
            .map_err(|e| format!("cannot write staged audio: {e}"))?;
    }
    let _cleanup = TempAudioFile { path: path.clone() };

    let mut command = Command::new(&cmd_vec[0]);
    let mut passed_file = false;
    for arg in &cmd_vec[1..] {
        if arg == "{file}" {
            command.arg(&path);
            passed_file = true;
        } else {
            command.arg(arg);
        }
    }
    if !passed_file {
        command.arg(&path);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = timeout(STT_TIMEOUT, command.output())
        .await
        .map_err(|_| "host STT command timed out".to_owned())?
        .map_err(|e| format!("host STT command failed to launch: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "host STT failed ({}): {}",
            output.status,
            stderr.trim().chars().take(240).collect::<String>()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(text)
}
