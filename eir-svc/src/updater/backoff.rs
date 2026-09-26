//! Pausing apps whose updates keep failing. The same failure — no silent installer, an
//! unsigned download, a vendor web page instead of an installer — repeated in every
//! daily cycle and cost an AI web search each time. After two failed update runs in a
//! row an app waits 2 days before Eir tries it again on its own, after three 4 days,
//! then a week. Retry on the app's row always tries at once, and one success (or
//! finding the app already current) ends the pause. Pure: the records come from
//! `update_attempts`.

use std::collections::HashMap;

const DAY: i64 = 86_400;

/// One recorded attempt, as the pause rule needs it.
#[derive(Debug, Clone)]
pub struct AttemptRecord {
    pub app_id: String,
    pub cycle_id: i64,
    /// The app was updated, or found already current.
    pub settled: bool,
    pub method: String,
    pub detail: String,
    /// Unix seconds.
    pub at: i64,
}

/// An app Eir will not try again on its own until `until` (unix seconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pause {
    pub failed_runs: usize,
    pub until: i64,
    pub last_method: String,
    pub last_detail: String,
}

impl Pause {
    /// What the app's row says while it is paused.
    pub fn describe(&self) -> String {
        let when = chrono::DateTime::from_timestamp(self.until, 0)
            .map(|at| {
                at.with_timezone(&chrono::Local)
                    .format("%a %-d %b")
                    .to_string()
            })
            .unwrap_or_else(|| "later".to_string());
        format!(
            "Failed in {} update runs in a row, so Eir next tries it on its own on {when}; \
Retry tries now. Last result: {}",
            self.failed_runs, self.last_detail
        )
    }
}

fn pause_days(failed_runs: usize) -> Option<i64> {
    match failed_runs {
        0 | 1 => None,
        2 => Some(2),
        3 => Some(4),
        _ => Some(7),
    }
}

/// The pauses in force at `now`, by app id. `records` are in insertion order.
pub fn paused_apps(records: &[AttemptRecord], now: i64) -> HashMap<String, Pause> {
    // app -> cycle -> (settled in that run, its last attempt)
    let mut runs: HashMap<&str, HashMap<i64, (bool, &AttemptRecord)>> = HashMap::new();
    for record in records {
        let run = runs
            .entry(record.app_id.as_str())
            .or_default()
            .entry(record.cycle_id)
            .or_insert((false, record));
        run.0 |= record.settled;
        if record.at >= run.1.at {
            run.1 = record;
        }
    }
    let mut pauses = HashMap::new();
    for (app, cycles) in runs {
        let mut cycles: Vec<(i64, (bool, &AttemptRecord))> = cycles.into_iter().collect();
        cycles.sort_by_key(|(cycle_id, _)| std::cmp::Reverse(*cycle_id));
        let failed_runs = cycles
            .iter()
            .take_while(|(_, (settled, _))| !settled)
            .count();
        let (Some(days), Some((_, (_, last)))) = (pause_days(failed_runs), cycles.first()) else {
            continue;
        };
        let until = last.at + days * DAY;
        if now < until {
            pauses.insert(
                app.to_string(),
                Pause {
                    failed_runs,
                    until,
                    last_method: last.method.clone(),
                    last_detail: last.detail.clone(),
                },
            );
        }
    }
    pauses
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(app: &str, cycle_day: i64, settled: bool, detail: &str) -> AttemptRecord {
        AttemptRecord {
            app_id: app.to_string(),
            cycle_id: cycle_day * DAY,
            settled,
            method: "native".to_string(),
            detail: detail.to_string(),
            at: cycle_day * DAY + 600,
        }
    }

    #[test]
    fn one_failure_is_not_paused_but_two_in_a_row_are() {
        let records = vec![attempt("lua", 10, false, "no direct installer URL")];
        assert!(paused_apps(&records, 10 * DAY + 700).is_empty());

        let records = vec![
            attempt("lua", 10, false, "no direct installer URL"),
            attempt(
                "lua",
                11,
                false,
                "no direct installer URL — manual download: x",
            ),
        ];
        let pauses = paused_apps(&records, 11 * DAY + 700);
        let pause = &pauses["lua"];
        assert_eq!(pause.failed_runs, 2);
        assert_eq!(pause.until, 13 * DAY + 600, "two failed runs wait two days");
        assert_eq!(
            pause.last_detail,
            "no direct installer URL — manual download: x"
        );
        assert!(
            paused_apps(&records, 13 * DAY + 600).is_empty(),
            "the pause ends on time"
        );
    }

    #[test]
    fn longer_streaks_wait_longer_up_to_a_week() {
        let days = |runs: i64| {
            let records: Vec<_> = (1..=runs)
                .map(|day| attempt("go", day, false, "x"))
                .collect();
            let pause = paused_apps(&records, runs * DAY + 700)
                .remove("go")
                .expect("paused");
            (pause.until - (runs * DAY + 600)) / DAY
        };
        assert_eq!(days(3), 4);
        assert_eq!(days(4), 7);
        assert_eq!(days(12), 7);
    }

    #[test]
    fn a_success_or_already_current_ends_the_streak() {
        let records = vec![
            attempt("chrome", 1, false, "timed out"),
            attempt("chrome", 2, false, "timed out"),
            attempt("chrome", 3, true, "Successfully installed"),
            attempt("chrome", 4, false, "timed out"),
        ];
        assert!(
            paused_apps(&records, 4 * DAY + 700).is_empty(),
            "only the failures after the last success count"
        );
    }

    #[test]
    fn a_run_that_failed_over_to_a_working_method_counts_as_settled() {
        let mut records = vec![
            attempt("vlc", 1, false, "x"),
            attempt("vlc", 2, false, "winget failed"),
        ];
        records.push(attempt("vlc", 2, true, "choco upgraded vlc"));
        assert!(paused_apps(&records, 2 * DAY + 700).is_empty());
    }

    #[test]
    fn the_last_attempt_of_the_newest_run_is_reported() {
        let mut first = attempt("battle.net", 5, false, "winget: version unknown");
        first.method = "winget".to_string();
        let mut last = attempt("battle.net", 5, false, "no direct installer URL");
        last.method = "native".to_string();
        let records = vec![attempt("battle.net", 4, false, "older"), first, last];
        let pause = paused_apps(&records, 5 * DAY + 700)
            .remove("battle.net")
            .expect("paused");
        assert_eq!(pause.last_method, "native");
        assert_eq!(pause.last_detail, "no direct installer URL");
        assert!(pause
            .describe()
            .starts_with("Failed in 2 update runs in a row"));
        assert!(pause
            .describe()
            .ends_with("Last result: no direct installer URL"));
    }
}
