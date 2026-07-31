//! On-disk configuration for the update subsystem (the `[updater]` section of
//! `config.toml`). The whole struct is `#[serde(default)]`, and the parent
//! `Config` carries it as `#[serde(default)]`, so an existing `config.toml` with
//! no `[updater]` section still loads — every field falls back to a sane default.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_APP_NOTES: usize = 500;
pub const MAX_APP_NOTE_CHARS: usize = 2_000;
pub const MAX_APP_ID_CHARS: usize = 200;
pub const MAX_IGNORED_APPS: usize = 500;
pub const MAX_SCHEDULE_INTERVAL_SECS: u64 = 365 * 24 * 3600;
pub const MAX_ATTEMPTS_PER_APP: u32 = 10;
pub const MAX_APPS_PER_RUN: u32 = 100;
pub const MAX_INSTALLER_MB: u64 = 4_096;

pub fn valid_app_id(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty() && id.chars().count() <= MAX_APP_ID_CHARS && !id.chars().any(char::is_control)
}

fn sanitize_methods(methods: Vec<String>) -> Vec<String> {
    let mut safe = Vec::new();
    for method in methods {
        let method = method.trim().to_ascii_lowercase();
        if matches!(method.as_str(), "winget" | "choco" | "scoop" | "msstore")
            && !safe.contains(&method)
        {
            safe.push(method);
        }
    }
    safe
}

fn sanitize_ignored(ids: Vec<String>) -> Vec<String> {
    let mut safe = Vec::new();
    for id in ids {
        if safe.len() >= MAX_IGNORED_APPS {
            break;
        }
        let id = id.trim().to_lowercase();
        if valid_app_id(&id) && !safe.contains(&id) {
            safe.push(id);
        }
    }
    safe
}

fn sanitize_notes(notes: BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut safe = BTreeMap::new();
    for (id, note) in notes {
        if safe.len() >= MAX_APP_NOTES {
            break;
        }
        let id = id.trim().to_lowercase();
        let note = note.trim();
        if valid_app_id(&id) && !note.is_empty() && note.chars().count() <= MAX_APP_NOTE_CHARS {
            safe.insert(id, note.to_string());
        }
    }
    safe
}

/// How strictly a downloaded AI-found native installer must be signed before it
/// is allowed to run. Decided in Rust *before* the installer is staged, never by
/// the AI. Defaults to the safe choice for an unattended SYSTEM install.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SignaturePolicy {
    /// Installer must carry a valid Authenticode signature (any trusted signer).
    #[default]
    RequireValid,
    /// Valid signature AND the signer subject must contain the expected publisher.
    RequirePublisherMatch,
    /// Run regardless of signature — least safe, explicit opt-in only.
    AllowUnsigned,
}

impl SignaturePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            SignaturePolicy::RequireValid => "require_valid",
            SignaturePolicy::RequirePublisherMatch => "require_publisher_match",
            SignaturePolicy::AllowUnsigned => "allow_unsigned",
        }
    }

    /// Parse a wire token; anything unrecognised falls back to the safe default so a
    /// bad value never weakens the gate.
    pub fn from_token(s: &str) -> SignaturePolicy {
        match s.trim().to_ascii_lowercase().as_str() {
            "require_publisher_match" => SignaturePolicy::RequirePublisherMatch,
            "allow_unsigned" => SignaturePolicy::AllowUnsigned,
            _ => SignaturePolicy::RequireValid,
        }
    }
}

/// Configuration for the autonomous updater. Field order matters for TOML
/// serialization: all scalars/arrays come before the `notes` sub-table so the
/// emitted `[updater]` section is valid (a sub-table must follow its parent's
/// own keys).
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct UpdaterConfig {
    /// Master switch. Off by default — running installers unattended as SYSTEM is
    /// opt-in; the user turns it on in Settings.
    pub enabled: bool,
    /// How often an unattended update cycle runs, in seconds.
    pub schedule_interval_secs: u64,
    /// Package-manager methods to use, in preference order. Unknown names are
    /// ignored at use time. `native` (AI-found installers) is gated separately by
    /// `native_enabled`, not listed here.
    pub methods: Vec<String>,
    /// Whether AI-found native installers may be downloaded and run at all.
    pub native_enabled: bool,
    /// Signature gate applied to native installs.
    pub native_signature_policy: SignaturePolicy,
    /// Max methods tried for one app before giving up.
    pub max_attempts_per_app: u32,
    /// Max apps acted on in a single cycle (bounds cost and blast radius).
    pub max_apps_per_run: u32,
    /// Largest native installer to download, in MiB.
    pub max_installer_mb: u64,
    /// Auto-install Chocolatey when that method needs it. Scoop is never bootstrapped
    /// or run by the SYSTEM service.
    pub bootstrap_managers: bool,
    /// App identities to skip entirely. Keyed by the stable, version-stripped,
    /// lowercased display name (the same key used for notes).
    pub ignored: Vec<String>,
    /// Per-app freeform hints for the AI, keyed by the stable app identity. A
    /// `BTreeMap` so serialization is deterministic (stable diffs/tests).
    pub notes: BTreeMap<String, String>,
    /// Guidance that has subsequently produced a successful targeted update.
    /// Kept separately so close product variants can reuse proven instructions.
    pub learned_notes: BTreeMap<String, String>,
}

impl UpdaterConfig {
    /// Bound values loaded from disk before they can control loops, batch size, or
    /// native download limits.
    pub fn sanitize(&mut self) {
        self.schedule_interval_secs = self
            .schedule_interval_secs
            .clamp(300, MAX_SCHEDULE_INTERVAL_SECS);
        self.max_attempts_per_app = self.max_attempts_per_app.clamp(1, MAX_ATTEMPTS_PER_APP);
        self.max_apps_per_run = self.max_apps_per_run.clamp(1, MAX_APPS_PER_RUN);
        self.max_installer_mb = self.max_installer_mb.clamp(1, MAX_INSTALLER_MB);
        self.methods = sanitize_methods(std::mem::take(&mut self.methods));
        self.ignored = sanitize_ignored(std::mem::take(&mut self.ignored));
        self.notes = sanitize_notes(std::mem::take(&mut self.notes));
        self.learned_notes = sanitize_notes(std::mem::take(&mut self.learned_notes));
        self.learned_notes
            .retain(|id, note| self.notes.get(id) == Some(note));
    }

    /// The settings the UI shows (no secrets here).
    pub fn to_view(&self) -> eir_proto::UpdaterSettingsView {
        eir_proto::UpdaterSettingsView {
            enabled: self.enabled,
            schedule_interval_secs: self.schedule_interval_secs,
            methods: self.methods.clone(),
            native_enabled: self.native_enabled,
            native_signature_policy: self.native_signature_policy.as_str().to_string(),
            ignored: self.ignored.clone(),
        }
    }

    /// Apply a UI settings change in place. The schedule is clamped to a sane floor,
    /// and an empty method list is ignored (keeps the existing one).
    pub fn apply_view(&mut self, u: eir_proto::UpdaterSettingsUpdate) {
        self.enabled = u.enabled;
        self.schedule_interval_secs = u
            .schedule_interval_secs
            .clamp(300, MAX_SCHEDULE_INTERVAL_SECS);
        if !u.methods.is_empty() {
            let methods = sanitize_methods(u.methods);
            if !methods.is_empty() {
                self.methods = methods;
            }
        }
        self.native_enabled = u.native_enabled;
        self.native_signature_policy = SignaturePolicy::from_token(&u.native_signature_policy);
    }

    /// Save, replace, or clear one app's AI guidance. Returns whether config changed.
    pub fn set_app_note(&mut self, id: &str, note: &str) -> Result<bool, &'static str> {
        let key = id.trim().to_lowercase();
        if !valid_app_id(&key) {
            return Err("invalid app id");
        }
        let note = note.trim();
        if note.chars().count() > MAX_APP_NOTE_CHARS {
            return Err("app note is too long");
        }
        if note.is_empty() {
            let changed =
                self.notes.remove(&key).is_some() | self.learned_notes.remove(&key).is_some();
            return Ok(changed);
        }
        if !self.notes.contains_key(&key) && self.notes.len() >= MAX_APP_NOTES {
            return Err("app note limit reached");
        }
        if self.notes.get(&key).map(String::as_str) == Some(note) {
            return Ok(false);
        }
        self.notes.insert(key.clone(), note.to_string());
        self.learned_notes.remove(&key);
        Ok(true)
    }

    /// Promote guidance only after it has contributed to an observed successful
    /// targeted update. Future close-name variants may reuse it.
    pub fn set_learned_guidance(&mut self, id: &str, note: &str) -> Result<bool, &'static str> {
        let key = id.trim().to_lowercase();
        if !valid_app_id(&key) {
            return Err("invalid app id");
        }
        let note = note.trim();
        if note.is_empty() || note.chars().count() > MAX_APP_NOTE_CHARS {
            return Err("invalid learned guidance");
        }
        if !self.notes.contains_key(&key) && self.notes.len() >= MAX_APP_NOTES {
            return Err("app note limit reached");
        }
        let changed = self.notes.get(&key).map(String::as_str) != Some(note)
            || self.learned_notes.get(&key).map(String::as_str) != Some(note);
        self.notes.insert(key.clone(), note.to_string());
        self.learned_notes.insert(key, note.to_string());
        Ok(changed)
    }

    /// Exact per-app guidance wins. Otherwise reuse proven guidance only for a close
    /// name variant (for example `tool` and `tool x64`), never an unrelated app.
    pub fn guidance_for<'a>(&'a self, id: &str) -> Option<&'a str> {
        let key = id.trim().to_lowercase();
        if let Some(note) = self.notes.get(&key) {
            return Some(note);
        }
        let compact = compact_id(&key);
        self.learned_notes
            .iter()
            .filter(|(learned_id, _)| {
                let learned = compact_id(learned_id);
                learned.len() >= 4 && (compact.contains(&learned) || learned.contains(&compact))
            })
            .max_by_key(|(learned_id, _)| compact_id(learned_id).len())
            .map(|(_, note)| note.as_str())
    }

    /// Add or remove one stable app identity from the ignore list.
    pub fn set_app_ignored(&mut self, id: &str, ignored: bool) -> Result<bool, &'static str> {
        let key = id.trim().to_lowercase();
        if !valid_app_id(&key) {
            return Err("invalid app id");
        }
        if ignored {
            if self
                .ignored
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&key))
            {
                return Ok(false);
            }
            if self.ignored.len() >= MAX_IGNORED_APPS {
                return Err("ignore list limit reached");
            }
            self.ignored.push(key);
            Ok(true)
        } else {
            let old_len = self.ignored.len();
            self.ignored
                .retain(|existing| !existing.eq_ignore_ascii_case(&key));
            Ok(self.ignored.len() != old_len)
        }
    }

    pub fn note_views(&self) -> Vec<eir_proto::UpdaterAppNote> {
        self.notes
            .iter()
            .filter(|(id, _)| valid_app_id(id))
            .map(|(id, note)| eir_proto::UpdaterAppNote {
                id: id.clone(),
                note: note.clone(),
            })
            .collect()
    }
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            schedule_interval_secs: 24 * 3600,
            methods: vec![
                "winget".to_string(),
                "choco".to_string(),
                "msstore".to_string(),
            ],
            native_enabled: true,
            native_signature_policy: SignaturePolicy::default(),
            max_attempts_per_app: 3,
            max_apps_per_run: 20,
            max_installer_mb: 256,
            bootstrap_managers: true,
            ignored: Vec::new(),
            notes: BTreeMap::new(),
            learned_notes: BTreeMap::new(),
        }
    }
}

fn compact_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_policy_round_trips_as_snake_case_string() {
        // TOML stores the enum as a bare string; the snake_case names must survive.
        for (policy, token) in [
            (SignaturePolicy::RequireValid, "require_valid"),
            (
                SignaturePolicy::RequirePublisherMatch,
                "require_publisher_match",
            ),
            (SignaturePolicy::AllowUnsigned, "allow_unsigned"),
        ] {
            #[derive(Serialize, Deserialize)]
            struct Wrap {
                p: SignaturePolicy,
            }
            let s = toml::to_string(&Wrap { p: policy }).unwrap();
            assert!(s.contains(token), "{s} should contain {token}");
            let back: Wrap = toml::from_str(&s).unwrap();
            assert_eq!(back.p, policy);
        }
    }

    #[test]
    fn default_serializes_and_round_trips() {
        let cfg = UpdaterConfig::default();
        let toml = toml::to_string_pretty(&cfg).expect("serialize");
        let back: UpdaterConfig = toml::from_str(&toml).expect("reparse");
        assert_eq!(back.enabled, cfg.enabled);
        assert_eq!(back.methods, cfg.methods);
        assert!(!cfg.methods.iter().any(|method| method == "scoop"));
        assert_eq!(back.native_signature_policy, cfg.native_signature_policy);
        assert_eq!(back.max_apps_per_run, cfg.max_apps_per_run);
    }

    #[test]
    fn app_note_can_be_saved_replaced_and_cleared() {
        let mut cfg = UpdaterConfig::default();
        assert!(cfg
            .set_app_note(
                "Example App",
                "Official downloads: https://example.com/download"
            )
            .expect("save"));
        assert_eq!(
            cfg.notes.get("example app").map(String::as_str),
            Some("Official downloads: https://example.com/download")
        );
        assert!(cfg
            .set_app_note("example app", "Not the unrelated namesake")
            .expect("replace"));
        assert!(cfg.set_app_note("example app", " ").expect("clear"));
        assert!(!cfg.notes.contains_key("example app"));
        assert!(cfg
            .set_app_note("example app", &"x".repeat(MAX_APP_NOTE_CHARS + 1))
            .is_err());
    }

    #[test]
    fn successful_guidance_is_reused_for_related_app_ids_until_edited() {
        let mut cfg = UpdaterConfig::default();
        cfg.set_learned_guidance(
            "Example Tool",
            "Official releases: https://example.com/releases/latest",
        )
        .expect("learn");

        assert_eq!(
            cfg.guidance_for("example tool x64"),
            Some("Official releases: https://example.com/releases/latest")
        );
        assert!(cfg
            .set_app_note("example tool", "Use winget package Example.Tool")
            .expect("edit"));
        assert_eq!(cfg.guidance_for("example tool x64"), None);
        assert_eq!(
            cfg.guidance_for("example tool"),
            Some("Use winget package Example.Tool")
        );
    }

    #[test]
    fn app_ignore_rejects_empty_and_oversized_ids() {
        let mut cfg = UpdaterConfig::default();
        assert!(cfg.set_app_ignored("", true).is_err());
        assert!(cfg
            .set_app_ignored(&"x".repeat(MAX_APP_ID_CHARS + 1), true)
            .is_err());
        assert!(cfg.set_app_ignored("bad\nid", true).is_err());
        assert!(cfg.ignored.is_empty());
    }

    #[test]
    fn app_ignore_normalises_dedupes_and_removes_ids() {
        let mut cfg = UpdaterConfig::default();
        assert!(cfg.set_app_ignored(" Example App ", true).expect("add"));
        assert_eq!(cfg.ignored, ["example app"]);
        assert_eq!(cfg.to_view().ignored, ["example app"]);
        assert!(!cfg.set_app_ignored("EXAMPLE APP", true).expect("dedupe"));
        assert!(cfg.set_app_ignored(" example app ", false).expect("remove"));
        assert!(cfg.ignored.is_empty());
    }

    #[test]
    fn app_ignore_enforces_the_config_owned_cap() {
        let mut cfg = UpdaterConfig {
            ignored: (0..MAX_IGNORED_APPS)
                .map(|index| format!("app {index}"))
                .collect(),
            ..UpdaterConfig::default()
        };
        assert!(cfg.set_app_ignored("one too many", true).is_err());
        assert!(!cfg.set_app_ignored("APP 0", true).expect("existing"));
    }

    #[test]
    fn schedule_interval_cannot_wrap_the_signed_scheduler_timestamp() {
        let mut cfg = UpdaterConfig::default();
        cfg.apply_view(eir_proto::UpdaterSettingsUpdate {
            schedule_interval_secs: u64::MAX,
            ..Default::default()
        });
        assert!(cfg.schedule_interval_secs <= i64::MAX as u64);
        assert_ne!(cfg.schedule_interval_secs, u64::MAX);
    }

    #[test]
    fn loaded_updater_collections_are_normalized_and_bounded() {
        let mut cfg = UpdaterConfig {
            methods: vec![
                " WINGET ".into(),
                "winget".into(),
                "unknown".into(),
                "CHOCO".into(),
            ],
            ignored: (0..=MAX_IGNORED_APPS)
                .map(|index| format!(" APP {index} "))
                .chain(["".into(), "bad\nid".into()])
                .collect(),
            notes: (0..=MAX_APP_NOTES)
                .map(|index| (format!(" APP {index} "), " guidance ".into()))
                .chain([("bad\nid".into(), "unsafe".into())])
                .collect(),
            learned_notes: BTreeMap::from([
                (" APP 0 ".into(), " guidance ".into()),
                ("orphan".into(), "unused".into()),
            ]),
            ..UpdaterConfig::default()
        };

        cfg.sanitize();

        assert_eq!(cfg.methods, ["winget", "choco"]);
        assert_eq!(cfg.ignored.len(), MAX_IGNORED_APPS);
        assert!(cfg.ignored.iter().all(|id| valid_app_id(id)));
        assert_eq!(cfg.notes.len(), MAX_APP_NOTES);
        assert!(cfg
            .notes
            .iter()
            .all(|(id, note)| { valid_app_id(id) && note.chars().count() <= MAX_APP_NOTE_CHARS }));
        assert_eq!(
            cfg.learned_notes.get("app 0").map(String::as_str),
            Some("guidance")
        );
        assert!(!cfg.learned_notes.contains_key("orphan"));
    }
}
