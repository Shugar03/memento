//! Domain error taxonomy (design D7).
//!
//! The taxonomy lives in memento-domain so every surface shares one
//! definition: CLI exit codes (REQ-CL-005) and MCP structured errors
//! (REQ-MS-005) both key off the stable codes below.
//!
//! # Stability contract
//!
//! * [`DomainError::code`] returns a stable string code. Codes MUST NOT change
//!   between releases — they are part of the external contract.
//! * [`DomainError::exit_code`] maps each variant to a deterministic process
//!   exit code used by the CLI (REQ-CL-005).
//!
//! ```text
//! Category        | exit codes
//! ----------------|-------------
//! internal        | 1
//! validation      | 2, 14, 15
//! duplicate       | 3
//! auth            | 4, 10, 11
//! io / parse      | 5, 6, 7
//! backup          | 8, 9, 17
//! quota           | 12, 13, 16
//! not found       | 20, 21
//! subprocess      | 30, 31, 32
//! ```

use crate::chunk::ChunkId;
use serde_json::{Value, json};

/// Stable machine-readable code: no tenant context bound (REQ-TA-005).
pub const CODE_TENANT_REQUIRED: &str = "TENANT_REQUIRED";
/// Stable machine-readable code: cross-tenant access attempt.
pub const CODE_TENANT_FORBIDDEN: &str = "TENANT_FORBIDDEN";
/// Stable machine-readable code: per-tenant limit hit.
pub const CODE_QUOTA_EXCEEDED: &str = "QUOTA_EXCEEDED";
/// Stable machine-readable code: embedding model version conflict.
pub const CODE_EMBEDDING_MODEL_MISMATCH: &str = "EMBEDDING_MODEL_MISMATCH";
/// Stable machine-readable code: search limit too high.
pub const CODE_TOP_K_EXCEEDED: &str = "TOP_K_EXCEEDED";
/// Stable machine-readable code: search query missing workspace (REQ-MR-006).
pub const CODE_WORKSPACE_REQUIRED: &str = "WORKSPACE_REQUIRED";
/// Stable machine-readable code: requested chunk doesn't exist (REQ-MR-005).
pub const CODE_CHUNK_NOT_FOUND: &str = "CHUNK_NOT_FOUND";
/// Stable machine-readable code: embed batch too large.
pub const CODE_RESOURCE_EXHAUSTED: &str = "RESOURCE_EXHAUSTED";
/// Stable machine-readable code: backup storage full.
pub const CODE_CAPACITY_EXCEEDED: &str = "CAPACITY_EXCEEDED";
/// Stable machine-readable code: uncategorized (logged + structured).
pub const CODE_INTERNAL: &str = "INTERNAL";
/// Stable machine-readable code: generic not found.
pub const CODE_NOT_FOUND: &str = "NOT_FOUND";
/// Stable machine-readable code: generic invalid input.
pub const CODE_INVALID_INPUT: &str = "INVALID_INPUT";
/// Stable machine-readable code: duplicate creation.
pub const CODE_ALREADY_EXISTS: &str = "ALREADY_EXISTS";
/// Stable machine-readable code: credentials wrong (uniform — no
/// tenant-existence leak, REQ-TA-006).
pub const CODE_AUTH_FAILED: &str = "AUTH_FAILED";
/// Stable machine-readable code: file/network failure.
pub const CODE_IO: &str = "IO";
/// Stable machine-readable code: document parsing failure.
pub const CODE_PARSE: &str = "PARSE";
/// Stable machine-readable code: model inference error.
pub const CODE_EMBEDDING_FAILED: &str = "EMBEDDING_FAILED";
/// Stable machine-readable code: backup checksum/decrypt failed (REQ-ML-005).
pub const CODE_BACKUP_CORRUPT: &str = "BACKUP_CORRUPT";
/// Stable machine-readable code: backup schema version mismatch.
pub const CODE_BACKUP_VERSION: &str = "BACKUP_VERSION";
/// Stable machine-readable code: anydoc exceeded 60s.
pub const CODE_SUBPROCESS_TIMEOUT: &str = "SUBPROCESS_TIMEOUT";
/// Stable machine-readable code: anydoc output > 50MB.
pub const CODE_SUBPROCESS_STDOUT_OVERFLOW: &str = "SUBPROCESS_STDOUT_OVERFLOW";
/// Stable machine-readable code: bash metacharacters in path.
pub const CODE_SUBPROCESS_ARGV_INVALID: &str = "SUBPROCESS_ARGV_INVALID";

/// Memento domain error. Every variant carries a stable code and a
/// deterministic CLI exit code (see module docs).
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    /// No tenant context bound to this execution.
    #[error("no tenant context bound to this execution")]
    TenantRequired,
    /// Operation attempts to cross the tenant boundary.
    #[error("operation crosses the tenant boundary")]
    TenantForbidden,
    /// Per-tenant limit hit.
    #[error("per-tenant quota exceeded: {message}")]
    QuotaExceeded { message: String },
    /// Embedding model version conflict.
    #[error("embedding model mismatch: expected {expected}, found {found}")]
    EmbeddingModelMismatch { expected: String, found: String },
    /// Search limit too high.
    #[error("top_k {requested} exceeds the maximum of {max}")]
    TopKExceeded { requested: usize, max: usize },
    /// Search query missing workspace.
    #[error("search requires a workspace")]
    WorkspaceRequired,
    /// Requested chunk doesn't exist.
    #[error("chunk {id} not found")]
    ChunkNotFound { id: ChunkId },
    /// Embed batch too large.
    #[error("resource exhausted: {message}")]
    ResourceExhausted { message: String },
    /// Backup storage full.
    #[error("backup capacity exceeded: {message}")]
    CapacityExceeded { message: String },
    /// Uncategorized internal failure (logged + structured).
    #[error("internal error: {message}")]
    Internal { message: String },
    /// Generic not found.
    #[error("{what} not found")]
    NotFound { what: String },
    /// Generic invalid input.
    #[error("invalid input: {message}")]
    InvalidInput { message: String },
    /// Duplicate creation.
    #[error("already exists: {message}")]
    AlreadyExists { message: String },
    /// Credentials wrong.
    #[error("authentication failed")]
    AuthFailed,
    /// File/network failure.
    #[error("io error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },
    /// Document parsing failure.
    #[error("document parse failed: {message}")]
    Parse { message: String },
    /// Model inference error.
    #[error("embedding failed: {message}")]
    EmbeddingFailed { message: String },
    /// Backup checksum/decrypt failed.
    #[error("backup is corrupt: {reason}")]
    BackupCorrupt { reason: String },
    /// Backup schema version mismatch.
    #[error("backup schema version mismatch: found {found}, expected {expected}")]
    BackupVersion { found: String, expected: String },
    /// anydoc subprocess exceeded 60s.
    #[error("subprocess timed out after 60s: {command}")]
    SubprocessTimeout { command: String },
    /// anydoc stdout > 50MB.
    #[error("subprocess stdout exceeded 50MB: {bytes} bytes")]
    SubprocessStdoutOverflow { bytes: u64 },
    /// Bash metacharacters in path (argv rejected).
    #[error("subprocess argv rejected: {detail}")]
    SubprocessArgvInvalid { detail: String },
}

impl DomainError {
    /// Stable machine-readable code (stability contract in module docs).
    pub fn code(&self) -> &'static str {
        match self {
            DomainError::TenantRequired => CODE_TENANT_REQUIRED,
            DomainError::TenantForbidden => CODE_TENANT_FORBIDDEN,
            DomainError::QuotaExceeded { .. } => CODE_QUOTA_EXCEEDED,
            DomainError::EmbeddingModelMismatch { .. } => CODE_EMBEDDING_MODEL_MISMATCH,
            DomainError::TopKExceeded { .. } => CODE_TOP_K_EXCEEDED,
            DomainError::WorkspaceRequired => CODE_WORKSPACE_REQUIRED,
            DomainError::ChunkNotFound { .. } => CODE_CHUNK_NOT_FOUND,
            DomainError::ResourceExhausted { .. } => CODE_RESOURCE_EXHAUSTED,
            DomainError::CapacityExceeded { .. } => CODE_CAPACITY_EXCEEDED,
            DomainError::Internal { .. } => CODE_INTERNAL,
            DomainError::NotFound { .. } => CODE_NOT_FOUND,
            DomainError::InvalidInput { .. } => CODE_INVALID_INPUT,
            DomainError::AlreadyExists { .. } => CODE_ALREADY_EXISTS,
            DomainError::AuthFailed => CODE_AUTH_FAILED,
            DomainError::Io { .. } => CODE_IO,
            DomainError::Parse { .. } => CODE_PARSE,
            DomainError::EmbeddingFailed { .. } => CODE_EMBEDDING_FAILED,
            DomainError::BackupCorrupt { .. } => CODE_BACKUP_CORRUPT,
            DomainError::BackupVersion { .. } => CODE_BACKUP_VERSION,
            DomainError::SubprocessTimeout { .. } => CODE_SUBPROCESS_TIMEOUT,
            DomainError::SubprocessStdoutOverflow { .. } => CODE_SUBPROCESS_STDOUT_OVERFLOW,
            DomainError::SubprocessArgvInvalid { .. } => CODE_SUBPROCESS_ARGV_INVALID,
        }
    }

    /// Deterministic process exit code for CLI integration (REQ-CL-005).
    /// Never change an existing mapping: the CLI exit-code matrix depends on it.
    pub fn exit_code(&self) -> i32 {
        match self {
            DomainError::Internal { .. } => 1,
            DomainError::InvalidInput { .. } => 2,
            DomainError::AlreadyExists { .. } => 3,
            DomainError::AuthFailed => 4,
            DomainError::Io { .. } => 5,
            DomainError::Parse { .. } => 6,
            DomainError::EmbeddingFailed { .. } => 7,
            DomainError::BackupCorrupt { .. } => 8,
            DomainError::BackupVersion { .. } => 9,
            DomainError::TenantRequired => 10,
            DomainError::TenantForbidden => 11,
            DomainError::QuotaExceeded { .. } => 12,
            DomainError::EmbeddingModelMismatch { .. } => 13,
            DomainError::TopKExceeded { .. } => 14,
            DomainError::WorkspaceRequired => 15,
            DomainError::ResourceExhausted { .. } => 16,
            DomainError::CapacityExceeded { .. } => 17,
            DomainError::NotFound { .. } => 20,
            DomainError::ChunkNotFound { .. } => 21,
            DomainError::SubprocessTimeout { .. } => 30,
            DomainError::SubprocessStdoutOverflow { .. } => 31,
            DomainError::SubprocessArgvInvalid { .. } => 32,
        }
    }
}

impl From<DomainError> for std::io::Error {
    fn from(err: DomainError) -> Self {
        // The stable code rides in the io error string so callers can match it.
        std::io::Error::other(format!("[{}] {err}", err.code()))
    }
}

impl From<DomainError> for Value {
    fn from(err: DomainError) -> Self {
        // Structured error for MCP surfaces (REQ-MS-005): stable code + message.
        json!({
            "code": err.code(),
            "message": err.to_string(),
            "exit_code": err.exit_code(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// One constructed value per variant, so tests enumerate the whole
    /// taxonomy without relying on any single variant's payload.
    fn all_variants() -> Vec<DomainError> {
        vec![
            DomainError::TenantRequired,
            DomainError::TenantForbidden,
            DomainError::QuotaExceeded {
                message: "quota".into(),
            },
            DomainError::EmbeddingModelMismatch {
                expected: "e5".into(),
                found: "bge".into(),
            },
            DomainError::TopKExceeded {
                requested: 200,
                max: 100,
            },
            DomainError::WorkspaceRequired,
            DomainError::ChunkNotFound { id: ChunkId::new() },
            DomainError::ResourceExhausted {
                message: "batch".into(),
            },
            DomainError::CapacityExceeded {
                message: "disk".into(),
            },
            DomainError::Internal {
                message: "boom".into(),
            },
            DomainError::NotFound {
                what: "item".into(),
            },
            DomainError::InvalidInput {
                message: "bad".into(),
            },
            DomainError::AlreadyExists {
                message: "dup".into(),
            },
            DomainError::AuthFailed,
            DomainError::Io {
                source: std::io::Error::other("io"),
            },
            DomainError::Parse {
                message: "corrupt".into(),
            },
            DomainError::EmbeddingFailed {
                message: "onnx".into(),
            },
            DomainError::BackupCorrupt {
                reason: "checksum".into(),
            },
            DomainError::BackupVersion {
                found: "2".into(),
                expected: "1".into(),
            },
            DomainError::SubprocessTimeout {
                command: "anydoc".into(),
            },
            DomainError::SubprocessStdoutOverflow {
                bytes: 51 * 1024 * 1024,
            },
            DomainError::SubprocessArgvInvalid {
                detail: "metachar".into(),
            },
        ]
    }

    #[test]
    fn codes_are_stable() {
        // Every variant maps to a hard-coded string literal (no format!).
        // These values are the external contract: never change them.
        let cases = [
            (DomainError::TenantRequired, "TENANT_REQUIRED"),
            (DomainError::TenantForbidden, "TENANT_FORBIDDEN"),
            (
                DomainError::QuotaExceeded {
                    message: String::new(),
                },
                "QUOTA_EXCEEDED",
            ),
            (
                DomainError::EmbeddingModelMismatch {
                    expected: String::new(),
                    found: String::new(),
                },
                "EMBEDDING_MODEL_MISMATCH",
            ),
            (
                DomainError::TopKExceeded {
                    requested: 1,
                    max: 1,
                },
                "TOP_K_EXCEEDED",
            ),
            (DomainError::WorkspaceRequired, "WORKSPACE_REQUIRED"),
            (
                DomainError::ChunkNotFound { id: ChunkId::new() },
                "CHUNK_NOT_FOUND",
            ),
            (
                DomainError::ResourceExhausted {
                    message: String::new(),
                },
                "RESOURCE_EXHAUSTED",
            ),
            (
                DomainError::CapacityExceeded {
                    message: String::new(),
                },
                "CAPACITY_EXCEEDED",
            ),
            (
                DomainError::Internal {
                    message: String::new(),
                },
                "INTERNAL",
            ),
            (
                DomainError::NotFound {
                    what: String::new(),
                },
                "NOT_FOUND",
            ),
            (
                DomainError::InvalidInput {
                    message: String::new(),
                },
                "INVALID_INPUT",
            ),
            (
                DomainError::AlreadyExists {
                    message: String::new(),
                },
                "ALREADY_EXISTS",
            ),
            (DomainError::AuthFailed, "AUTH_FAILED"),
            (
                DomainError::Io {
                    source: std::io::Error::other(""),
                },
                "IO",
            ),
            (
                DomainError::Parse {
                    message: String::new(),
                },
                "PARSE",
            ),
            (
                DomainError::EmbeddingFailed {
                    message: String::new(),
                },
                "EMBEDDING_FAILED",
            ),
            (
                DomainError::BackupCorrupt {
                    reason: String::new(),
                },
                "BACKUP_CORRUPT",
            ),
            (
                DomainError::BackupVersion {
                    found: String::new(),
                    expected: String::new(),
                },
                "BACKUP_VERSION",
            ),
            (
                DomainError::SubprocessTimeout {
                    command: String::new(),
                },
                "SUBPROCESS_TIMEOUT",
            ),
            (
                DomainError::SubprocessStdoutOverflow { bytes: 0 },
                "SUBPROCESS_STDOUT_OVERFLOW",
            ),
            (
                DomainError::SubprocessArgvInvalid {
                    detail: String::new(),
                },
                "SUBPROCESS_ARGV_INVALID",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.code(), expected, "stable code for {err:?}");
        }
    }

    #[test]
    fn codes_are_unique() {
        let codes: HashSet<&str> = all_variants().iter().map(DomainError::code).collect();
        assert_eq!(
            codes.len(),
            all_variants().len(),
            "every variant must have its own code (no collisions)"
        );
    }

    #[test]
    fn exit_codes_match_cli_contract() {
        // REQ-CL-005: deterministic exit-code matrix shared with the CLI.
        let cases = [
            (
                DomainError::Internal {
                    message: String::new(),
                },
                1,
            ),
            (
                DomainError::InvalidInput {
                    message: String::new(),
                },
                2,
            ),
            (
                DomainError::AlreadyExists {
                    message: String::new(),
                },
                3,
            ),
            (DomainError::AuthFailed, 4),
            (
                DomainError::Io {
                    source: std::io::Error::other(""),
                },
                5,
            ),
            (
                DomainError::Parse {
                    message: String::new(),
                },
                6,
            ),
            (
                DomainError::EmbeddingFailed {
                    message: String::new(),
                },
                7,
            ),
            (
                DomainError::BackupCorrupt {
                    reason: String::new(),
                },
                8,
            ),
            (
                DomainError::BackupVersion {
                    found: String::new(),
                    expected: String::new(),
                },
                9,
            ),
            (DomainError::TenantRequired, 10),
            (DomainError::TenantForbidden, 11),
            (
                DomainError::QuotaExceeded {
                    message: String::new(),
                },
                12,
            ),
            (
                DomainError::EmbeddingModelMismatch {
                    expected: String::new(),
                    found: String::new(),
                },
                13,
            ),
            (
                DomainError::TopKExceeded {
                    requested: 1,
                    max: 1,
                },
                14,
            ),
            (DomainError::WorkspaceRequired, 15),
            (
                DomainError::ResourceExhausted {
                    message: String::new(),
                },
                16,
            ),
            (
                DomainError::CapacityExceeded {
                    message: String::new(),
                },
                17,
            ),
            (
                DomainError::NotFound {
                    what: String::new(),
                },
                20,
            ),
            (DomainError::ChunkNotFound { id: ChunkId::new() }, 21),
            (
                DomainError::SubprocessTimeout {
                    command: String::new(),
                },
                30,
            ),
            (DomainError::SubprocessStdoutOverflow { bytes: 0 }, 31),
            (
                DomainError::SubprocessArgvInvalid {
                    detail: String::new(),
                },
                32,
            ),
        ];
        let mut seen: HashSet<i32> = HashSet::new();
        for (err, expected) in cases {
            let code = err.exit_code();
            assert_eq!(code, expected, "exit code for {err:?}");
            assert!(seen.insert(code), "exit code {code} duplicated");
            assert!(
                (1..=255).contains(&code),
                "exit code {code} outside OS range"
            );
        }
        // The whole taxonomy is covered by the matrix above.
        assert_eq!(seen.len(), all_variants().len());
    }

    #[test]
    fn structured_value_keeps_code() {
        // MCP structured errors (REQ-MS-005): the JSON payload keeps the
        // stable code and the deterministic exit code.
        let v: Value = DomainError::ChunkNotFound { id: ChunkId::new() }.into();
        assert_eq!(v["code"], "CHUNK_NOT_FOUND");
        assert_eq!(v["exit_code"], 21);
    }
}
