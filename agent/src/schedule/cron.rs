//! When a schedule is due: standard five-field cron in this machine's local
//! time, or a fixed interval. Local time comes through a function so the
//! daylight-saving rules can be tested without changing the process's zone.

use std::collections::BTreeSet;

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
/// How far ahead a cron match is looked for: a leap day can be eight
/// years away.
const HORIZON_MS: i64 = 8 * 366 * 24 * HOUR_MS;

/// The local wall-clock fields of an instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Local {
    pub(crate) year: i32,
    /// 1-12.
    pub(crate) month: u32,
    /// 1-31.
    pub(crate) day: u32,
    pub(crate) hour: u32,
    pub(crate) minute: u32,
    /// 0 = Sunday.
    pub(crate) weekday: u32,
    /// Seconds east of UTC at that instant.
    pub(crate) offset: i64,
}

impl Local {
    /// The wall-clock minute, comparable across instants.
    fn wall(&self) -> (i32, u32, u32, u32, u32) {
        (self.year, self.month, self.day, self.hour, self.minute)
    }
}

/// This machine's local time for a UTC instant in milliseconds.
pub(crate) fn system_local(ms: i64) -> Local {
    let seconds = (ms.div_euclid(1000)) as libc::time_t;
    // SAFETY: a zeroed tm is valid for localtime_r to fill.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers; localtime_r is the thread-safe form.
    unsafe { libc::localtime_r(&seconds, &mut tm) };
    Local {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        weekday: tm.tm_wday as u32,
        offset: tm.tm_gmtoff as i64,
    }
}

/// Re-reads the machine's time zone, so a changed zone counts.
pub(crate) fn refresh_zone() {
    // SAFETY: tzset has no preconditions.
    unsafe { tzset() };
}

unsafe extern "C" {
    fn tzset();
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cron {
    minutes: BTreeSet<u32>,
    hours: BTreeSet<u32>,
    days: BTreeSet<u32>,
    months: BTreeSet<u32>,
    weekdays: BTreeSet<u32>,
    /// Whether day-of-month and day-of-week were both restricted: then
    /// either matching is enough, as in standard cron.
    either_day: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Spec {
    Cron(Box<Cron>),
    /// Every so many seconds, counted from `origin_ms`.
    Every {
        seconds: u64,
    },
}

fn field(text: &str, low: u32, high: u32, name: &str) -> Result<(BTreeSet<u32>, bool), String> {
    let invalid = || {
        format!(
            "schedule_invalid: cron {name} field {text:?}; use numbers {low}-{high}, *, lists, ranges and /steps"
        )
    };
    let mut values = BTreeSet::new();
    let mut restricted = false;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, step.parse::<u32>().map_err(|_| invalid())?),
            None => (part, 1),
        };
        if step == 0 {
            return Err(invalid());
        }
        let (start, end) = if range == "*" {
            (low, high)
        } else {
            restricted = true;
            match range.split_once('-') {
                Some((a, b)) => (
                    a.parse::<u32>().map_err(|_| invalid())?,
                    b.parse::<u32>().map_err(|_| invalid())?,
                ),
                None => {
                    let value = range.parse::<u32>().map_err(|_| invalid())?;
                    // A single value with a step runs to the end, as in cron.
                    (value, if part.contains('/') { high } else { value })
                }
            }
        };
        if start < low || end > high || start > end {
            return Err(invalid());
        }
        values.extend((start..=end).step_by(step as usize));
    }
    if values.is_empty() {
        return Err(invalid());
    }
    Ok((values, restricted))
}

impl Spec {
    /// `cron:M H DOM MON DOW` or `every:SECONDS`, as stored; also accepts
    /// the bare forms from the command line through [`Spec::cron`] and
    /// [`Spec::every`].
    pub(crate) fn parse(stored: &str) -> Result<Self, String> {
        if let Some(text) = stored.strip_prefix("cron:") {
            return Self::cron(text);
        }
        if let Some(seconds) = stored.strip_prefix("every:") {
            let seconds = seconds
                .parse::<u64>()
                .map_err(|_| format!("schedule_invalid: {stored}"))?;
            return Self::every_seconds(seconds);
        }
        Err(format!("schedule_invalid: {stored}"))
    }

    pub(crate) fn cron(text: &str) -> Result<Self, String> {
        let fields: Vec<&str> = text.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            return Err(format!(
                "schedule_invalid: cron needs five fields (minute hour day month weekday): {text:?}"
            ));
        };
        let (minutes, _) = field(minute, 0, 59, "minute")?;
        let (hours, _) = field(hour, 0, 23, "hour")?;
        let (days, day_restricted) = field(day, 1, 31, "day")?;
        let (months, _) = field(month, 1, 12, "month")?;
        let (mut weekdays, weekday_restricted) = field(weekday, 0, 7, "weekday")?;
        if weekdays.remove(&7) {
            weekdays.insert(0);
        }
        Ok(Self::Cron(Box::new(Cron {
            minutes,
            hours,
            days,
            months,
            weekdays,
            either_day: day_restricted && weekday_restricted,
        })))
    }

    /// `30s`, `15m`, `2h`, `1d`; at least a minute.
    pub(crate) fn every(text: &str) -> Result<Self, String> {
        let invalid =
            || format!("schedule_invalid: interval {text:?}; use a number with s, m, h or d");
        let (at, unit) = text.char_indices().last().ok_or_else(invalid)?;
        let number = text[..at].parse::<u64>().map_err(|_| invalid())?;
        let scale = match unit {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return Err(invalid()),
        };
        Self::every_seconds(number.checked_mul(scale).ok_or_else(invalid)?)
    }

    fn every_seconds(seconds: u64) -> Result<Self, String> {
        if !(60..=366 * 86_400).contains(&seconds) {
            return Err("schedule_invalid: an interval is between 1 minute and 366 days".into());
        }
        Ok(Self::Every { seconds })
    }

    pub(crate) fn stored(&self, text: &str) -> String {
        match self {
            Self::Cron(_) => format!(
                "cron:{}",
                text.split_whitespace().collect::<Vec<_>>().join(" ")
            ),
            Self::Every { seconds } => format!("every:{seconds}"),
        }
    }

    /// The first due instant strictly after `after_ms`. A cron time the
    /// clock shows twice, when it is set back, runs only the first time.
    pub(crate) fn next_after(
        &self,
        after_ms: i64,
        origin_ms: i64,
        local: &dyn Fn(i64) -> Local,
    ) -> Option<i64> {
        match self {
            Self::Every { seconds } => {
                let step = *seconds as i64 * 1000;
                let elapsed = (after_ms - origin_ms).max(-1);
                Some(origin_ms + (elapsed.div_euclid(step) + 1) * step)
            }
            Self::Cron(cron) => {
                let end = after_ms + HORIZON_MS;
                let mut candidate = (after_ms.div_euclid(MINUTE_MS) + 1) * MINUTE_MS;
                while candidate <= end {
                    let at = local(candidate);
                    let day = cron.day_matches(&at) && cron.months.contains(&at.month);
                    if !day {
                        // Whole hours, so a zone change inside the day is seen.
                        candidate += HOUR_MS - i64::from(at.minute) * MINUTE_MS;
                        continue;
                    }
                    if !cron.hours.contains(&at.hour) {
                        candidate += HOUR_MS - i64::from(at.minute) * MINUTE_MS;
                        continue;
                    }
                    if cron.minutes.contains(&at.minute) && !shown_again(candidate, &at, local) {
                        return Some(candidate);
                    }
                    candidate += MINUTE_MS;
                }
                None
            }
        }
    }
}

/// Whether the clock showed this local minute once already: it was set
/// back by Δ during the last three hours and the minute Δ earlier reads the
/// same. Holds for any shift size and needs no memory of earlier runs.
fn shown_again(at_ms: i64, at: &Local, local: &dyn Fn(i64) -> Local) -> bool {
    let earlier = local(at_ms - 3 * HOUR_MS);
    let back = earlier.offset - at.offset;
    if back <= 0 {
        return false;
    }
    let before = local(at_ms - back * 1000);
    before.wall() == at.wall()
}

impl Cron {
    fn day_matches(&self, at: &Local) -> bool {
        let day = self.days.contains(&at.day);
        let weekday = self.weekdays.contains(&at.weekday);
        if self.either_day {
            day || weekday
        } else {
            day && weekday
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zone at UTC+0 that, on 2026-03-08 at 02:00, jumps to 03:00 and on
    /// 2026-11-01 at 02:00 falls back to 01:00 (like US rules, shifted).
    fn shifting(ms: i64) -> Local {
        let spring = utc(2026, 3, 8, 2, 0);
        let fall = utc(2026, 11, 1, 1, 0);
        let offset = if ms >= spring && ms < fall { 3_600 } else { 0 };
        fields(ms + offset * 1000, offset)
    }

    /// Lord Howe: +10:30, and +11:00 in summer; back at 02:00 local.
    fn half_hour(ms: i64) -> Local {
        let back = utc(2026, 4, 4, 15, 0);
        let offset = if ms < back { 39_600 } else { 37_800 };
        fields(ms + offset * 1000, offset)
    }

    fn utc_local(ms: i64) -> Local {
        fields(ms, 0)
    }

    fn fields(ms: i64, offset: i64) -> Local {
        let days = ms.div_euclid(86_400_000);
        let minute_of_day = ms.rem_euclid(86_400_000) / 60_000;
        // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
        Local {
            year,
            month,
            day,
            hour: (minute_of_day / 60) as u32,
            minute: (minute_of_day % 60) as u32,
            weekday: ((days + 4).rem_euclid(7)) as u32,
            offset,
        }
    }

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        let y = i64::from(year) - i64::from(month <= 2);
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = i64::from(month);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        (days * 1440 + i64::from(hour) * 60 + i64::from(minute)) * 60_000
    }

    #[test]
    fn fields_parse_and_refuse() {
        assert!(Spec::cron("0 9 * * 1-5").is_ok());
        assert!(Spec::cron("*/15 * * * *").is_ok());
        assert!(Spec::cron("0 0 1,15 * 7").is_ok());
        for bad in [
            "",
            "0 9 * *",
            "60 * * * *",
            "0 24 * * *",
            "0 0 0 * *",
            "*/0 * * * *",
            "a * * * *",
            "5-1 * * * *",
        ] {
            assert!(Spec::cron(bad).is_err(), "{bad}");
        }
        assert_eq!(Spec::every("30m"), Ok(Spec::Every { seconds: 1800 }));
        assert!(Spec::every("30s").is_err());
        assert!(Spec::every("5x").is_err());
        assert!(Spec::every("30분").is_err());
        assert!(Spec::every("").is_err());
        assert!(Spec::every("213503982334602d").is_err());
    }

    #[test]
    fn weekday_mornings_and_steps() {
        let spec = Spec::cron("0 9 * * 1-5").unwrap();
        // Friday 2026-10-02 10:00 → Monday 10-05 09:00.
        let friday = utc(2026, 10, 2, 10, 0);
        assert_eq!(
            spec.next_after(friday, 0, &utc_local),
            Some(utc(2026, 10, 5, 9, 0))
        );
        let quarter = Spec::cron("*/15 * * * *").unwrap();
        assert_eq!(
            quarter.next_after(utc(2026, 1, 1, 0, 7), 0, &utc_local),
            Some(utc(2026, 1, 1, 0, 15))
        );
        // Day of month or weekday, as cron: the 13th or any Friday.
        let either = Spec::cron("0 0 13 * 5").unwrap();
        assert_eq!(
            either.next_after(utc(2026, 2, 1, 0, 0), 0, &utc_local),
            Some(utc(2026, 2, 6, 0, 0))
        );
    }

    #[test]
    fn a_time_that_does_not_exist_is_skipped_that_day() {
        let spec = Spec::cron("30 2 * * *").unwrap();
        // 2026-03-08 02:30 local does not exist: next is 03-09 02:30 local.
        let before = utc(2026, 3, 8, 0, 0);
        let due = spec.next_after(before, 0, &shifting).unwrap();
        assert_eq!(shifting(due).wall(), (2026, 3, 9, 2, 30));
    }

    #[test]
    fn a_time_shown_twice_runs_once() {
        let spec = Spec::cron("30 1 * * *").unwrap();
        // On 2026-11-01, 01:30 local happens at 00:30 UTC (summer) and again
        // at 01:30 UTC (winter).
        let first = spec
            .next_after(utc(2026, 10, 31, 23, 0), 0, &shifting)
            .unwrap();
        assert_eq!(first, utc(2026, 11, 1, 0, 30));
        let second = spec.next_after(first, 0, &shifting).unwrap();
        assert_eq!(shifting(second).wall(), (2026, 11, 2, 1, 30));
        // Every half hour through the hour shown twice: four runs, not six.
        let halves = Spec::cron("0,30 1 * * *").unwrap();
        let mut at = utc(2026, 10, 31, 23, 0);
        let mut runs = Vec::new();
        while let Some(due) = halves.next_after(at, 0, &shifting) {
            if due > utc(2026, 11, 1, 3, 0) {
                break;
            }
            runs.push(shifting(due).wall());
            at = due;
        }
        assert_eq!(runs, [(2026, 11, 1, 1, 0), (2026, 11, 1, 1, 30)]);
        // A half-hour shift, at 02:00 back to 01:30.
        let late = Spec::cron("45 1 * * *").unwrap();
        let mut at = utc(2026, 4, 4, 12, 0);
        let mut runs = Vec::new();
        while let Some(due) = late.next_after(at, 0, &half_hour) {
            if due > utc(2026, 4, 4, 17, 0) {
                break;
            }
            runs.push(due);
            at = due;
        }
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn intervals_count_from_their_origin() {
        let spec = Spec::Every { seconds: 600 };
        let origin = utc(2026, 1, 1, 0, 3);
        assert_eq!(
            spec.next_after(origin, origin, &utc_local),
            Some(origin + 600_000)
        );
        assert_eq!(
            spec.next_after(origin + 601_000, origin, &utc_local),
            Some(origin + 1_200_000)
        );
    }
}
