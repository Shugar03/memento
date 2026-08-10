//! Command modules (cluster I: CLI surface). One module per task group:
//! * [`tenant`] — T-082: admin commands (create/rotate/delete ceremony,
//!   retention, export, backup, restore, sweep).
//! * [`ingest`] — T-083: ingest text/document + bulk with the T-080
//!   canonical-path walker.
//! * [`memory`] — T-083: search/get_chunk/feedback/delete/context_fit.
//! * [`stats`] — T-083: stats (REQ-CL-006) + health (REQ-OP-001 Q3).
//! * [`code`] — T-084: code index/status/debug.

pub mod code;
pub mod ingest;
pub mod memory;
pub mod stats;
pub mod tenant;

use memento_domain::{DomainError, TenantId};
use memento_i18n::{I18n, StringKey};

/// Confirmation ceremony for tenant-wide destruction (design: destructive
/// ops get a ceremony). A `yes` line on stdin is required; anything else
/// (including EOF) aborts with a structured validation error — the
/// ceremony tests assert the abort leaves data intact.
///
/// In `--json` mode the human prompt is suppressed (stderr stays pure
/// JSON; machine callers pipe `yes`).
pub(crate) fn confirm_ceremony(i18n: &I18n, tid: &TenantId, json: bool) -> Result<(), DomainError> {
    if !json {
        use std::io::Write;
        eprint!(
            "{} ",
            i18n.t(StringKey::CliPromptConfirmDelete)
                .replace("{tid}", &tid.to_string())
        );
        let _ = std::io::stderr().flush();
    }
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(DomainError::from)?;
    if line.trim() == "yes" {
        Ok(())
    } else {
        Err(DomainError::InvalidInput {
            message: "deletion aborted: type 'yes' to confirm".into(),
        })
    }
}
