//! Version-string comparison and the post-update verification verdict. Ported
//! verbatim from the original UI engine (its tests came with it): the tricky bits
//! are the dotted-numeric comparison, the "marketing truncation" guard against
//! cross-product false positives, and the strictly-newer test the update check
//! relies on. Pure — no I/O.

use super::domain::Verification;

pub fn normalize_version(v: &str) -> String {
    v.trim().trim_start_matches(['v', 'V']).trim().to_string()
}

/// Parse a version's leading dotted-numeric run into its components, e.g.
/// "1.7.12227.37421622" -> [1, 7, 12227, 37421622]. The run stops at the first
/// non-numeric label, so "2.43.0.windows.1" -> [2, 43, 0]. Returns None when there
/// is no parseable dotted-numeric head (or a component overflows u64) — the signal
/// callers use to fall back to a string comparison.
pub fn numeric_components(s: &str) -> Option<Vec<u64>> {
    let s = normalize_version(s);
    let head: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let parts: Vec<u64> = head
        .split('.')
        .filter(|p| !p.is_empty())
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

/// Compare two version strings numerically, component by component. Returns None
/// when neither side begins with a parseable dotted-numeric version.
pub fn version_cmp(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let (a, b) = (normalize_version(a), normalize_version(b));
    // Real SemVer first, so a prerelease ranks BELOW its stable release instead of
    // comparing equal (the numeric head alone drops the "-beta"). Windows/vendor
    // schemes ("2.43.0.windows.1", "1.7.12227.37421622") aren't valid SemVer and
    // fall through to the dotted-numeric comparison unchanged.
    if let (Ok(x), Ok(y)) = (semver::Version::parse(&a), semver::Version::parse(&b)) {
        return Some(x.cmp(&y));
    }
    let (mut x, mut y) = (numeric_components(&a)?, numeric_components(&b)?);
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    Some(x.cmp(&y))
}

/// Detect a "marketing truncation": a numeric `latest` that only looks newer than
/// `current` because it is a SHORT marketing version held up against `current`'s
/// LONGER build/revision version that shares the same major. This is the signature
/// of cross-product / cross-scheme conflation — e.g. an installed "NVIDIA FrameView
/// SDK 1.7.12227.37421622" reported as upgradable to the FrameView *app*'s
/// marketing "1.9": `1.9` wins only on the minor while ignoring the 12227.37421622
/// of real build precision the proposal omits entirely. Comparing the two is
/// meaningless, so such a "newer" verdict must not be trusted.
///
/// Conservative by construction — it fires ONLY when every part of that shape
/// holds: both sides parse numerically; `current` is at least two components
/// longer than `latest` (a build/revision tail `latest` lacks); the majors agree
/// (the win lands on a later component, never the leading one); and `current`
/// carries genuine non-zero precision beyond `latest`'s length. It deliberately
/// does NOT touch bare-major bumps such as a driver "551.86.0.0" -> "552", because
/// those are structurally identical to legitimate major upgrades like Chrome
/// "85.0.4183.121" -> "86" and cannot be told apart from the version strings
/// alone; the recourse for that residual case is the per-app Ignore list.
pub fn is_marketing_truncation(latest: &str, current: &str) -> bool {
    let (Some(l), Some(c)) = (numeric_components(latest), numeric_components(current)) else {
        return false;
    };
    // `current` must be materially longer — a build/revision tail `latest` omits.
    if c.len() < l.len() + 2 {
        return false;
    }
    // First component where they differ (zero-extending `latest`, as version_cmp
    // does). `latest` must win there, on a component it actually has (a real value,
    // not zero-padding) and past the major (index >= 1, so the leading component
    // agrees — this is not an ambiguous bare-major bump).
    let divergence = (0..c.len()).find_map(|i| {
        let li = l.get(i).copied().unwrap_or(0);
        (li != c[i]).then_some((i, li > c[i]))
    });
    let Some((index, latest_wins)) = divergence else {
        return false;
    };
    if !latest_wins || index == 0 {
        return false;
    }
    // `current` must carry real (non-zero) precision beyond `latest`'s length —
    // the build/revision numbers that make the comparison incoherent.
    c[l.len()..].iter().any(|&part| part != 0)
}

/// True only when `latest` is a STRICTLY newer version than `current`. Numeric
/// comparison when both parse as dotted-numeric; otherwise a normalized string
/// inequality, so an identical version is never reported as an update. An empty
/// `latest` is never newer; an empty/unknown `current` lets any real `latest`
/// through (we'd rather show an unverifiable update than silently hide one). A
/// numeric "newer" that is really a marketing truncation of a longer build version
/// (see is_marketing_truncation) is rejected — that is the cross-scheme false
/// positive that surfaced an unkillable "FrameView SDK 1.7.x -> 1.9" row.
pub fn is_newer(latest: &str, current: &str) -> bool {
    if latest.trim().is_empty() {
        return false;
    }
    if is_marketing_truncation(latest, current) {
        return false;
    }
    match version_cmp(latest, current) {
        Some(std::cmp::Ordering::Greater) => true,
        Some(_) => false,
        None => normalize_version(latest) != normalize_version(current),
    }
}

/// Map a found-vs-expected version comparison to a verification verdict.
pub fn classify_version(found: &str, expected: &str) -> Verification {
    if found.trim().is_empty() || found.eq_ignore_ascii_case("unknown") {
        return Verification::Unverified;
    }
    match version_cmp(found, expected) {
        // Still older than what we expected to install => the update didn't take.
        Some(std::cmp::Ordering::Less) => Verification::Mismatch,
        // Equal or newer (vendor may have shipped an even newer build) => success.
        Some(_) => Verification::Verified,
        None => {
            if normalize_version(found) == normalize_version(expected) {
                Verification::Verified
            } else {
                Verification::Unverified
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare_and_classify() {
        assert_eq!(
            version_cmp("2.0.0", "1.9.9"),
            Some(std::cmp::Ordering::Greater)
        );
        assert_eq!(version_cmp("1.0", "1.0.0"), Some(std::cmp::Ordering::Equal));
        assert_eq!(
            version_cmp("v2.1", "v2.0"),
            Some(std::cmp::Ordering::Greater)
        );
        assert_eq!(classify_version("2.0.0", "2.0.0"), Verification::Verified);
        assert_eq!(classify_version("2.1.0", "2.0.0"), Verification::Verified); // newer than expected is fine
        assert_eq!(classify_version("1.0.0", "2.0.0"), Verification::Mismatch); // still the old version
        assert_eq!(
            classify_version("Unknown", "2.0.0"),
            Verification::Unverified
        );
        // A prerelease is NOT the stable release: verifying an attempted 2.0.0 against
        // an installed 2.0.0-beta must be a mismatch, and the stable must still be
        // offered as an update over it.
        assert_eq!(
            version_cmp("2.0.0-beta", "2.0.0"),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(
            classify_version("2.0.0-beta", "2.0.0"),
            Verification::Mismatch
        );
        assert!(is_newer("2.0.0", "2.0.0-beta"));
    }

    #[test]
    fn is_newer_only_accepts_strictly_higher() {
        // The Revision Tool regression: an AI "2.7.5" must NOT count as an update
        // over an installed "2.9.0", nor over an equal version.
        assert!(!is_newer("2.7.5", "2.9.0"));
        assert!(!is_newer("2.9.0", "2.9.0"));
        assert!(!is_newer("2.7.1", "2.7.5"));
        // A genuinely newer release is reported; `v`-prefixes are normalized.
        assert!(is_newer("2.7.5", "2.7.1"));
        assert!(is_newer("v2.9.1", "2.9.0"));
        // Empty latest is never an update; unknown current lets a real latest pass.
        assert!(!is_newer("", "2.9.0"));
        assert!(is_newer("2.9.0", ""));
        // Dotted dates still compare numerically (newer minor wins).
        assert!(is_newer("2024.02", "2024.01"));
        // Truly non-numeric tags fall back to a string inequality: equal is never
        // an update, differing is.
        assert!(!is_newer("nightly", "nightly"));
        assert!(is_newer("build-b", "build-a"));
    }

    #[test]
    fn numeric_components_parses_leading_dotted_run() {
        assert_eq!(
            numeric_components("1.7.12227.37421622"),
            Some(vec![1, 7, 12227, 37421622])
        );
        // The run stops at the first non-numeric label (Git-for-Windows style):
        // the ".windows.1" suffix is dropped, leaving the numeric core.
        assert_eq!(
            numeric_components("v2.43.0.windows.1"),
            Some(vec![2, 43, 0])
        );
        assert_eq!(numeric_components("2024.02"), Some(vec![2024, 2]));
        // No parseable dotted-numeric head -> None (drives the string fallback).
        assert_eq!(numeric_components("nightly"), None);
        assert_eq!(numeric_components(""), None);
    }

    #[test]
    fn is_newer_rejects_marketing_truncation() {
        // The motivating bug: an installed 4-part SDK build reported as upgradable
        // to the separate FrameView *app*'s 2-part marketing version. "1.9" beats
        // "1.7.…" only on the minor while ignoring the 12227.37421622 build tail —
        // an incoherent cross-product comparison that must never surface.
        assert!(!is_newer("1.9", "1.7.12227.37421622"));
        // A direct sibling with a much smaller build tail — still rejected, because
        // the guard is structural (no magnitude threshold to slip under).
        assert!(!is_newer("1.9", "1.6.0.4590"));
        // The whole minor-divergence family: a short marketing `latest` vs a long
        // build `current` sharing the major, with the build/revision tail dropped.
        assert!(!is_newer("10.1", "10.0.19041.4046"));
        assert!(!is_newer("12.6", "12.4.131.0"));
        assert!(!is_newer("9.7", "9.5.0.6294"));
        assert!(!is_newer("8.2", "8.1.7600.16385"));
        assert!(!is_newer("3.5", "3.4.2.10481"));
        assert!(!is_newer("6.1", "6.0.2312.40001"));
        assert!(!is_newer("1.16", "1.14.6.30146"));
        assert!(!is_newer("2.2", "2.1.0.20240115"));
        assert!(!is_newer("1.3", "1.2.11.8"));
        assert!(!is_newer("3.12", "3.11.2150.1013"));
        assert!(!is_newer("10.20", "10.2.3.40066"));
    }

    #[test]
    fn is_newer_keeps_real_updates_the_guard_must_not_eat() {
        // Same-length big-build upgrade of the SAME family — the case that proves
        // the guard keys on the MISMATCH between the two versions, not on "current
        // has big numbers". Both are 4-part builds; 1.7.x < 1.8.x is a real update.
        assert!(is_newer("1.8.13000.41000000", "1.7.12227.37421622"));
        assert!(is_newer("1.0.22631.40000000", "1.0.22621.39000000"));
        // `latest` LONGER than `current` (adds build detail) — current's missing
        // tail is an implicit .0, so the comparison is sound. Must be kept.
        assert!(is_newer("1.0.7600.16385", "1.0"));
        assert!(is_newer("85.0.4183.83", "85"));
        assert!(is_newer("1.0.1", "1.0.0.0"));
        // Component count shrinks by one, but it is a genuine point/minor release
        // (the omitted tail is too short to be a build/revision number).
        assert!(is_newer("1.3", "1.2.3"));
        assert!(is_newer("116.0", "115.0.1"));
        // A short `current` whose omitted tail is all zeros is just padding, not
        // build precision — "1.7.0.0" -> "1.9" is a real minor bump.
        assert!(is_newer("1.9", "1.7.0.0"));
        // Bare-major bump (4-part build -> next major) — structurally identical to
        // a driver cross-product case, so the guard leaves it ALONE; it stays the
        // (correct, common) real update, e.g. Chrome 85 -> 86.
        assert!(is_newer("86", "85.0.4183.121"));
        // Ordinary same-scheme updates are unaffected.
        assert!(is_newer("1.3.0", "1.2.3"));
        assert!(is_newer("119.0.6045.105", "118.0.5993.88"));
        assert!(is_newer("1.10.0", "1.9.0"));
    }

    #[test]
    fn marketing_truncation_predicate_is_conservative() {
        // Fires on the FrameView shape.
        assert!(is_marketing_truncation("1.9", "1.7.12227.37421622"));
        // Does NOT fire when the majors differ — an ambiguous bare-major bump
        // (driver "551.86.0.0" -> "552") indistinguishable from a real one.
        assert!(!is_marketing_truncation("552", "551.86.0.0"));
        assert!(!is_marketing_truncation("23", "22.11.0.62007"));
        // Does NOT fire when current's omitted tail is all zeros (1.7.0.0 ~= 1.7).
        assert!(!is_marketing_truncation("1.9", "1.7.0.0"));
        // Does NOT fire when latest is longer, equal-length, or only one longer.
        assert!(!is_marketing_truncation("1.0.7600.16385", "1.0"));
        assert!(!is_marketing_truncation(
            "1.8.13000.41000000",
            "1.7.12227.37421622"
        ));
        assert!(!is_marketing_truncation("1.3", "1.2.3"));
        // Non-numeric input never qualifies.
        assert!(!is_marketing_truncation("nightly", "1.2.3.4"));
    }
}
