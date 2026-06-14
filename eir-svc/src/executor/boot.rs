use anyhow::{bail, Result};

const SAFE_ELEMENTS: &[&str] = &[
    "timeout",
    "bootmenupolicy",
    "bootstatuspolicy",
    "recoveryenabled",
    "quietboot",
    "nx",
];

pub async fn bcd_edit(element: &str, value: &str) -> Result<String> {
    let el_lower = element.to_lowercase();
    if !SAFE_ELEMENTS.iter().any(|&e| e == el_lower) {
        bail!(
            "BCD element '{element}' is not on the safe list \
             (allowed: {})",
            SAFE_ELEMENTS.join(", ")
        );
    }
    // Reject shell-injection characters in the value
    if value.chars().any(|c| {
        matches!(
            c,
            '{' | '}' | ';' | '&' | '|' | '`' | '$' | '(' | ')' | '\n' | '\r' | '"'
        )
    }) {
        bail!("BCD value contains disallowed characters: {value}");
    }

    let safe_el = element.replace('\'', "''");
    let safe_val = value.replace('\'', "''");
    // Single-quote {current} so PowerShell doesn't try to expand the braces.
    let script = format!(
        "bcdedit /set '{{current}}' {safe_el} '{safe_val}'; \
         if ($LASTEXITCODE -ne 0) {{ throw 'bcdedit failed' }}; \
         Write-Output 'BCD updated: {safe_el}={safe_val}'"
    );
    super::powershell::run_diagnostic(&script).await
}
