//! Output contract (REQ-CL-003/005): canonical JSON + human lines.
//!
//! * `--json` output goes to stdout as compact JSON; the schema mirrors the
//!   MCP DTOs (REQ-MS-006 equivalence) — same fields, same provenance shape.
//! * Errors go to STDERR: a bilingual human message, or a structured JSON
//!   object `{code, message, detail, exit_code}` in `--json` mode
//!   (REQ-CL-003: "JSON error output MUST be structured").
//! * The process exit code is always [`DomainError::exit_code`]
//!   (REQ-CL-005) — identical in human and JSON modes.

use memento_domain::DomainError;
use memento_i18n::{I18n, format_error_json};
use serde::Serialize;

/// Print a serializable result as the canonical JSON envelope (stdout).
///
/// # Errors
///
/// * `Internal` — the value cannot be serialized (programming error).
pub fn emit_json(value: &impl Serialize) -> Result<(), DomainError> {
    let raw = serde_json::to_string(value).map_err(|err| DomainError::Internal {
        message: format!("cannot serialize CLI output: {err}"),
    })?;
    println!("{raw}");
    Ok(())
}

/// Print a pre-built JSON value (stdout).
pub fn emit_json_value(value: &serde_json::Value) {
    println!("{value}");
}

/// Render a domain error for the user: bilingual message (ES primary, EN
/// fallback) plus the technical detail line. JSON mode emits the structured
/// envelope `{code, message, detail, exit_code}` (REQ-CL-003).
pub fn report_error(err: &DomainError, i18n: &I18n, json: bool) {
    if json {
        let value = format_error_json(err, i18n.locale());
        eprintln!("{value}");
    } else {
        eprintln!("error: {}", i18n.format_error(err));
        eprintln!("       {err}");
    }
}
