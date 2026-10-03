//! Scheduled actions run by the coordinator (P7b, docs/schedules.md).

pub(crate) mod cron;

pub(crate) use cron::{Local, Spec};

/// A due time may start this late; later it is missed.
pub(crate) const GRACE_MS: i64 = 60_000;
/// Schedules a server may have.
pub(crate) const MAX_SCHEDULES: i64 = 64;
/// Due times counted one by one before the count skips to the last two
/// days; a minute schedule off for a month costs no more than this.
const COUNT_LIMIT: u64 = 5_000;
const COUNT_SPAN_MS: i64 = 2 * 24 * 60 * 60 * 1000;

/// What is due for one schedule.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Due {
    /// The due time to run now: within [`GRACE_MS`] of now.
    pub(crate) run: Option<i64>,
    /// The latest missed due time, how many were missed, and whether that
    /// count is only a lower bound.
    pub(crate) missed: Option<Missed>,
    /// The first due time after now.
    pub(crate) next: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Missed {
    pub(crate) due_ms: i64,
    pub(crate) count: u64,
    pub(crate) at_least: bool,
}

/// The due times after `lower_ms` (settled: run, missed, or before the
/// schedule was made or switched on) up to `now_ms`. Only the latest can
/// run, and only within the grace; the others are missed. Nothing is run
/// late.
pub(crate) fn due(
    spec: &Spec,
    origin_ms: i64,
    lower_ms: i64,
    now_ms: i64,
    local: &dyn Fn(i64) -> Local,
) -> Due {
    let mut result = Due::default();
    let mut at = lower_ms;
    let mut count = 0u64;
    let mut at_least = false;
    let (mut latest, mut previous) = (None, None);
    while let Some(next) = spec.next_after(at, origin_ms, local) {
        if next > now_ms {
            result.next = Some(next);
            break;
        }
        previous = latest;
        latest = Some(next);
        count += 1;
        at = next;
        if count == COUNT_LIMIT && !at_least {
            at_least = true;
            at = at.max(now_ms - COUNT_SPAN_MS);
        }
    }
    let Some(latest) = latest else {
        return result;
    };
    if now_ms - latest <= GRACE_MS {
        result.run = Some(latest);
        result.missed = previous.map(|due_ms| Missed {
            due_ms,
            count: count - 1,
            at_least,
        });
    } else {
        result.missed = Some(Missed {
            due_ms: latest,
            count,
            at_least,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(ms: i64) -> Local {
        let minute_of_day = ms.rem_euclid(86_400_000) / 60_000;
        Local {
            year: 2026,
            month: 1,
            day: 1 + ms.div_euclid(86_400_000).rem_euclid(28) as u32,
            hour: (minute_of_day / 60) as u32,
            minute: (minute_of_day % 60) as u32,
            weekday: 4,
            offset: 0,
        }
    }

    const MINUTE: i64 = 60_000;

    #[test]
    fn only_the_latest_time_runs_and_only_on_time() {
        let every = Spec::Every { seconds: 600 };
        // Nothing due yet.
        assert_eq!(
            due(&every, 0, 0, 5 * MINUTE, &utc),
            Due {
                run: None,
                missed: None,
                next: Some(10 * MINUTE)
            }
        );
        // On time, with a little delay.
        let on_time = due(&every, 0, 0, 10 * MINUTE + 30_000, &utc);
        assert_eq!((on_time.run, on_time.missed), (Some(10 * MINUTE), None));
        // Asleep for an hour: five missed, the latest runs if on time.
        let woke = due(&every, 0, 0, 60 * MINUTE + 1_000, &utc);
        assert_eq!(woke.run, Some(60 * MINUTE));
        assert_eq!(
            woke.missed,
            Some(Missed {
                due_ms: 50 * MINUTE,
                count: 5,
                at_least: false
            })
        );
        assert_eq!(woke.next, Some(70 * MINUTE));
        // Woken past the grace: all six missed, none run.
        let late = due(&every, 0, 0, 61 * MINUTE + 1, &utc);
        assert_eq!(late.run, None);
        assert_eq!(
            late.missed,
            Some(Missed {
                due_ms: 60 * MINUTE,
                count: 6,
                at_least: false
            })
        );
        // Settled times are not counted again.
        let settled = due(&every, 0, 60 * MINUTE, 61 * MINUTE + 1, &utc);
        assert_eq!((settled.run, settled.missed), (None, None));
    }

    #[test]
    fn a_long_gap_costs_a_bounded_count() {
        let minutely = Spec::cron("* * * * *").unwrap();
        let month = 30 * 24 * 60 * MINUTE;
        let woke = due(&minutely, 0, 0, month + 10_000, &utc);
        assert_eq!(woke.run, Some(month));
        let missed = woke.missed.unwrap();
        assert!(missed.at_least);
        assert_eq!(missed.due_ms, month - MINUTE);
        assert!(missed.count < 8_000);
    }
}
