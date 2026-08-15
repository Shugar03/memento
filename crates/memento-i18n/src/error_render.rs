//! Bilingual rendering of `DomainError` (REQ-MS-004/005, REQ-CL-004).

use crate::strings::{Locale, StringKey, lookup};
use memento_domain::DomainError;
use serde_json::{Value, json};

/// Map a `DomainError` to its string-table key (one per stable code, T-011).
fn error_key(err: &DomainError) -> StringKey {
    match err {
        DomainError::TenantRequired => StringKey::ErrTenantRequired,
        DomainError::TenantForbidden => StringKey::ErrTenantForbidden,
        DomainError::QuotaExceeded { .. } => StringKey::ErrQuotaExceeded,
        DomainError::EmbeddingModelMismatch { .. } => StringKey::ErrEmbeddingModelMismatch,
        DomainError::TopKExceeded { .. } => StringKey::ErrTopKExceeded,
        DomainError::WorkspaceRequired => StringKey::ErrWorkspaceRequired,
        DomainError::ChunkNotFound { .. } => StringKey::ErrChunkNotFound,
        DomainError::ResourceExhausted { .. } => StringKey::ErrResourceExhausted,
        DomainError::CapacityExceeded { .. } => StringKey::ErrCapacityExceeded,
        DomainError::Internal { .. } => StringKey::ErrInternal,
        DomainError::NotFound { .. } => StringKey::ErrNotFound,
        DomainError::InvalidInput { .. } => StringKey::ErrInvalidInput,
        DomainError::AlreadyExists { .. } => StringKey::ErrAlreadyExists,
        DomainError::AuthFailed => StringKey::ErrAuthFailed,
        DomainError::Io { .. } => StringKey::ErrIo,
        DomainError::Parse { .. } => StringKey::ErrParse,
        DomainError::EmbeddingFailed { .. } => StringKey::ErrEmbeddingFailed,
        DomainError::RerankFailed { .. } => StringKey::ErrRerankFailed,
        DomainError::BackupCorrupt { .. } => StringKey::ErrBackupCorrupt,
        DomainError::BackupVersion { .. } => StringKey::ErrBackupVersion,
        DomainError::SubprocessTimeout { .. } => StringKey::ErrSubprocessTimeout,
        DomainError::SubprocessStdoutOverflow { .. } => StringKey::ErrSubprocessStdoutOverflow,
        DomainError::SubprocessArgvInvalid { .. } => StringKey::ErrSubprocessArgvInvalid,
        DomainError::DaemonUnavailable { .. } => StringKey::ErrDaemonUnavailable,
        DomainError::StoreBusy { .. } => StringKey::ErrStoreBusy,
        DomainError::StoreLocked { .. } => StringKey::ErrStoreLocked,
    }
}

/// Render an error message for `locale`. Deterministic: the same
/// `(err, locale)` always produces the same string. Spanish for `Es`,
/// English for `En`.
pub fn format_error(err: &DomainError, locale: Locale) -> String {
    lookup(error_key(err), locale).to_string()
}

/// Structured error for MCP (REQ-MS-005): stable code + bilingual message +
/// technical detail + deterministic exit code.
pub fn format_error_json(err: &DomainError, locale: Locale) -> Value {
    json!({
        "code": err.code(),
        "message": format_error(err, locale),
        "detail": err.to_string(),
        "exit_code": err.exit_code(),
    })
}
