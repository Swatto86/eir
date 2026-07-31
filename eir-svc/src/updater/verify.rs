//! Post-update verification: confirm an app now reports the version we expected to
//! install. Reads the installed version from winget (by id or by name) and maps it
//! to a [`Verification`] verdict via [`super::version::classify_version`]. The
//! comparison itself is pure and unit-tested in `version`; this module is the I/O.

use super::domain::Verification;
use super::methods::winget;
use super::names::{app_id, match_installed_entry};
use super::proc::VERIFY;
use super::version::{classify_version, version_cmp};
use super::winget_parse::{column, winget_table};
use std::time::Duration;

/// What to read the installed version from.
pub enum VerifyTarget {
    Winget { id: String },
    ByName { name: String },
}

/// A verdict worth acting on immediately. A first-read `Mismatch` is re-read once:
/// right after a successful install the old version can still be the registered one
/// for a moment, and reporting that as a failed update is worse than a 2s wait. A
/// genuinely failed install still reads `Mismatch` the second time. Pure.
fn settled(verdict: Verification) -> bool {
    verdict != Verification::Mismatch
}

/// Confirm an app now reports `expected` (or newer). Returns (verdict, found
/// version). One short retry absorbs the ARP-registration lag right after a fresh
/// install — whether that shows up as no readable version at all or as the previous
/// version still being registered.
pub async fn verify_app(target: &VerifyTarget, expected: &str) -> (Verification, String) {
    let mut last = (Verification::Unverified, String::new());
    for attempt in 0..2 {
        if attempt == 1 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let found = match target {
            VerifyTarget::Winget { id } => winget_installed_version(id).await,
            VerifyTarget::ByName { name } => winget_installed_version_by_name(name).await,
        };
        if let Some(found) = found {
            let verdict = classify_version(&found, expected);
            if settled(verdict) {
                return (verdict, found);
            }
            last = (verdict, found);
        }
    }
    last
}

/// Read an installed app's version from `winget list --id <id> --exact`.
async fn winget_installed_version(id: &str) -> Option<String> {
    let (_code, text) = winget::run_winget(
        vec![
            "list".to_string(),
            "--id".to_string(),
            id.to_string(),
            "--exact".to_string(),
            "--accept-source-agreements".to_string(),
            "--disable-interactivity".to_string(),
        ],
        VERIFY,
    )
    .await;
    let (offsets, rows) = winget_table(&text);
    rows.first()
        .map(|r| column(&offsets, r, "Version"))
        .filter(|v| !v.is_empty())
}

/// Read an installed app's version from `winget list --name <name>`, matching the
/// row whose Name overlaps the queried name (display names are fuzzy).
async fn winget_installed_version_by_name(name: &str) -> Option<String> {
    let (_code, text) = winget::run_winget(
        vec![
            "list".to_string(),
            "--name".to_string(),
            name.to_string(),
            "--accept-source-agreements".to_string(),
            "--disable-interactivity".to_string(),
        ],
        VERIFY,
    )
    .await;
    installed_version_by_name(&text, name)
}

fn installed_version_by_name(text: &str, name: &str) -> Option<String> {
    let (offsets, rows) = winget_table(text);
    let mut installed = std::collections::HashMap::new();
    let mut ambiguous = std::collections::HashSet::new();
    for r in &rows {
        let n = column(&offsets, r, "Name");
        if n.is_empty() {
            continue;
        }
        let v = column(&offsets, r, "Version");
        if v.is_empty() {
            continue;
        }
        let key = app_id(&n);
        if ambiguous.contains(&key) {
            continue;
        }
        match installed.entry(key.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(v);
            }
            std::collections::hash_map::Entry::Occupied(entry) => {
                let same = v.eq_ignore_ascii_case(entry.get())
                    || version_cmp(&v, entry.get()) == Some(std::cmp::Ordering::Equal);
                if !same {
                    entry.remove();
                    ambiguous.insert(key);
                }
            }
        }
    }
    match_installed_entry(&installed, name).map(|(_, version)| version.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_mismatch_is_re_read() {
        // A just-installed app can still report the old version for a moment, so that
        // one verdict gets the second read; everything else settles on the first.
        assert!(!settled(Verification::Mismatch));
        assert!(settled(Verification::Verified));
        assert!(settled(Verification::Unverified));
        assert!(settled(Verification::NotChecked));
    }

    #[test]
    fn by_name_verification_does_not_match_a_substring_namesake() {
        let table = format!(
            "{:<16}{:<18}{:<10}{}\n{}\n{:<16}{:<18}{:<10}{}",
            "Name",
            "Id",
            "Version",
            "Source",
            "-".repeat(50),
            "GitHub Desktop",
            "GitHub.Desktop",
            "3.4.0",
            "winget"
        );
        assert_eq!(installed_version_by_name(&table, "Git"), None);
        assert_eq!(
            installed_version_by_name(&table, "GitHub Desktop").as_deref(),
            Some("3.4.0")
        );
    }

    #[test]
    fn by_name_verification_rejects_differing_duplicate_registrations() {
        let table = format!(
            "{:<16}{:<18}{:<10}{}\n{}\n{:<16}{:<18}{:<10}{}\n{:<16}{:<18}{:<10}{}",
            "Name",
            "Id",
            "Version",
            "Source",
            "-".repeat(50),
            "Example Tool",
            "Example.Tool",
            "1.0.0",
            "winget",
            "Example Tool",
            "Example.Tool",
            "2.0.0",
            "winget"
        );
        assert_eq!(installed_version_by_name(&table, "Example Tool"), None);
    }
}
