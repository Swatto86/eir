//! Core update-engine types and their pure decision logic. No I/O lives here —
//! this is the "Rust disposes" layer: it classifies what a method run means and
//! validates the AI diagnostician's proposed next step against a fixed, safe set
//! of choices. Adapters and the orchestrator depend on these types, not vice
//! versa.

use serde::{Deserialize, Serialize};

/// A package-update method. Closed set, dispatched by exhaustive `match` — no
/// trait object, so the orchestrator can never silently miss a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Winget,
    Choco,
    Scoop,
    MsStore,
    /// An AI-found official installer for an app no package manager can update.
    Native,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Winget => "winget",
            Method::Choco => "choco",
            Method::Scoop => "scoop",
            Method::MsStore => "msstore",
            Method::Native => "native",
        }
    }

    /// Parse a config/wire token into a method. Unknown tokens return `None` so
    /// callers can ignore them rather than fail.
    pub fn from_token(s: &str) -> Option<Method> {
        match s.trim().to_ascii_lowercase().as_str() {
            "winget" => Some(Method::Winget),
            "choco" | "chocolatey" => Some(Method::Choco),
            "scoop" => Some(Method::Scoop),
            "msstore" | "store" | "ms_store" => Some(Method::MsStore),
            "native" => Some(Method::Native),
            _ => None,
        }
    }

    /// Whether a `--force`-style remedy is meaningful for this method.
    pub fn supports_force(self) -> bool {
        matches!(self, Method::Winget | Method::Choco)
    }

    /// Whether this method keeps a clearable global lock (choco's, scoop's).
    pub fn has_manager_lock(self) -> bool {
        matches!(self, Method::Choco | Method::Scoop)
    }
}

/// What an attempt's failure means, classified by Rust from the exit code and
/// captured output — never by the AI. Drives the deterministic fallback ladder
/// and is shown to the AI diagnostician as context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    /// The method doesn't know this app (no package, wrong id).
    NotFound,
    /// A transient download/network failure — a same-method retry may help.
    NetworkTransient,
    /// The downloaded file's SHA-256 didn't match — terminal, never bypassed.
    HashMismatch,
    /// The installer's Authenticode signature failed the policy — terminal.
    SignatureRejected,
    /// The installer ran but returned a failure exit code.
    InstallerFailed,
    /// The method reported success but the installed version didn't move.
    VerifyFailed,
    /// Access denied / insufficient rights for this method.
    PermissionDenied,
    /// Another process or installer holds a lock.
    LockHeld,
    /// The method refused; its documented remedy is a force flag.
    NeedsForce,
    /// A pending reboot is blocking the update.
    NeedsReboot,
    /// The method considers the app already current (nothing to do).
    AlreadyCurrent,
    /// Blocked by policy or configuration.
    Blocked,
    Unknown,
}

impl ErrorCategory {
    /// Integrity failures must never be retried or AI-bypassed.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ErrorCategory::HashMismatch | ErrorCategory::SignatureRejected
        )
    }
}

/// Verdict of the post-update version check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    /// The installed version is at or above the expected version.
    Verified,
    /// The installed version is still older than expected — the update didn't take.
    Mismatch,
    /// Couldn't read a version to compare (the install may still be fine).
    Unverified,
    /// No verification was attempted (e.g. the attempt failed before installing).
    NotChecked,
}

/// One method attempt's structured result.
#[derive(Debug, Clone)]
pub struct AttemptOutcome {
    pub method: Method,
    pub success: bool,
    pub verification: Verification,
    /// `None` on success; the failure category otherwise.
    pub category: Option<ErrorCategory>,
    pub exit_code: Option<i32>,
    /// Version observed after the attempt, if read.
    pub installed_version: Option<String>,
    /// Cleaned, human-readable message (the method's own output or the reason).
    pub detail: String,
    /// Authenticode result, for native installs ("signed: CN", "unsigned", …).
    pub signature: Option<String>,
    /// SHA-256 of the downloaded installer, for native installs.
    pub sha256: Option<String>,
    /// AI spend attributable to this attempt (planning/diagnosis), in USD.
    pub cost_usd: f64,
}

impl AttemptOutcome {
    /// A bare failed outcome for `method` with a category and message.
    pub fn failed(method: Method, category: ErrorCategory, detail: impl Into<String>) -> Self {
        Self {
            method,
            success: false,
            verification: Verification::NotChecked,
            category: Some(category),
            exit_code: None,
            installed_version: None,
            detail: detail.into(),
            signature: None,
            sha256: None,
            cost_usd: 0.0,
        }
    }
}

/// An app the check step found to be updatable, with the methods that could plausibly
/// handle it (in preference order).
#[derive(Debug, Clone)]
pub struct UpdateCandidate {
    /// Stable, version-stripped, lowercased identity (note/ignore key).
    pub id: String,
    /// Display name shown to the user and the AI.
    pub name: String,
    /// Version currently installed.
    pub current: String,
    /// Target version we expect after the update (winget's "Available", or the AI's
    /// latest). Empty when unknown; used as the post-update verification target.
    pub available: String,
    /// Method-specific package id where known (e.g. a winget `--id`).
    pub package_id: Option<String>,
    /// Methods to try, in order.
    pub methods: Vec<Method>,
}

/// An allow-listed remedy the AI may request before retrying a method. The set is
/// fixed (it is an enum), so the AI can only ever *select*; Rust applies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Remedy {
    /// Re-run with the method's force switch.
    Force,
    /// Kill a process the error names as holding a lock. The name must actually
    /// appear in the captured error text, or the remedy is rejected.
    KillProcess { name: String },
    /// Clear the package manager's stale lock before retrying.
    ClearManagerLock,
    /// Defer: a reboot is required before the update can succeed.
    RetryAfterReboot,
}

/// The AI diagnostician's untrusted proposal for what to do after a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposedStep {
    /// Try a different method.
    Switch { method: Method },
    /// Retry a method (possibly the same one) after applying a remedy.
    Retry { method: Method, remedy: Remedy },
    /// Stop trying this app.
    GiveUp { reason: String },
}

/// The validated decision the orchestrator will actually act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextStep {
    SwitchTo(Method),
    RetryWith(Method, Remedy),
    GiveUp(String),
}

/// Everything the validator needs to dispose of the AI's proposal.
pub struct StepContext<'a> {
    /// Category of the attempt that just failed.
    pub failed: ErrorCategory,
    /// Methods already attempted for this app.
    pub tried: &'a [Method],
    /// Methods usable on this machine and enabled in config.
    pub available: &'a [Method],
    /// The captured error text (used to validate a KillProcess target).
    pub error_text: &'a str,
}

/// The deterministic fallback: the first available, not-yet-tried method, else
/// give up. This is what runs when the AI is unavailable, over budget, or proposes
/// something invalid.
pub fn deterministic_next(ctx: &StepContext) -> NextStep {
    if ctx.failed.is_terminal() {
        return NextStep::GiveUp("integrity check failed".to_string());
    }
    match ctx.available.iter().find(|m| !ctx.tried.contains(m)) {
        Some(&m) => NextStep::SwitchTo(m),
        None => NextStep::GiveUp("all available methods tried".to_string()),
    }
}

/// Dispose of the AI's proposed next step. Rust has the final say:
///  - integrity-terminal failures always give up, whatever the AI says;
///  - a switched/retried method must be available (and, for a switch, untried);
///  - a remedy must make sense for the method and the failure, and a KillProcess
///    target must actually appear in the error text;
///  - anything invalid falls back to the deterministic next step.
pub fn validate_next_step(proposed: ProposedStep, ctx: &StepContext) -> NextStep {
    if ctx.failed.is_terminal() {
        return NextStep::GiveUp("integrity check failed".to_string());
    }
    match proposed {
        ProposedStep::GiveUp { reason } => {
            let reason = reason.trim();
            NextStep::GiveUp(if reason.is_empty() {
                "AI advised giving up".to_string()
            } else {
                reason.to_string()
            })
        }
        ProposedStep::Switch { method } => {
            if ctx.available.contains(&method) && !ctx.tried.contains(&method) {
                NextStep::SwitchTo(method)
            } else {
                deterministic_next(ctx)
            }
        }
        ProposedStep::Retry { method, remedy } => {
            if ctx.available.contains(&method) && remedy_ok(&remedy, method, ctx) {
                NextStep::RetryWith(method, remedy)
            } else {
                deterministic_next(ctx)
            }
        }
    }
}

/// Whether a remedy is valid for the method and the failure that prompted it.
fn remedy_ok(remedy: &Remedy, method: Method, ctx: &StepContext) -> bool {
    match remedy {
        Remedy::Force => method.supports_force(),
        Remedy::ClearManagerLock => method.has_manager_lock(),
        Remedy::RetryAfterReboot => ctx.failed == ErrorCategory::NeedsReboot,
        Remedy::KillProcess { name } => {
            let n = name.trim().to_ascii_lowercase();
            let stem = n.strip_suffix(".exe").unwrap_or(&n);
            if stem.len() < 5 {
                return false;
            }
            // The stem must appear as a WHOLE TOKEN of the captured error (not a
            // substring of a larger path/word like "setuphost" matching "host"), so
            // the AI can only kill a process the error actually names as the blocker.
            ctx.error_text
                .to_ascii_lowercase()
                .split(|c: char| {
                    c.is_whitespace()
                        || matches!(c, '\\' | '/' | '"' | '\'' | ':' | ',' | ';' | '(' | ')')
                })
                .map(|t| t.strip_suffix(".exe").unwrap_or(t))
                .any(|t| t == stem)
        }
    }
}

/// Map a method's exit code and captured output to an [`ErrorCategory`]. Heuristic
/// and shared; individual adapters may refine it. Ordering matters — integrity and
/// reboot signals are checked before the generic "non-zero code = installer failed".
pub fn classify_error(_method: Method, exit_code: Option<i32>, output: &str) -> ErrorCategory {
    use ErrorCategory as E;
    let o = output.to_ascii_lowercase();
    let has = |needle: &str| o.contains(needle);

    // Our own integrity checks (surfaced in the detail text) — terminal.
    if has("sha-256 mismatch") || has("sha256 mismatch") || has("hash mismatch") {
        return E::HashMismatch;
    }
    if has("signature")
        && (has("reject") || has("unsigned") || has("untrusted") || has("not signed"))
    {
        return E::SignatureRejected;
    }
    // The documented force guard (winget portable, choco checksum override).
    if has("has been modified") && has("--force") {
        return E::NeedsForce;
    }
    // Pending reboot.
    if exit_code == Some(3010) || has("pending reboot") || (has("restart") && has("required")) {
        return E::NeedsReboot;
    }
    // A held lock / concurrent installer.
    if has("being used by another process")
        || has("another installation is already in progress")
        || has("0x80070652")
        || (has("lock") && (has("held") || has("could not acquire")))
    {
        return E::LockHeld;
    }
    // Access / elevation.
    if has("access is denied")
        || has("access denied")
        || exit_code == Some(5)
        || exit_code == Some(740)
    {
        return E::PermissionDenied;
    }
    // Network / download.
    if has("timed out")
        || has("could not be resolved")
        || has("connection")
        || has("download failed")
        || has("network")
    {
        return E::NetworkTransient;
    }
    // The method considers it already current.
    if has("no applicable upgrade")
        || has("no available upgrade")
        || has("already up to date")
        || has("already installed")
        || has("no newer version")
    {
        return E::AlreadyCurrent;
    }
    // The method doesn't know this app.
    if has("no installed package found")
        || has("no package found")
        || has("not found")
        || has("could not find")
    {
        return E::NotFound;
    }
    // Any other non-success exit code means the installer/method failed.
    match exit_code {
        Some(c) if c != 0 && c != 3010 => E::InstallerFailed,
        _ => E::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_token_round_trip() {
        for m in [
            Method::Winget,
            Method::Choco,
            Method::Scoop,
            Method::MsStore,
            Method::Native,
        ] {
            assert_eq!(Method::from_token(m.as_str()), Some(m));
        }
        assert_eq!(Method::from_token("chocolatey"), Some(Method::Choco));
        assert_eq!(Method::from_token("STORE"), Some(Method::MsStore));
        assert_eq!(Method::from_token("apt"), None);
    }

    #[test]
    fn classify_error_categorises_common_signals() {
        use ErrorCategory as E;
        let w = Method::Winget;
        assert_eq!(
            classify_error(w, None, "download SHA-256 mismatch"),
            E::HashMismatch
        );
        assert_eq!(
            classify_error(w, None, "signature rejected: unsigned installer"),
            E::SignatureRejected
        );
        assert_eq!(
            classify_error(
                w,
                Some(1),
                "Unable to remove Portable package as it has been modified; use --force"
            ),
            E::NeedsForce
        );
        assert_eq!(classify_error(w, Some(3010), "ok"), E::NeedsReboot);
        assert_eq!(
            classify_error(w, Some(1), "The file is being used by another process"),
            E::LockHeld
        );
        assert_eq!(
            classify_error(w, Some(5), "Access is denied"),
            E::PermissionDenied
        );
        assert_eq!(
            classify_error(w, None, "The request timed out"),
            E::NetworkTransient
        );
        assert_eq!(
            classify_error(w, Some(0), "No applicable upgrade found"),
            E::AlreadyCurrent
        );
        assert_eq!(
            classify_error(w, None, "No installed package found matching input"),
            E::NotFound
        );
        assert_eq!(
            classify_error(w, Some(1603), "Installer failed with exit code: 1603"),
            E::InstallerFailed
        );
    }

    #[test]
    fn integrity_failure_always_gives_up() {
        let ctx = StepContext {
            failed: ErrorCategory::HashMismatch,
            tried: &[Method::Winget],
            available: &[Method::Winget, Method::Choco],
            error_text: "",
        };
        // Even if the AI wants to switch, an integrity failure is terminal.
        let step = validate_next_step(
            ProposedStep::Switch {
                method: Method::Choco,
            },
            &ctx,
        );
        assert!(matches!(step, NextStep::GiveUp(_)));
        assert!(matches!(deterministic_next(&ctx), NextStep::GiveUp(_)));
    }

    #[test]
    fn switch_to_unavailable_or_tried_method_falls_back() {
        let ctx = StepContext {
            failed: ErrorCategory::InstallerFailed,
            tried: &[Method::Winget],
            available: &[Method::Winget, Method::Choco, Method::Scoop],
            error_text: "boom",
        };
        // Scoop isn't tried and is available -> honoured.
        assert_eq!(
            validate_next_step(
                ProposedStep::Switch {
                    method: Method::Scoop
                },
                &ctx
            ),
            NextStep::SwitchTo(Method::Scoop)
        );
        // MsStore isn't available -> deterministic fallback (first untried: Choco).
        assert_eq!(
            validate_next_step(
                ProposedStep::Switch {
                    method: Method::MsStore
                },
                &ctx
            ),
            NextStep::SwitchTo(Method::Choco)
        );
        // Switching back to an already-tried method -> fallback.
        assert_eq!(
            validate_next_step(
                ProposedStep::Switch {
                    method: Method::Winget
                },
                &ctx
            ),
            NextStep::SwitchTo(Method::Choco)
        );
    }

    #[test]
    fn retry_remedy_is_validated() {
        let ctx = StepContext {
            failed: ErrorCategory::NeedsForce,
            tried: &[Method::Winget],
            available: &[Method::Winget, Method::Choco],
            error_text: "package has been modified; use --force",
        };
        // Force on winget (which supports it) is honoured even though winget was tried.
        assert_eq!(
            validate_next_step(
                ProposedStep::Retry {
                    method: Method::Winget,
                    remedy: Remedy::Force
                },
                &ctx
            ),
            NextStep::RetryWith(Method::Winget, Remedy::Force)
        );
        // Force on a method that doesn't support it -> fallback to next untried (Choco).
        assert_eq!(
            validate_next_step(
                ProposedStep::Retry {
                    method: Method::Scoop,
                    remedy: Remedy::Force
                },
                &ctx
            ),
            NextStep::SwitchTo(Method::Choco)
        );
    }

    #[test]
    fn kill_process_remedy_requires_the_name_in_the_error() {
        let ctx = StepContext {
            failed: ErrorCategory::LockHeld,
            tried: &[Method::Winget],
            available: &[Method::Winget],
            error_text: "file locked by firefox.exe",
        };
        // The error names firefox -> allowed.
        assert_eq!(
            validate_next_step(
                ProposedStep::Retry {
                    method: Method::Winget,
                    remedy: Remedy::KillProcess {
                        name: "firefox.exe".into()
                    },
                },
                &ctx
            ),
            NextStep::RetryWith(
                Method::Winget,
                Remedy::KillProcess {
                    name: "firefox.exe".into()
                }
            )
        );
        // A process NOT named in the error is rejected -> deterministic give-up
        // (no other method available).
        assert!(matches!(
            validate_next_step(
                ProposedStep::Retry {
                    method: Method::Winget,
                    remedy: Remedy::KillProcess {
                        name: "explorer.exe".into()
                    },
                },
                &ctx
            ),
            NextStep::GiveUp(_)
        ));
    }
}
