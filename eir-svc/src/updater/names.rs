//! Stable app-identity helpers: strip a version baked into a winget DisplayName so
//! the note/ignore key (and the name shown to the AI) stays constant across
//! releases, and resolve a fuzzy AI-returned name back to an installed version.
//! Ported verbatim with its tests. Pure.

use std::collections::HashMap;

/// True when a token is a version designator we can safely strip: a dotted-numeric
/// core (>= 2 dot-separated numeric groups, optionally `v`-prefixed or parenthesised)
/// with an optional trailing build/pre-release qualifier — so "2.9.0", "v2.9",
/// "(1.2.3)", "5.6.0-beta.1", "2.43.0.windows.1" all match. It MUST start with a
/// digit (after an optional `v`/`(`) so product tokens that merely contain digits or
/// dots — "Node.js", "7-Zip", "paint.net" — never match, and a dot is REQUIRED so a
/// bare integer/year ("3", "2021") is never a version (keeps "Office 2021" distinct).
pub fn is_version_token(token: &str) -> bool {
    let t = token.trim().trim_start_matches('(').trim_end_matches(')');
    let t = t.strip_prefix(['v', 'V']).unwrap_or(t);
    // Split into a leading numeric-dotted core and any remainder.
    let core_len = t
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .count();
    let mut core: String = t.chars().take(core_len).collect();
    let mut rest: String = t.chars().skip(core_len).collect();
    // A trailing dot belongs to the qualifier, not the core ("2.43.0" + ".windows.1").
    while core.ends_with('.') {
        core.pop();
        rest.insert(0, '.');
    }
    if !core.starts_with(|c: char| c.is_ascii_digit()) || !core.contains('.') {
        return false;
    }
    // Either nothing after the core, or one qualifier that starts with a separator
    // and is alphanumerics/dots/hyphens/plus only (covers -rc1, .windows.1, +build).
    rest.is_empty()
        || (rest.starts_with(['-', '+', '.'])
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')))
}

/// A trailing token that qualifies a build but carries no product identity — CPU
/// arch / bitness — so it can be dropped to expose (or sit alongside) a version.
/// A closed set: nothing here is ever part of a real product name.
pub fn is_arch_qualifier(token: &str) -> bool {
    let t = token
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .to_ascii_lowercase();
    matches!(
        t.as_str(),
        "x64"
            | "x86"
            | "amd64"
            | "arm64"
            | "win64"
            | "win32"
            | "32-bit"
            | "64-bit"
            | "32bit"
            | "64bit"
    )
}

/// A lone connector glyph (dash family, pipe, colon, middot) left dangling once a
/// trailing version is removed — e.g. "draw.io — 24.7.17" -> "draw.io —".
pub fn is_connector(token: &str) -> bool {
    matches!(token, "-" | "–" | "—" | "‒" | "−" | "|" | ":" | "·")
}

/// Strip a trailing version designator (and its build qualifiers) from a winget
/// display name so an app whose DisplayName bakes in its version — Inno Setup's
/// default "App version X.Y.Z", a bare "App X.Y.Z", "App 1.2.3 (x64)", "App — 1.0" —
/// keeps a STABLE identity across releases. That identity is the note/ignore key
/// (and the name handed to the AI), so without this a note set on one version is
/// silently orphaned by the next.
///
/// Conservative by construction — every removable trailing token is a version, a
/// closed-set arch/bitness qualifier, a connector glyph, or a keyword-gated
/// build/update counter ("Update 421", "Build 4169"). It never strips a bare
/// integer/year without such a keyword, and never empties the name (a string that
/// is nothing but a version is returned unchanged). Best-effort: same-base products
/// that differ only by major (e.g. Python 3.11 vs 3.12, Java 8 vs 17 without an
/// "Update" counter) intentionally fold to one key — acceptable here because the
/// major runtimes (.NET, VC++, SDKs) are already dropped upstream as noise.
pub fn clean_app_name(name: &str) -> String {
    let mut words: Vec<&str> = name.split_whitespace().collect();
    let mut removed_version = false;
    while let Some(&last) = words.last() {
        if is_version_token(last) {
            words.pop();
            removed_version = true;
            continue;
        }
        if is_arch_qualifier(last) || is_connector(last) {
            words.pop();
            continue;
        }
        // "<keyword> <integer>" release counter, e.g. "Update 421" / "Build 4169".
        // Gated on the keyword so a bare trailing integer that is product identity
        // ("Windows 11", "Python 3") is never stripped.
        if !last.is_empty()
            && last.chars().all(|c| c.is_ascii_digit())
            && words.len() >= 2
            && matches!(
                words[words.len() - 2].to_ascii_lowercase().as_str(),
                "update" | "build" | "rev" | "revision" | "patch"
            )
        {
            words.pop(); // the integer
            words.pop(); // the keyword
            removed_version = true;
            continue;
        }
        // A "version"/"ver" lead-in left dangling once its version was removed.
        if removed_version && matches!(last.to_ascii_lowercase().as_str(), "version" | "ver") {
            words.pop();
            continue;
        }
        break;
    }
    // Trim any connector that ended up trailing after the version came off.
    while words.last().is_some_and(|w| is_connector(w)) {
        words.pop();
    }
    if words.is_empty() {
        name.trim().to_string()
    } else {
        words.join(" ")
    }
}

/// Lowercased alphanumeric tokens of a name, e.g. "Revision Tool v2" -> [revision, tool, v2].
fn name_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

/// Whether `small` appears as a contiguous whole-token run inside `big`.
fn contiguous_sublist(small: &[String], big: &[String]) -> bool {
    if small.is_empty() || small.len() > big.len() {
        return false;
    }
    big.windows(small.len()).any(|w| w == small)
}

/// Resolve the installed version of an AI-returned app name against the apps we
/// actually queried (keyed lowercased name -> version). Display names are fuzzy —
/// the model may echo "Revision Tool" for a winget "Revision Tool version 2.9.0" —
/// so after an exact hit fails, fall back to a WHOLE-TOKEN containment match (not a
/// raw substring). A raw `contains` let a short AI name like "note" resolve to an
/// unrelated "Notepad++", which — since the resolved candidate's name feeds the
/// download host gate — could anchor a malicious vendor domain to a real installed
/// app. Whole-token matching closes that, and the most specific (longest) match wins
/// so a short query can't grab the wrong longer app.
pub fn match_installed<'a>(
    installed: &'a HashMap<String, String>,
    name: &str,
) -> Option<&'a String> {
    let n = name.to_lowercase();
    if let Some(v) = installed.get(&n) {
        return Some(v);
    }
    let nt = name_tokens(name);
    if nt.is_empty() {
        return None;
    }
    installed
        .iter()
        .filter(|(k, _)| {
            let kt = name_tokens(k);
            !kt.is_empty() && (contiguous_sublist(&nt, &kt) || contiguous_sublist(&kt, &nt))
        })
        .max_by_key(|(k, _)| k.len())
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_installed_uses_whole_token_matching() {
        let mut m = HashMap::new();
        m.insert("notepad++".to_string(), "8.6".to_string());
        m.insert(
            "revision tool version 2.9.0".to_string(),
            "2.9.0".to_string(),
        );
        m.insert("brave".to_string(), "1.60".to_string());
        // The attack: a short AI name must NOT substring-resolve to an unrelated app.
        assert_eq!(match_installed(&m, "note"), None);
        // Legit fuzzy display-name match still works (whole-token subset).
        assert_eq!(
            match_installed(&m, "Revision Tool").map(String::as_str),
            Some("2.9.0")
        );
        // Exact match and a genuinely-unrelated name.
        assert_eq!(
            match_installed(&m, "Brave").map(String::as_str),
            Some("1.60")
        );
        assert_eq!(match_installed(&m, "Firefox"), None);
    }

    #[test]
    fn clean_app_name_strips_trailing_version() {
        // The Inno Setup default that started this — version baked into the name,
        // and the SAME stable identity must fall out of any release.
        assert_eq!(
            clean_app_name("Revision Tool version 2.9.0"),
            "Revision Tool"
        );
        assert_eq!(
            clean_app_name("Revision Tool version 2.7.1"),
            "Revision Tool"
        );
        // Bare and v-prefixed trailing versions.
        assert_eq!(clean_app_name("Krita 5.2.0"), "Krita");
        assert_eq!(clean_app_name("Obsidian v1.5.3"), "Obsidian");
        assert_eq!(
            clean_app_name("K-Lite Codec Pack 17.8.5"),
            "K-Lite Codec Pack"
        );
        // No trailing version -> unchanged (incl. a dot inside a real name).
        assert_eq!(clean_app_name("Discord"), "Discord");
        assert_eq!(clean_app_name("Acrobat Reader DC"), "Acrobat Reader DC");
        assert_eq!(clean_app_name("Battle.net"), "Battle.net");
        assert_eq!(clean_app_name("Node.js"), "Node.js");
        // Bare integers / years are NOT versions — distinct products stay distinct.
        assert_eq!(clean_app_name("Office 2021"), "Office 2021");
        assert_eq!(clean_app_name("Python 3"), "Python 3");
        // Never empty the key: a name that is only a version is left intact.
        assert_eq!(clean_app_name("2.9.0"), "2.9.0");
    }

    #[test]
    fn clean_app_name_handles_arch_separators_and_counters() {
        // A trailing arch/bitness qualifier must not shield the version (the most
        // common real installer shape).
        assert_eq!(clean_app_name("7-Zip 23.01 (x64)"), "7-Zip");
        assert_eq!(clean_app_name("OBS Studio 30.0.2 (64-bit)"), "OBS Studio");
        assert_eq!(
            clean_app_name("VLC media player 3.0.20 x64"),
            "VLC media player"
        );
        // Arch alone (no version) still folds 32/64-bit builds of one product.
        assert_eq!(clean_app_name("Far Manager x64"), "Far Manager");
        // Dangling connector glyphs are trimmed, not baked into the key.
        assert_eq!(clean_app_name("draw.io — 24.7.17"), "draw.io");
        assert_eq!(clean_app_name("WinRAR – 7.01"), "WinRAR");
        // Build/pre-release and vendor-suffixed version tokens.
        assert_eq!(clean_app_name("Cura 5.6.0-beta.1"), "Cura");
        assert_eq!(clean_app_name("OBS Studio 30.0.2-rc1"), "OBS Studio");
        assert_eq!(clean_app_name("Git version 2.43.0.windows.1"), "Git");
        // Keyword-gated dotless counters; the major before them is identity, kept.
        assert_eq!(clean_app_name("Sublime Text Build 4169"), "Sublime Text");
        assert_eq!(clean_app_name("Java 8 Update 421"), "Java 8");
        // The gate holds: a bare trailing integer with no keyword is product identity.
        assert_eq!(clean_app_name("Windows Terminal"), "Windows Terminal");
        assert_eq!(clean_app_name("Process Lasso 9"), "Process Lasso 9");
    }

    #[test]
    fn clean_app_name_accepts_documented_multimajor_merge() {
        // KNOWN, accepted limitation: same-base products differing only by a dotted
        // major collapse to one key. Tolerable because the heavyweight runtimes
        // (.NET, VC++, SDKs) are filtered as noise upstream before they get here.
        assert_eq!(clean_app_name("Python 3.12.4"), "Python");
        assert_eq!(clean_app_name("Python 3.11.9"), "Python");
    }

    #[test]
    fn match_installed_resolves_fuzzy_names() {
        let mut installed = HashMap::new();
        installed.insert(
            "revision tool version 2.9.0".to_string(),
            "2.9.0".to_string(),
        );
        installed.insert("krita".to_string(), "5.2.0".to_string());
        // The AI echoes a clean "Revision Tool"; it must still resolve to 2.9.0.
        assert_eq!(
            match_installed(&installed, "Revision Tool"),
            Some(&"2.9.0".to_string())
        );
        // Exact (case-insensitive) hit.
        assert_eq!(
            match_installed(&installed, "Krita"),
            Some(&"5.2.0".to_string())
        );
        // No overlap -> no match (caller falls back to the AI's own `current`).
        assert_eq!(match_installed(&installed, "Obsidian"), None);
    }
}
