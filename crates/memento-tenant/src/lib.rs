//! memento-tenant — Memento RS tenancy and credential resolution (REQ-TA-*).
//!
//! Auth flow (design D3 / Auth Flow section):
//!
//! 1. **Provisioning** — `memento tenant create --name X` (CLI, T-082) calls
//!    [`CredentialStore::create_tenant`]: a `memo_<tid>_<48×base62>` token is
//!    generated, only its Argon2id hash (m=19MiB, t=2, p=1) is persisted at
//!    `db/tenants/<tid>/auth/credentials.toml` (0600), and the plaintext is
//!    printed exactly once (REQ-TA-006: hash-only at rest).
//! 2. **Startup binding** — the process starts with `MEMENTO_TOKEN` +
//!    `MEMENTO_AGENT_ID`; the resolver (T-051) parses the token, verifies the
//!    hash, and binds the opaque `TenantContext` for the process lifetime
//!    (REQ-TA-002/003/005). Every auth failure is uniform — unknown tenant and
//!    wrong key are indistinguishable (REQ-TA-006).
//! 3. **Rotation** — `memento tenant rotate-token` (T-082) replaces the hash;
//!    the old token dies immediately, a restart is required (risk R9).

mod credentials;

pub use credentials::{ApiKey, CredentialStore, SECRET_LEN, TOKEN_PREFIX, hash_key};

use memento_domain::DomainError;
use std::path::PathBuf;

/// Production storage root (`~/.memento`, design D8).
pub fn default_root() -> Result<PathBuf, DomainError> {
    dirs::home_dir()
        .map(|home| home.join(".memento"))
        .ok_or_else(|| DomainError::InvalidInput {
            message: "cannot determine the home directory".into(),
        })
}
