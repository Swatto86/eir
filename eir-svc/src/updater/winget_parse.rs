//! Parsing of `winget upgrade` / `winget list` output. winget aligns columns to
//! fixed header positions and separates them with a *single* space (wide gaps are
//! padding), truncating long fields with '…' — both defeat a whitespace splitter,
//! so we parse by column offset instead. Also: which `winget list` rows are apps
//! the AI check should cover (the ones winget can't actually upgrade). Ported
//! verbatim with its tests. Pure.

use super::names::{app_id, clean_app_name};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppUpdate {
    pub id: String,
    pub name: String,
    pub current: String,
    pub available: String,
}

/// The columns a winget table can have, in display order.
const WINGET_COLUMNS: [&str; 5] = ["Name", "Id", "Version", "Available", "Source"];

/// Locate each column's start (char offset) from the table header. winget aligns
/// columns to fixed positions under their header labels, so this is far more
/// robust than splitting on whitespace: winget separates columns with a *single*
/// space (the wide gaps are just padding to the widest cell), and it truncates
/// long fields with '…'. Both break a space-run splitter but leave column
/// positions intact. The header is often prefixed with progress-spinner output
/// terminated by '\r', so only the text after the last '\r' is considered.
fn header_offsets(text: &str) -> Vec<(&'static str, usize)> {
    let header = text
        .lines()
        .map(|l| l.rsplit('\r').next().unwrap_or(l))
        .find(|l| l.contains("Id") && l.contains("Version"));
    let mut offsets = Vec::new();
    if let Some(h) = header {
        for label in WINGET_COLUMNS {
            if let Some(byte) = h.find(label) {
                offsets.push((label, h[..byte].chars().count()));
            }
        }
        offsets.sort_by_key(|&(_, start)| start);
    }
    offsets
}

/// Whether the winget table header (located by the English "Id"/"Version" labels) was
/// found in `text`. Returns false on a localized (non-English) header even when a table
/// is present — used to distinguish the locale blind spot from the benign "header found,
/// rows all filtered out" case (e.g. `--include-unknown` rows with an empty Available).
pub fn header_present(text: &str) -> bool {
    !header_offsets(text).is_empty()
}

/// Read one column's trimmed value from a row. A column spans from its own start
/// to the next column's start (the last runs to end of line). Returns "" when the
/// column is absent or starts past the row's end.
pub fn column(offsets: &[(&'static str, usize)], row: &[char], label: &str) -> String {
    let Some(idx) = offsets.iter().position(|&(l, _)| l == label) else {
        return String::new();
    };
    let start = offsets[idx].1;
    if start >= row.len() {
        return String::new();
    }
    let end = offsets
        .get(idx + 1)
        .map(|&(_, s)| s.min(row.len()))
        .unwrap_or(row.len());
    row[start..end]
        .iter()
        .collect::<String>()
        .trim()
        .to_string()
}

/// Split a winget table into its column offsets and data rows (as char vectors).
/// Skips the progress-noise and header above the dashed separator, and stops at
/// the "N upgrades available" footer (and the "explicit targeting" sub-table some
/// winget versions append).
pub fn winget_table(text: &str) -> (Vec<(&'static str, usize)>, Vec<Vec<char>>) {
    let offsets = header_offsets(text);
    let mut rows = Vec::new();
    let mut in_table = false;
    for raw_line in text.lines() {
        // Strip spinner paint the same way the header does — otherwise a `\r` bleeding
        // onto a data row would misalign it against the header's column offsets.
        let line = raw_line.rsplit('\r').next().unwrap_or(raw_line);
        let trimmed = line.trim();
        if !in_table {
            if trimmed.contains("-----") {
                in_table = true;
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        // The footer is "N upgrades available." — a line that starts with a digit and
        // contains "available". Matching "upgrade" + "available" loosely (as before)
        // truncated the table at any app whose name happened to contain both words.
        let lower = trimmed.to_lowercase();
        let starts_with_digit = trimmed.chars().next().is_some_and(|c| c.is_ascii_digit());
        if (starts_with_digit && lower.contains("available"))
            || lower.starts_with("the following packages")
        {
            break;
        }
        rows.push(line.chars().collect());
    }
    (offsets, rows)
}

/// Parse `winget upgrade` output into the apps with an available update.
pub fn parse_upgrades(text: &str) -> Vec<AppUpdate> {
    let (offsets, rows) = winget_table(text);
    let mut updates = Vec::new();
    for row in &rows {
        let name = column(&offsets, row, "Name");
        // Strip the truncation ellipsis winget adds to long ids; winget's `--id`
        // does substring matching, so the un-truncated prefix still resolves.
        let id = column(&offsets, row, "Id")
            .trim_end_matches('…')
            .trim_end_matches('.')
            .to_string();
        let current = column(&offsets, row, "Version");
        let available = column(&offsets, row, "Available");
        if id.is_empty() || available.is_empty() {
            continue;
        }
        updates.push(AppUpdate {
            id,
            name,
            current,
            available,
        });
    }
    updates
}

/// Names we never AI-check: drivers, runtimes, redistributables, self-updating
/// suites, and Eir itself. Keeps the batch to real, user-updatable apps.
pub fn is_noise(name: &str) -> bool {
    let n = name.to_lowercase();
    const SKIP: &[&str] = &[
        "driver",
        "redistributable",
        "runtime",
        "microsoft visual c++",
        "windows sdk",
        "update for",
        "security update",
        "hotfix",
        "maintenance service",
        "microsoft .net",
        "directx",
        "realtek",
        "intel(r)",
        "host app",
        "web experience",
        "microsoft 365",
        "microsoft office",
        "visual studio installer",
        "onedrive",
        "teams machine-wide",
        "eir",
    ];
    SKIP.iter().any(|s| n.contains(s))
}

/// A winget *catalog* id looks like `Publisher.App` — a dot, no path separators
/// or spaces. winget can genuinely manage (and `winget upgrade` will flag) apps
/// with such ids, so they belong to the winget method, not the AI check. A
/// Microsoft Store product id (e.g. `XPDC2RH70K22MN`) or an ARP id
/// (`ARP\\Machine\\…`) is NOT a catalog id — those are the correlated-standalone /
/// unmanaged apps winget upgrade silently ignores, which is exactly the gap the AI
/// check must cover.
pub fn is_winget_catalog_id(id: &str) -> bool {
    let id = id.trim_end_matches('…');
    id.contains('.') && !id.contains('\\') && !id.contains('/') && !id.contains(' ')
}

/// Parse `winget list` for apps the AI should check for updates — the ones winget
/// upgrade cannot or will not handle.
///
/// We skip only what winget genuinely owns or what updates elsewhere:
///   - `MSIX\` packages and msstore-source rows (true Store apps — update via the Store);
///   - winget-source rows with a real `Publisher.App` catalog id (winget upgrade owns these);
///   - noise (drivers/runtimes/etc.);
///   - anything already flagged by the winget upgrade pass (`already_managed`,
///     case-insensitive by name).
///
/// Everything else — store-correlated standalone apps (Discord) and ARP/unmanaged
/// apps — is kept. Returns (name, version).
pub fn parse_unmanaged(text: &str, already_managed: &HashSet<String>) -> Vec<(String, String)> {
    let (offsets, rows) = winget_table(text);
    let mut apps = Vec::new();
    for row in &rows {
        let id = column(&offsets, row, "Id");
        if id.starts_with("MSIX\\") {
            continue;
        }
        let name = column(&offsets, row, "Name");
        let version = column(&offsets, row, "Version");
        if name.is_empty() || version.is_empty() || is_noise(&name) {
            continue;
        }
        // Source, ellipsis-stripped (winget truncates long cells with '…').
        let source = column(&offsets, row, "Source")
            .trim_end_matches('…')
            .to_lowercase();
        let is_msstore = source.len() >= 5 && "msstore".starts_with(source.as_str());
        let is_winget = source.len() >= 5 && "winget".starts_with(source.as_str());
        // True Store app, or an app winget genuinely manages -> not for the AI check.
        if is_msstore || (is_winget && is_winget_catalog_id(&id)) {
            continue;
        }
        if already_managed.contains(&app_id(&name)) {
            continue;
        }
        // Strip any version baked into the DisplayName so the note/ignore key and
        // the name shown to the AI stay stable across releases. The dedup/noise
        // checks above run on the RAW name (more signal); only the kept name is
        // cleaned. The version column is preserved as winget reported it.
        apps.push((clean_app_name(&name), version));
    }
    apps
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a winget-style fixed-width table from explicit column widths, a
    /// header, and data rows. Mirrors winget exactly: columns are left-aligned and
    /// joined by a single space (so when a field fills its column the gap is one
    /// space — the layout that defeats whitespace splitting), and any field longer
    /// than its column is truncated with a trailing '…'.
    fn render(widths: &[usize], header: &[&str], rows: &[&[&str]]) -> String {
        fn line(widths: &[usize], fields: &[&str]) -> String {
            let cells: Vec<String> = fields
                .iter()
                .enumerate()
                .map(|(c, &f)| {
                    let w = widths[c];
                    if f.chars().count() > w {
                        f.chars().take(w - 1).collect::<String>() + "…"
                    } else {
                        format!("{f:<w$}")
                    }
                })
                .collect();
            cells.join(" ").trim_end().to_string()
        }
        let total: usize = widths.iter().sum::<usize>() + widths.len().saturating_sub(1);
        let mut out = line(widths, header);
        out.push('\n');
        out.push_str(&"-".repeat(total));
        for r in rows {
            out.push('\n');
            out.push_str(&line(widths, r));
        }
        out
    }

    #[test]
    fn header_offsets_ignore_progress_spinner_noise() {
        // winget prefixes the header with carriage-return-overwritten spinner text;
        // only the text after the last '\r' is the real header.
        let noisy = "  -  \r  \\  \rName    Id      Version  Source";
        let clean = "Name    Id      Version  Source";
        assert_eq!(header_offsets(noisy), header_offsets(clean));
        assert_eq!(header_offsets(clean).first(), Some(&("Name", 0)));
    }

    #[test]
    fn parse_upgrades_handles_single_space_columns() {
        // Every field fills its column, so winget separates them with a single
        // space — the narrow layout that previously parsed to zero upgrades.
        let widths = [11, 14, 7, 9, 6];
        let header = ["Name", "Id", "Version", "Available", "Source"];
        let table = render(
            &widths,
            &header,
            &[
                &[
                    "Copilot CLI",
                    "GitHub.Copilot",
                    "v1.0.44",
                    "v1.0.63",
                    "winget",
                ],
                &["7-Zip", "7zip.7zip", "25.01", "26.01", "winget"],
            ],
        );
        let table = format!("{table}\n2 upgrades available.");
        let ups = parse_upgrades(&table);
        assert_eq!(ups.len(), 2);
        assert_eq!(ups[0].name, "Copilot CLI");
        assert_eq!(ups[0].id, "GitHub.Copilot");
        assert_eq!(ups[0].current, "v1.0.44");
        assert_eq!(ups[0].available, "v1.0.63");
    }

    #[test]
    fn parse_upgrades_strips_truncated_id() {
        // A long id is truncated with '…'; winget's `--id` substring match still
        // resolves it, so we keep the prefix and drop the ellipsis.
        let widths = [30, 33, 23, 9, 6];
        let header = ["Name", "Id", "Version", "Available", "Source"];
        let table = render(
            &widths,
            &header,
            &[&[
                "Visual Studio Build Tools 2022",
                "Microsoft.VisualStudio.2022.BuildTools",
                "17.14.25 (January 2026)",
                "17.14.34",
                "winget",
            ]],
        );
        let ups = parse_upgrades(&table);
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].name, "Visual Studio Build Tools 2022");
        assert_eq!(ups[0].id, "Microsoft.VisualStudio.2022.Buil");
        assert_eq!(ups[0].current, "17.14.25 (January 2026)");
        assert_eq!(ups[0].available, "17.14.34");
    }

    #[test]
    fn unmanaged_keeps_correlated_standalone_and_dedupes_managed_and_store() {
        let widths = [24, 32, 18, 7];
        let header = ["Name", "Id", "Version", "Source"];
        let table = render(
            &widths,
            &header,
            &[
                &["7-Zip", "7zip.7zip", "25.01", "winget"], // already in winget upgrade → skip
                &["Discord", "XPDC2RH70K22MN", "1.0.9242", "winget"], // correlated standalone → KEEP
                &["iCloud", "9PKTQ5699M62", "15.8.127.0", "msstore"], // real Store/Appx → skip via token
                &["Git", "ARP\\Machine\\X64\\Git_is1", "2.52.0", ""], // unmanaged → keep
                &["Battle.net", "ARP\\Machine\\X86\\Battle.net", "Unknown", ""], // ".net" must not filter it
                &[
                    "NVIDIA Graphics Driver",
                    "ARP\\Machine\\X64\\{B2FE1952-0186-46C3}",
                    "596.49",
                    "",
                ], // driver → noise
                &[
                    "AV1 Video Extension",
                    "MSIX\\Microsoft.AV1VideoExtension_2.0.7.0_x64",
                    "2.0.7.0",
                    "",
                ], // MSIX/Store → skip
                &[
                    "Microsoft .NET Runtime",
                    "ARP\\Machine\\X64\\{DOTNET8}",
                    "8.0.11",
                    "",
                ], // runtime → noise
            ],
        );
        // Nothing pre-flagged by the winget pass; 7-Zip excluded purely by its catalog id.
        let managed = HashSet::new();
        let apps = parse_unmanaged(&table, &managed);
        let names: Vec<&str> = apps.iter().map(|(n, _)| n.as_str()).collect();
        // The regression guard: a winget-CORRELATED standalone app (Discord shows a
        // Store id + winget source, NOT a Publisher.App catalog id) is detected now.
        assert!(names.contains(&"Discord"), "Discord must be detected now");
        assert_eq!(
            apps.iter().find(|(n, _)| n == "Discord").unwrap().1,
            "1.0.9242"
        );
        assert!(names.contains(&"Git"));
        assert!(names.contains(&"Battle.net"));
        // 7-Zip excluded via its winget catalog id (winget manages it); iCloud via
        // msstore source; driver/runtime via noise; AV1 via the MSIX id.
        assert!(!names.contains(&"7-Zip"));
        assert!(!names.contains(&"iCloud"));
        assert!(!names.contains(&"NVIDIA Graphics Driver"));
        assert!(!names.contains(&"AV1 Video Extension"));
        assert!(!names.contains(&"Microsoft .NET Runtime"));
    }

    #[test]
    fn footer_heuristic_does_not_truncate_on_app_named_like_the_footer() {
        // An app row whose NAME contains both "Upgrade" and "Available" must not be
        // mistaken for the "N upgrades available." footer (it doesn't start with a digit).
        let widths = [34, 24, 10, 9, 6];
        let header = ["Name", "Id", "Version", "Available", "Source"];
        let table = render(
            &widths,
            &header,
            &[
                &[
                    "Available Upgrade Assistant",
                    "Vendor.Assistant",
                    "1.0",
                    "1.1",
                    "winget",
                ],
                &["7-Zip", "7zip.7zip", "25.01", "26.01", "winget"],
            ],
        );
        let table = format!("{table}\n2 upgrades available.");
        let ups = parse_upgrades(&table);
        // Both rows survive; the digit-led footer still terminates the table.
        assert_eq!(ups.len(), 2, "app named like the footer must not truncate");
        assert_eq!(ups[0].name, "Available Upgrade Assistant");
        assert_eq!(ups[1].name, "7-Zip");
    }

    #[test]
    fn catalog_id_distinguishes_managed_from_correlated() {
        assert!(is_winget_catalog_id("Anthropic.Claude"));
        assert!(is_winget_catalog_id("7zip.7zip"));
        assert!(is_winget_catalog_id("JanDeDobbeleer.OhMyPosh"));
        assert!(is_winget_catalog_id("Microsoft.VisualStudio.2022.Buil…")); // truncated, still catalog
        assert!(!is_winget_catalog_id("XPDC2RH70K22MN")); // Store id (Discord)
        assert!(!is_winget_catalog_id("9PKTQ5699M62")); // Store id
        assert!(!is_winget_catalog_id("ARP\\Machine\\X64\\Git_is1")); // ARP id
    }

    #[test]
    fn unmanaged_dedupes_phase1_apps_case_insensitively() {
        let widths = [22, 26, 12, 7];
        let header = ["Name", "Id", "Version", "Source"];
        let table = render(
            &widths,
            &header,
            &[
                &["Obsidian", "ARP\\X64\\Obsidian", "1.5.0", "winget"],
                &["Krita", "ARP\\X64\\Krita", "5.2.0", ""],
            ],
        );
        // managed uses a different letter case than the display name.
        let managed: HashSet<String> = ["obsidian".to_string()].into_iter().collect();
        let apps = parse_unmanaged(&table, &managed);
        let names: Vec<&str> = apps.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&"Obsidian"));
        assert!(names.contains(&"Krita"));
    }

    #[test]
    fn unmanaged_cleans_versioned_display_name() {
        let widths = [30, 36, 10];
        let header = ["Name", "Id", "Version"];
        let table = render(
            &widths,
            &header,
            &[&[
                "Revision Tool version 2.9.0",
                "ARP\\Machine\\X64\\{FC609131}_is1",
                "2.9.0",
            ]],
        );
        let managed = HashSet::new();
        let apps = parse_unmanaged(&table, &managed);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].0, "Revision Tool"); // name cleaned for a stable key
        assert_eq!(apps[0].1, "2.9.0"); // version column preserved
    }
}
