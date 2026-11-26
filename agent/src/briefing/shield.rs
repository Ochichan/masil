//! Phase 2B: Deep-work Attention Shield (`focus on <25m|60m|until-09:00> | off | status`).
//!
//! While active, routine notifications (`turn_completed`, `returned_idle`,
//! low-risk approvals/questions) are held in SQLite `focus_held_events` instead
//! of ringing mobile push. Only 3 high-severity breakthrough events cut through:
//! - `circuit_breaker_tripped` (or `loop_paused`)
//! - `hard_conflict` (or `same_directory_race`)
//! - `critical_risk_permission`

use crate::managed::{FocusHeldEventRecord, FocusShieldRecord, Store};
use crate::observation::now_ms;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FocusShieldStatus {
    pub active: bool,
    pub started_ms: Option<u64>,
    pub until_ms: Option<u64>,
    pub remaining_ms: u64,
    pub reason: Option<String>,
    pub held_count: u64,
    pub updated_ms: u64,
}

/// Returns `true` if `event_kind` is one of the 3 Attention Shield breakthrough
/// exceptions allowed to alert immediately even during Focus mode.
pub fn is_breakthrough_event(event_kind: &str) -> bool {
    matches!(
        event_kind.trim().to_ascii_lowercase().as_str(),
        "circuit_breaker_tripped"
            | "loop_paused"
            | "hard_conflict"
            | "same_directory_race"
            | "critical_risk_permission"
    )
}

/// Parses Focus Shield durations such as `"25m"`, `"60m"`, `"2h"`, `"90s"`,
/// `"1h30m"`, `"until-09:00"`, or `"until-21:30"` into a target epoch millisecond.
pub fn parse_focus_until_ms(spec: &str, now_epoch_ms: u64) -> Result<u64, String> {
    let trimmed = spec.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Err("missing focus duration (e.g. 25m, 60m, 2h, until-09:00)".into());
    }

    if let Some(clock) = trimmed.strip_prefix("until-") {
        return parse_until_clock_ms(clock, now_epoch_ms);
    }

    // Parse compound or single relative duration like `25m`, `60m`, `2h`, `1h30m`, `90s`.
    let mut total_secs: u64 = 0;
    let mut num_buf = String::new();
    let mut parsed_any = false;
    for ch in trimmed.chars() {
        if ch.is_ascii_digit() {
            num_buf.push(ch);
            continue;
        }
        let val: u64 = num_buf
            .parse()
            .map_err(|_| format!("invalid duration '{spec}'"))?;
        num_buf.clear();
        let mult = match ch {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return Err(format!("unknown duration unit '{ch}' in '{spec}'")),
        };
        total_secs = total_secs.saturating_add(val.saturating_mul(mult));
        parsed_any = true;
    }
    if !num_buf.is_empty() {
        // Bare integer defaults to minutes (e.g. `25` -> 25m).
        let val: u64 = num_buf
            .parse()
            .map_err(|_| format!("invalid duration '{spec}'"))?;
        total_secs = total_secs.saturating_add(val.saturating_mul(60));
        parsed_any = true;
    }
    if !parsed_any || total_secs == 0 {
        return Err(format!("duration must be > 0: '{spec}'"));
    }
    // Cap at 7 days so accidental typos cannot permanently mute alerts.
    let clamped = total_secs.min(7 * 86_400);
    Ok(now_epoch_ms.saturating_add(clamped * 1000))
}

fn parse_until_clock_ms(clock: &str, now_epoch_ms: u64) -> Result<u64, String> {
    let Some((h_str, m_str)) = clock.split_once(':') else {
        return Err(format!("expected HH:MM after 'until-', got '{clock}'"));
    };
    let hour: i32 = h_str
        .parse()
        .map_err(|_| format!("invalid hour in '{clock}'"))?;
    let minute: i32 = m_str
        .parse()
        .map_err(|_| format!("invalid minute in '{clock}'"))?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return Err(format!("clock out of range: '{clock}'"));
    }

    let now_secs = (now_epoch_ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r writes broken-down local time into `tm`.
    let res = unsafe { libc::localtime_r(&now_secs, &mut tm) };
    if res.is_null() {
        return Err("cannot determine local timezone".into());
    }
    let cur_secs_in_day = (tm.tm_hour * 3600 + tm.tm_min * 60 + tm.tm_sec) as i64;
    let target_secs_in_day = (hour * 3600 + minute * 60) as i64;
    let delta_secs = if target_secs_in_day > cur_secs_in_day {
        target_secs_in_day - cur_secs_in_day
    } else {
        (86_400 - cur_secs_in_day) + target_secs_in_day
    };
    Ok(now_epoch_ms.saturating_add((delta_secs as u64) * 1000))
}

/// Reads the current Focus Shield status, auto-expiring it in SQLite if `until_ms <= now`.
pub fn get_status(socket: &Path) -> Result<FocusShieldStatus, String> {
    let mut store = Store::open(socket)?;
    get_status_from_store(&mut store, now_ms())
}

pub(crate) fn get_status_from_store(
    store: &mut Store,
    now: u64,
) -> Result<FocusShieldStatus, String> {
    let mut rec = store.get_focus_shield()?;
    if rec.active
        && let Some(until) = rec.until_ms
        && until <= now
    {
        rec.active = false;
        rec.updated_ms = now;
        store.set_focus_shield(&rec)?;
    }
    let held_count = store.count_focus_held_events(rec.started_ms)?;
    let remaining_ms = if rec.active {
        rec.until_ms.map(|u| u.saturating_sub(now)).unwrap_or(0)
    } else {
        0
    };
    Ok(FocusShieldStatus {
        active: rec.active,
        started_ms: rec.started_ms,
        until_ms: rec.until_ms,
        remaining_ms,
        reason: rec.reason,
        held_count,
        updated_ms: rec.updated_ms,
    })
}

/// Enables Focus Shield for the given duration spec (`25m`, `60m`, `until-09:00`, etc.).
pub fn enable(
    socket: &Path,
    duration_spec: &str,
    reason: Option<String>,
) -> Result<FocusShieldStatus, String> {
    let now = now_ms();
    let until_ms = parse_focus_until_ms(duration_spec, now)?;
    let mut store = Store::open(socket)?;
    let rec = FocusShieldRecord {
        active: true,
        started_ms: Some(now),
        until_ms: Some(until_ms),
        reason: reason
            .map(|r| r.trim().to_owned())
            .filter(|r| !r.is_empty()),
        updated_ms: now,
    };
    store.set_focus_shield(&rec)?;
    get_status_from_store(&mut store, now)
}

/// Disables Focus Shield and returns both the new status and all events held during
/// the session (so callers can immediately show a 90-second return briefing).
pub fn disable(socket: &Path) -> Result<(FocusShieldStatus, Vec<FocusHeldEventRecord>), String> {
    let now = now_ms();
    let mut store = Store::open(socket)?;
    let prev = store.get_focus_shield()?;
    let held = store.list_focus_held_events(prev.started_ms, 200)?;
    let next = FocusShieldRecord {
        active: false,
        started_ms: prev.started_ms,
        until_ms: Some(now),
        reason: prev.reason,
        updated_ms: now,
    };
    store.set_focus_shield(&next)?;
    let status = get_status_from_store(&mut store, now)?;
    Ok((status, held))
}

/// Evaluates whether an outbound push notification should be suppressed by Focus Shield.
/// If suppressed, records the event into `focus_held_events` and returns `true`.
pub fn suppress_or_record_event(
    socket: &Path,
    pane_id: &str,
    run_id: &str,
    event_kind: &str,
    summary_json: Option<&str>,
) -> bool {
    if is_breakthrough_event(event_kind) {
        return false;
    }
    let Ok(mut store) = Store::open(socket) else {
        return false;
    };
    let now = now_ms();
    let Ok(status) = get_status_from_store(&mut store, now) else {
        return false;
    };
    if !status.active {
        return false;
    }
    let _ = store.record_focus_held_event(pane_id, run_id, event_kind, summary_json, now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_relative_and_clock_durations() {
        let base = 1_700_000_000_000u64;
        assert_eq!(
            parse_focus_until_ms("25m", base).unwrap(),
            base + 25 * 60_000
        );
        assert_eq!(
            parse_focus_until_ms("60m", base).unwrap(),
            base + 60 * 60_000
        );
        assert_eq!(
            parse_focus_until_ms("2h", base).unwrap(),
            base + 2 * 3_600_000
        );
        assert_eq!(
            parse_focus_until_ms("1h30m", base).unwrap(),
            base + 90 * 60_000
        );
        assert_eq!(parse_focus_until_ms("45s", base).unwrap(), base + 45_000);
        assert!(parse_focus_until_ms("0m", base).is_err());
        assert!(parse_focus_until_ms("invalid", base).is_err());

        let until = parse_focus_until_ms("until-09:00", base).unwrap();
        assert!(until > base);
        assert!(until <= base + 86_400_000);
        assert!(parse_focus_until_ms("until-25:00", base).is_err());
    }

    #[test]
    fn enforces_only_three_breakthrough_exceptions() {
        assert!(is_breakthrough_event("circuit_breaker_tripped"));
        assert!(is_breakthrough_event("loop_paused"));
        assert!(is_breakthrough_event("hard_conflict"));
        assert!(is_breakthrough_event("same_directory_race"));
        assert!(is_breakthrough_event("critical_risk_permission"));

        assert!(!is_breakthrough_event("turn_completed"));
        assert!(!is_breakthrough_event("returned_idle"));
        assert!(!is_breakthrough_event("approval_requested"));
        assert!(!is_breakthrough_event("question_asked"));
        assert!(!is_breakthrough_event("blocked"));
    }

    #[test]
    fn focus_shield_suppresses_and_auto_expires() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "masil-focus-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // SQLITE_OPEN_NOFOLLOW rejects symlinks anywhere in the path (/var on macOS).
        let dir = dir.canonicalize().unwrap();
        let db = dir.join("ops.sqlite3");
        let mut store = Store::open_path(&db).unwrap();

        let now = 50_000u64;
        store
            .set_focus_shield(&FocusShieldRecord {
                active: true,
                started_ms: Some(now),
                until_ms: Some(now + 60_000),
                reason: Some("coding".into()),
                updated_ms: now,
            })
            .unwrap();
        let st = get_status_from_store(&mut store, now + 10_000).unwrap();
        assert!(st.active);
        assert_eq!(st.remaining_ms, 50_000);

        // Past `until_ms` auto-expires cleanly.
        let expired = get_status_from_store(&mut store, now + 60_000).unwrap();
        assert!(!expired.active);
        assert_eq!(expired.remaining_ms, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
