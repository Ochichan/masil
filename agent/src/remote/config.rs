//! Persistent configuration, X25519 host identity, paired mobile devices,
//! and short-lived QR pairing tokens for `masil-agent remote`.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use masil_wire::noise::{KEY_LEN, Keypair};
use masil_wire::pairing::PairingOffer;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const PAIRING_TTL_MS: i64 = 5 * 60 * 1000;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn config_dir() -> Result<PathBuf, String> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("masil"));
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config").join("masil"))
}

pub fn state_dir() -> Result<PathBuf, String> {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("masil").join("remote"));
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("masil")
        .join("remote"))
}

pub fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot chmod {}: {e}", dir.display()))
}

fn write_private_atomic(path: &Path, content: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("cannot open {}: {e}", tmp.display()))?;
    f.write_all(content)
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    f.sync_all()
        .map_err(|e| format!("cannot sync {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("cannot move {}: {e}", path.display()))
}

pub fn random_token_b64(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

pub fn sha256_hex(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

pub fn default_hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into buf.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0 {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        if let Ok(s) = std::str::from_utf8(&buf[..len]) {
            let trimmed = s.trim().trim_end_matches(".local");
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
    }
    "masil-host".into()
}

fn sanitize_host_id(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch.to_ascii_lowercase());
        } else if ch.is_whitespace() || ch == '.' {
            out.push('-');
        }
        if out.len() >= 32 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_owned();
    if trimmed.is_empty() {
        "masil-host".into()
    } else {
        trimmed
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub relay_url: Option<String>,
    #[serde(default)]
    pub listen_addr: Option<String>,
    #[serde(default)]
    pub advertise_direct: Option<String>,
    #[serde(default)]
    pub host_name: Option<String>,
    #[serde(default = "default_true")]
    pub push_enabled: bool,
    #[serde(default)]
    pub stt_command: Option<Vec<String>>,
}

fn default_true() -> bool {
    true
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            relay_url: None,
            listen_addr: Some("127.0.0.1:7767".into()),
            advertise_direct: None,
            host_name: None,
            push_enabled: true,
            stt_command: None,
        }
    }
}

impl RemoteConfig {
    pub fn path() -> Result<PathBuf, String> {
        Ok(config_dir()?.join("remote.toml"))
    }

    pub fn load() -> Result<Self, String> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        toml::from_str(&body).map_err(|e| format!("invalid remote.toml: {e}"))
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path()?;
        let body = toml::to_string_pretty(self)
            .map_err(|e| format!("cannot serialize remote.toml: {e}"))?;
        write_private_atomic(&path, body.as_bytes())
    }

    pub fn effective_host_name(&self) -> String {
        self.host_name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(default_hostname)
    }

    pub fn relay_host_ws_url(&self, host_id: &str, host_token: &str) -> Option<String> {
        let relay = self.relay_url.as_ref()?.trim().trim_end_matches('/');
        let base = if relay.ends_with("/v1") {
            relay.to_owned()
        } else {
            format!("{relay}/v1")
        };
        Some(format!("{base}/host/{host_id}?token={host_token}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostIdentityFile {
    pub host_id: String,
    pub host_token: String,
    pub secret_b64: String,
    pub public_b64: String,
    pub created_ms: i64,
}

#[derive(Clone)]
pub struct HostIdentity {
    pub host_id: String,
    pub host_token: String,
    pub keypair: Keypair,
}

impl HostIdentity {
    pub fn load_or_create() -> Result<Self, String> {
        let path = state_dir()?.join("host-keypair.json");
        if path.exists() {
            let body = fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let stored: HostIdentityFile = serde_json::from_str(&body)
                .map_err(|e| format!("invalid host-keypair.json: {e}"))?;
            let raw = STANDARD
                .decode(stored.secret_b64.as_bytes())
                .map_err(|_| "invalid secret_b64 in host-keypair.json")?;
            if raw.len() != KEY_LEN {
                return Err("host static key must be 32 bytes".into());
            }
            let mut arr = [0u8; KEY_LEN];
            arr.copy_from_slice(&raw);
            let keypair = Keypair::from_secret_bytes(arr);
            return Ok(Self {
                host_id: stored.host_id,
                host_token: stored.host_token,
                keypair,
            });
        }

        let keypair = Keypair::generate();
        let suffix = random_token_b64(4)
            .to_ascii_lowercase()
            .replace(['-', '_'], "0");
        let host_id = format!(
            "{}-{}",
            sanitize_host_id(&default_hostname()),
            &suffix[..6.min(suffix.len())]
        );
        let host_token = random_token_b64(32);
        let stored = HostIdentityFile {
            host_id: host_id.clone(),
            host_token: host_token.clone(),
            secret_b64: STANDARD.encode(keypair.secret_bytes()),
            public_b64: STANDARD.encode(keypair.public_bytes()),
            created_ms: now_ms(),
        };
        let json = serde_json::to_vec_pretty(&stored).map_err(|e| e.to_string())?;
        write_private_atomic(&path, &json)?;
        Ok(Self {
            host_id,
            host_token,
            keypair,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedDevice {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub secret_hash: String,
    #[serde(default)]
    pub push_token: Option<String>,
    pub paired_at_ms: i64,
    pub last_seen_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPairToken {
    pub token_hash: String,
    pub expires_at_ms: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DeviceStoreData {
    #[serde(default)]
    pub devices: Vec<PairedDevice>,
    #[serde(default)]
    pub pending_pairs: Vec<PendingPairToken>,
    #[serde(default)]
    pub manual_bindings: HashMap<String, String>,
}

pub struct DeviceStore;

impl DeviceStore {
    fn path() -> Result<PathBuf, String> {
        Ok(state_dir()?.join("devices.json"))
    }

    pub fn load() -> Result<DeviceStoreData, String> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(DeviceStoreData::default());
        }
        let body = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        serde_json::from_str(&body).map_err(|e| format!("invalid devices.json: {e}"))
    }

    pub fn save(data: &DeviceStoreData) -> Result<(), String> {
        let path = Self::path()?;
        let body = serde_json::to_vec_pretty(data).map_err(|e| e.to_string())?;
        write_private_atomic(&path, &body)
    }

    pub fn create_pairing_offer(
        identity: &HostIdentity,
        config: &RemoteConfig,
    ) -> Result<(PairingOffer, String), String> {
        let pair_token = random_token_b64(24);
        let mut data = Self::load()?;
        let now = now_ms();
        data.pending_pairs.retain(|p| p.expires_at_ms > now);
        data.pending_pairs.push(PendingPairToken {
            token_hash: sha256_hex(pair_token.as_bytes()),
            expires_at_ms: now + PAIRING_TTL_MS,
        });
        Self::save(&data)?;

        let direct_addr = resolve_advertised_direct(config);
        let offer = PairingOffer {
            version: masil_wire::PROTOCOL_VERSION,
            host_id: identity.host_id.clone(),
            host_pub: *identity.keypair.public_bytes(),
            pair_token,
            relay_url: config.relay_url.clone(),
            direct_addr,
            host_name: config.effective_host_name(),
        };
        let uri = offer.to_uri().map_err(|e| e.to_string())?;
        Ok((offer, uri))
    }

    pub fn redeem_pair_token(
        pair_token: &str,
        device_name: &str,
        platform: &str,
    ) -> Result<(PairedDevice, String), String> {
        let mut data = Self::load()?;
        let now = now_ms();
        let hash = sha256_hex(pair_token.as_bytes());
        let pos = data
            .pending_pairs
            .iter()
            .position(|p| p.token_hash == hash && p.expires_at_ms > now)
            .ok_or("invalid or expired pairing token")?;
        data.pending_pairs.remove(pos);

        let device_id = format!("dev-{}", &random_token_b64(9));
        let device_secret = random_token_b64(32);
        let device = PairedDevice {
            device_id,
            name: if device_name.trim().is_empty() {
                "Mobile device".into()
            } else {
                device_name.trim().chars().take(64).collect()
            },
            platform: match platform {
                "ios" | "android" | "web" | "cli" => platform.into(),
                _ => "unknown".into(),
            },
            secret_hash: sha256_hex(device_secret.as_bytes()),
            push_token: None,
            paired_at_ms: now,
            last_seen_ms: now,
        };
        data.devices.push(device.clone());
        Self::save(&data)?;
        Ok((device, device_secret))
    }

    pub fn authenticate_device(
        device_id: &str,
        device_secret: &str,
    ) -> Result<PairedDevice, String> {
        let mut data = Self::load()?;
        let hash = sha256_hex(device_secret.as_bytes());
        let dev = data
            .devices
            .iter_mut()
            .find(|d| d.device_id == device_id && d.secret_hash == hash)
            .ok_or("device_unauthorized")?;
        dev.last_seen_ms = now_ms();
        let cloned = dev.clone();
        Self::save(&data)?;
        Ok(cloned)
    }

    pub fn register_push_token(device_id: &str, platform: &str, token: &str) -> Result<(), String> {
        let mut data = Self::load()?;
        let dev = data
            .devices
            .iter_mut()
            .find(|d| d.device_id == device_id)
            .ok_or("device_not_found")?;
        dev.platform = platform.to_owned();
        dev.push_token = if token.trim().is_empty() {
            None
        } else {
            Some(token.trim().to_owned())
        };
        Self::save(&data)
    }

    pub fn revoke(device_id: &str) -> Result<bool, String> {
        let mut data = Self::load()?;
        let before = data.devices.len();
        data.devices.retain(|d| d.device_id != device_id);
        let removed = data.devices.len() < before;
        if removed {
            Self::save(&data)?;
        }
        Ok(removed)
    }

    pub fn set_manual_binding(agent_key: &str, session_id: &str) -> Result<(), String> {
        let mut data = Self::load()?;
        if session_id.trim().is_empty() {
            data.manual_bindings.remove(agent_key);
        } else {
            data.manual_bindings
                .insert(agent_key.to_owned(), session_id.trim().to_owned());
        }
        Self::save(&data)
    }

    pub fn get_manual_binding(agent_key: &str) -> Option<String> {
        Self::load().ok()?.manual_bindings.get(agent_key).cloned()
    }
}

pub fn detect_preferred_direct_ip() -> Option<String> {
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs allocates a linked list freed by freeifaddrs below.
    if unsafe { libc::getifaddrs(&mut ifap) } != 0 || ifap.is_null() {
        return None;
    }
    let mut tailscale: Option<String> = None;
    let mut lan_192: Option<String> = None;
    let mut lan_other: Option<String> = None;

    let mut cur = ifap;
    while !cur.is_null() {
        // SAFETY: cur is a valid node from getifaddrs.
        let ifa = unsafe { &*cur };
        if !ifa.ifa_addr.is_null() {
            let flags = ifa.ifa_flags as i32;
            let is_up = (flags & libc::IFF_UP) != 0;
            let is_loop = (flags & libc::IFF_LOOPBACK) != 0;
            // SAFETY: sa_family is present on all sockaddr values.
            let family = unsafe { (*ifa.ifa_addr).sa_family } as i32;
            if is_up && !is_loop && family == libc::AF_INET {
                // SAFETY: family == AF_INET guarantees sockaddr_in layout.
                let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
                let octets = u32::from_be(sin.sin_addr.s_addr).to_be_bytes();
                let ip = format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], octets[3]);
                if octets[0] == 100 && (64..=127).contains(&octets[1]) {
                    if tailscale.is_none() {
                        tailscale = Some(ip);
                    }
                } else if octets[0] == 192 && octets[1] == 168 {
                    if lan_192.is_none() {
                        lan_192 = Some(ip);
                    }
                } else if (octets[0] == 10 || (octets[0] == 172 && (16..=31).contains(&octets[1])))
                    && lan_other.is_none()
                {
                    lan_other = Some(ip);
                }
            }
        }
        cur = ifa.ifa_next;
    }
    // SAFETY: ifap was allocated by getifaddrs.
    unsafe { libc::freeifaddrs(ifap) };
    lan_192.or(tailscale).or(lan_other)
}

pub fn resolve_advertised_direct(config: &RemoteConfig) -> Option<String> {
    if let Some(ref adv) = config.advertise_direct
        && !adv.trim().is_empty()
    {
        return Some(adv.trim().to_owned());
    }
    let listen = config.listen_addr.as_deref()?.trim();
    if listen.is_empty() {
        return None;
    }
    let port = listen
        .rsplit_once(':')
        .map(|(_, p)| p)
        .filter(|p| !p.is_empty())
        .unwrap_or("7767");
    if (listen.starts_with("127.0.0.1:")
        || listen.starts_with("0.0.0.0:")
        || listen.starts_with("[::]:")
        || listen.starts_with(":::"))
        && let Some(ip) = detect_preferred_direct_ip()
    {
        return Some(format!("{ip}:{port}"));
    }
    Some(listen.to_owned())
}

pub fn render_terminal_qr(uri: &str) -> Result<Vec<String>, String> {
    let code = qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::L)
        .map_err(|e| format!("qr encode failed: {e}"))?;
    let width = code.width() as i32;
    let colors = code.to_colors();
    let is_dark = |x: i32, y: i32| -> bool {
        if x < 0 || y < 0 || x >= width || y >= width {
            false
        } else {
            matches!(
                colors[(y as usize) * (width as usize) + (x as usize)],
                qrcode::Color::Dark
            )
        }
    };
    // 2-module quiet zone on all four sides (even bottom padding so bottom quiet
    // zone is crisp and never truncated). Foreground = pure white, background = pure black.
    let quiet = 2i32;
    let min_x = -quiet;
    let max_x = width + quiet;
    let min_y = -quiet;
    let mut max_y = width + quiet;
    if (max_y - min_y) % 2 != 0 {
        max_y += 1;
    }
    let mut lines = Vec::new();
    let mut y = min_y;
    while y < max_y {
        let mut row = String::with_capacity((max_x - min_x) as usize);
        for x in min_x..max_x {
            let top = is_dark(x, y);
            let bottom = is_dark(x, y + 1);
            let ch = match (top, bottom) {
                (true, true) => ' ',
                (true, false) => '▄',
                (false, true) => '▀',
                (false, false) => '█',
            };
            row.push(ch);
        }
        lines.push(row);
        y += 2;
    }
    Ok(lines)
}

pub fn write_qr_html_preview(uri: &str, host_name: &str) -> Result<PathBuf, String> {
    let code = qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::L)
        .map_err(|e| format!("qr encode failed: {e}"))?;
    let width = code.width();
    let colors = code.to_colors();
    let quiet = 4usize;
    let total = width + quiet * 2;

    let mut rects = String::new();
    for y in 0..width {
        for x in 0..width {
            if matches!(colors[y * width + x], qrcode::Color::Dark) {
                use std::fmt::Write;
                let _ = write!(
                    &mut rects,
                    "<rect x=\"{}\" y=\"{}\" width=\"1\" height=\"1\" fill=\"#000\"/>",
                    x + quiet,
                    y + quiet
                );
            }
        }
    }
    let esc_uri = uri
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let esc_host = host_name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let html = format!(
        r##"<!doctype html>
<html lang="ko"><head><meta charset="utf-8"><title>masil Pairing QR — {esc_host}</title>
<style>
body {{ margin:0; min-height:100vh; display:flex; flex-direction:column; align-items:center; justify-content:center;
       background:#0c1016; color:#e6ebf1; font-family:-apple-system,BlinkMacSystemFont,sans-serif; padding:24px; }}
.card {{ background:#141a22; border:1px solid #26313d; border-radius:16px; padding:28px; max-width:460px; text-align:center; }}
.qr {{ background:#fff; padding:16px; border-radius:12px; display:inline-block; margin:16px 0; }}
.uri {{ font-family:ui-monospace,Menlo,monospace; font-size:11px; color:#8d99a7; word-break:break-all;
        background:#0c1016; padding:10px; border-radius:8px; margin-top:10px; text-align:left; }}
button {{ background:#5bb5c2; color:#0c1016; border:0; border-radius:8px; padding:10px 18px; font-weight:700; cursor:pointer; margin-top:12px; }}
</style></head><body><div class="card">
<h2 style="margin:0 0 6px 0">masil 모바일 페어링</h2>
<div style="color:#8d99a7;font-size:13px">{esc_host} · 5분간 유효</div>
<div class="qr"><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {total} {total}" width="320" height="320" shape-rendering="crispEdges">
<rect width="{total}" height="{total}" fill="#fff"/>{rects}</svg></div>
<div class="uri" id="u">{esc_uri}</div>
<button onclick="navigator.clipboard.writeText(document.getElementById('u').textContent);this.textContent='복사됨!'">URI 복사</button>
</div></body></html>"##
    );
    let path = state_dir()?.join("pair-qr.html");
    write_private_atomic(&path, html.as_bytes())?;
    Ok(path)
}
