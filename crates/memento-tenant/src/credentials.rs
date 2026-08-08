//! Credential store: Argon2id hashing at rest (design D3, REQ-TA-006).
//!
//! Storage layout (design D8):
//!
//! ```text
//! <root>/db/tenants/<tenant_id>/auth/credentials.toml   # hash = "<phc>", 0600
//! <root>/db/tenants/<tenant_id>/config.toml             # [tenant] name = "..." (provisioning metadata)
//! ```
//!
//! Tokens follow D3: `memo_<tenant_id>_<48×base62>`. The store keeps ONLY the
//! Argon2id PHC hash (m=19MiB, t=2, p=1); the plaintext token is returned to
//! the caller exactly once (provisioning/rotation) and is never written to
//! disk, logged, or traced. Validation failures never distinguish "unknown
//! tenant" from "wrong key" — callers observe a uniform failure (REQ-TA-006).

use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};
use argon2::{Algorithm, Argon2, Params, Version};
use memento_domain::{DomainError, TenantId};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// D3 memory cost: 19 MiB (KiB units — argon2's own default, stated explicitly
/// so the acceptance test can pin it).
const ARGON2_M_COST: u32 = 19 * 1024;
/// D3 time cost.
const ARGON2_T_COST: u32 = 2;
/// D3 parallelism.
const ARGON2_P_COST: u32 = 1;
/// Argon2id output length (32-byte derived key).
const ARGON2_OUTPUT_LEN: usize = 32;

/// Token prefix (D3): `memo_<tenant_id>_<48×base62>`.
pub const TOKEN_PREFIX: &str = "memo_";
/// Secret length in base62 characters (D3).
pub const SECRET_LEN: usize = 48;
/// Base62 alphabet (D3): 0-9, A-Z, a-z — URL-safe, no look-alike pairs.
pub(crate) const BASE62: &[u8; 62] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// A bearer API key (`memo_<tid>_<48×base62>`, design D3). Shown to the
/// operator exactly once; only its Argon2id hash exists at rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// The full `memo_...` token.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the key into the underlying token string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for ApiKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// On-disk credential store for one process-bound tenant (REQ-TA-001/002).
///
/// The store never holds plaintext tokens: [`CredentialStore::create_tenant`]
/// and [`CredentialStore::rotate`] hash before persisting and return the
/// plaintext key to the caller only.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    /// Storage root (`~/.memento` in production; a tempdir in tests).
    root: PathBuf,
}

impl CredentialStore {
    /// Open the store rooted at `root` (design D8 layout).
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// `db/tenants/<tenant_id>` for the tenant (design D8).
    pub fn tenant_dir(&self, tenant_id: &TenantId) -> PathBuf {
        self.root
            .join("db")
            .join("tenants")
            .join(tenant_id.to_string())
    }

    /// `auth/credentials.toml` — the Argon2id hash (REQ-TA-006), 0600.
    pub fn credentials_path(&self, tenant_id: &TenantId) -> PathBuf {
        self.tenant_dir(tenant_id)
            .join("auth")
            .join("credentials.toml")
    }

    /// `config.toml` — per-tenant configuration (REQ-TA-007). Holds only
    /// provisioning metadata in this batch (`[tenant] name`); the retention
    /// override lands with the application layer (T-063).
    pub fn config_path(&self, tenant_id: &TenantId) -> PathBuf {
        self.tenant_dir(tenant_id).join("config.toml")
    }

    /// Provision a tenant: generate the token, persist only its Argon2id hash,
    /// and return the plaintext key to the caller exactly once.
    pub fn create_tenant(&self, name: &str) -> Result<(TenantId, ApiKey), DomainError> {
        let tenant_id = TenantId::new();
        let key = self.generate_key(&tenant_id)?;
        let phc = hash_key(key.as_str())?;
        let auth_dir = self.tenant_dir(&tenant_id).join("auth");
        fs::create_dir_all(&auth_dir)?;
        self.write_private(
            &self.credentials_path(&tenant_id),
            &format!("hash = \"{phc}\"\n"),
        )?;
        self.write_private(
            &self.config_path(&tenant_id),
            &format!("[tenant]\nname = {}\n", toml_basic_string(name)),
        )?;
        tracing::info!(tenant_id = %tenant_id, "tenant provisioned; credential stored as Argon2id hash");
        Ok((tenant_id, key))
    }

    /// Read the stored PHC hash for a tenant. Errors never distinguish a
    /// missing file from a corrupt one at this boundary; the resolver maps
    /// every failure to the uniform auth error (REQ-TA-006).
    pub fn load_hash(&self, tenant_id: &TenantId) -> Result<String, DomainError> {
        let raw = fs::read_to_string(self.credentials_path(tenant_id))?;
        parse_hash_line(&raw)
    }

    /// Constant-time Argon2id verification of `key` against a stored PHC hash.
    /// Returns `false` for any malformed hash or mismatched key — callers turn
    /// this into the uniform auth error, so no existence signal leaks.
    pub fn verify_key(&self, key: &str, phc_hash: &str) -> bool {
        match PasswordHash::new(phc_hash) {
            Ok(parsed) => Argon2::default()
                .verify_password(key.as_bytes(), &parsed)
                .is_ok(),
            Err(_) => false,
        }
    }

    /// Rotate the tenant token: replace the stored hash with a fresh key's
    /// hash. The old token dies immediately (its hash is gone). The running
    /// process keeps its already-bound context — a restart is required for
    /// the new token to take effect (risk R9, documented in [`rotate`]).
    pub fn rotate(&self, tenant_id: &TenantId) -> Result<ApiKey, DomainError> {
        // Existence check first: unknown tenants fail uniformly (REQ-TA-006).
        if self.load_hash(tenant_id).is_err() {
            return Err(DomainError::AuthFailed);
        }
        let key = self.generate_key(tenant_id)?;
        let phc = hash_key(key.as_str())?;
        self.write_private(
            &self.credentials_path(tenant_id),
            &format!("hash = \"{phc}\"\n"),
        )?;
        Ok(key)
    }

    /// Generate a D3 token: `memo_<tid>_<48×base62>` with an OS-entropy
    /// rejection-sampled base62 secret (no modulo bias).
    fn generate_key(&self, tenant_id: &TenantId) -> Result<ApiKey, DomainError> {
        let mut secret = [0u8; SECRET_LEN];
        for slot in secret.iter_mut() {
            *slot = BASE62[sample_base62()?];
        }
        let secret = std::str::from_utf8(&secret).map_err(|e| DomainError::Internal {
            message: e.to_string(),
        })?;
        Ok(ApiKey(format!("{TOKEN_PREFIX}{tenant_id}_{secret}")))
    }

    /// Write `contents` to `path` privately: temp file + fsync + atomic
    /// rename, mode 0600 (unix) before the rename so the final path never
    /// exists with looser permissions.
    fn write_private(&self, path: &Path, contents: &str) -> Result<(), DomainError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        {
            let mut file = fs::File::create(&tmp)?;
            set_private_perms(&file)?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Hash a token with Argon2id (D3: m=19MiB, t=2, p=1) into a PHC string with a
/// fresh 16-byte salt. Salts are random per hash, so the same token hashes
/// differently on every call (and every rotation).
pub fn hash_key(key: &str) -> Result<String, DomainError> {
    let params = Params::new(
        ARGON2_M_COST,
        ARGON2_T_COST,
        ARGON2_P_COST,
        Some(ARGON2_OUTPUT_LEN),
    )
    .map_err(|e| DomainError::Internal {
        message: format!("invalid Argon2 params: {e}"),
    })?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let salt = SaltString::generate(&mut OsRng);
    argon2
        .hash_password(key.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| DomainError::Internal {
            message: format!("Argon2id hashing failed: {e}"),
        })
}

/// Sample one base62 index from the OS CSPRNG with rejection sampling
/// (uniform over 62; no modulo bias).
fn sample_base62() -> Result<usize, DomainError> {
    use argon2::password_hash::rand_core::RngCore;
    const MOD: u32 = 62;
    const LIMIT: u32 = u32::MAX - (u32::MAX % MOD);
    loop {
        let value = OsRng.next_u32();
        if value < LIMIT {
            return Ok((value % MOD) as usize);
        }
    }
}

/// Parse a `hash = "<phc>"` line. The PHC charset ([A-Za-z0-9$+/=:.,-]) needs
/// no TOML escaping, so the parser is a strict prefix/suffix strip.
fn parse_hash_line(raw: &str) -> Result<String, DomainError> {
    let line = raw.trim();
    let rest = line
        .strip_prefix("hash = ")
        .filter(|r| r.starts_with('"') && r.ends_with('"') && r.len() > 2)
        .map(|r| &r[1..r.len() - 1])
        .ok_or_else(|| DomainError::InvalidInput {
            message: "corrupt credential store".into(),
        })?;
    if !rest.starts_with("$argon2") {
        return Err(DomainError::InvalidInput {
            message: "corrupt credential store".into(),
        });
    }
    Ok(rest.to_string())
}

/// Escape `s` as a TOML basic string literal (name metadata; the hash line is
/// written verbatim because PHC strings need no escaping).
fn toml_basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Mode 0600 on unix. Windows has no mode bits: the file lives under the
/// user profile (user-private by default) and is never marked world-readable;
/// this is documented in the threat-model (T-113).
#[cfg(unix)]
fn set_private_perms(file: &fs::File) -> Result<(), DomainError> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(DomainError::from)
}

#[cfg(not(unix))]
fn set_private_perms(_file: &fs::File) -> Result<(), DomainError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A fresh store on a tempdir, isolated per test.
    fn test_store() -> (TempDir, CredentialStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn key_creation_roundtrip() {
        let (_dir, store) = test_store();
        let (tenant_id, key) = store.create_tenant("dev").expect("provision");

        // Format per D3: memo_<tid>_<48 base62>.
        let token = key.as_str();
        let mut parts = token.split('_');
        assert_eq!(parts.next(), Some("memo"));
        let tid = tenant_id.to_string();
        assert_eq!(parts.next(), Some(tid.as_str()));
        let secret = parts.next().expect("secret");
        assert_eq!(secret.len(), SECRET_LEN);
        assert!(secret.bytes().all(|b| BASE62.contains(&b)), "base62 only");
        assert!(parts.next().is_none(), "no trailing parts");

        // The stored hash verifies the token.
        let phc = store.load_hash(&tenant_id).expect("hash on disk");
        assert!(store.verify_key(token, &phc));
    }

    #[test]
    fn on_disk_inspection_finds_only_hashes() {
        // REQ-TA-006: hashed at rest — the credential file must contain a PHC
        // hash and never the plaintext token.
        let (dir, store) = test_store();
        let (tenant_id, key) = store.create_tenant("dev").expect("provision");

        let cred = std::fs::read_to_string(store.credentials_path(&tenant_id)).expect("read");
        assert!(cred.contains("$argon2id$"), "PHC hash present: {cred}");
        assert!(!cred.contains(key.as_str()), "plaintext token on disk");
        // The secret portion must not appear either.
        let secret = key.as_str().split('_').nth(2).unwrap();
        assert!(!cred.contains(secret), "secret fragment on disk");

        // config.toml holds provisioning metadata only — no key material.
        let config = std::fs::read_to_string(store.config_path(&tenant_id)).expect("read");
        assert!(
            config.contains("name = \"dev\""),
            "name persisted: {config}"
        );
        assert!(!config.contains(secret), "secret in config.toml");

        // Every file under the tenant dir is hash/metadata-only.
        let mut found = String::new();
        for entry in walkdir_lite(dir.path()) {
            found.push_str(&std::fs::read_to_string(&entry).unwrap_or_default());
        }
        assert!(
            !found.contains(key.as_str()),
            "token leaked into tenant dir"
        );
    }

    #[test]
    fn hash_uses_argon2id_d3_params() {
        // Pin the D3 configuration in the PHC string itself:
        // $argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>
        let (_dir, store) = test_store();
        let (tenant_id, _key) = store.create_tenant("dev").expect("provision");
        let phc = store.load_hash(&tenant_id).expect("hash");

        let mut fields = phc.split('$');
        assert_eq!(fields.next(), Some(""));
        assert_eq!(fields.next(), Some("argon2id"), "algorithm: {phc}");
        assert_eq!(fields.next(), Some("v=19"), "version: {phc}");
        assert_eq!(fields.next(), Some("m=19456,t=2,p=1"), "params: {phc}");
        assert_eq!(fields.next().map(str::len), Some(22), "16-byte b64 salt");
        assert_eq!(fields.next().map(str::len), Some(43), "32-byte b64 hash");
        assert!(fields.next().is_none(), "no trailing fields");
    }

    #[test]
    fn wrong_key_rejected() {
        let (_dir, store) = test_store();
        let (tenant_id, key) = store.create_tenant("dev").expect("provision");
        let phc = store.load_hash(&tenant_id).expect("hash");

        assert!(store.verify_key(key.as_str(), &phc));

        // Flip the last base62 character — a different key entirely.
        let mut wrong = key.into_string();
        let last = wrong.pop().unwrap();
        let replacement = if last == '0' { '1' } else { '0' };
        wrong.push(replacement);
        assert!(!store.verify_key(&wrong, &phc));
    }

    #[test]
    fn rotation_invalidates_old_hash() {
        // Store-level rotation: the on-disk hash changes, so the old token no
        // longer verifies against the new hash.
        let (dir, store) = test_store();
        let (tenant_id, old_key) = store.create_tenant("dev").expect("provision");
        let old_phc = store.load_hash(&tenant_id).expect("old hash");

        let new_key = store.rotate(&tenant_id).expect("rotate");
        let new_phc = store.load_hash(&tenant_id).expect("new hash");

        assert_ne!(old_key, new_key);
        assert_ne!(old_phc, new_phc, "hash replaced on disk");
        assert!(!store.verify_key(old_key.as_str(), &new_phc));
        assert!(store.verify_key(new_key.as_str(), &new_phc));

        // No plaintext anywhere under the tenant dir.
        let mut found = String::new();
        for entry in walkdir_lite(dir.path()) {
            found.push_str(&std::fs::read_to_string(&entry).unwrap_or_default());
        }
        assert!(!found.contains(old_key.as_str()));
        assert!(!found.contains(new_key.as_str()));
    }

    #[test]
    fn rotate_unknown_tenant_is_uniform() {
        // REQ-TA-006: rotation must not confirm tenant existence either.
        let (_dir, store) = test_store();
        let err = store.rotate(&TenantId::new()).expect_err("unknown tenant");
        assert_eq!(err.code(), "AUTH_FAILED");
    }

    #[cfg(unix)]
    #[test]
    fn credentials_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, store) = test_store();
        let (tenant_id, _key) = store.create_tenant("dev").expect("provision");
        for path in [
            store.credentials_path(&tenant_id),
            store.config_path(&tenant_id),
        ] {
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "mode of {}", path.display());
        }
    }

    #[cfg(windows)]
    #[test]
    fn credentials_file_is_private_on_windows() {
        // Windows has no mode bits; assert the file is not flagged
        // read-only (so rotation can rewrite it) and lives under the
        // user profile, which is user-private by default.
        let (_dir, store) = test_store();
        let (tenant_id, _key) = store.create_tenant("dev").expect("provision");
        let path = store.credentials_path(&tenant_id);
        let perms = std::fs::metadata(&path).expect("meta").permissions();
        assert!(!perms.readonly(), "not marked readonly at {path:?}");
    }

    #[test]
    fn tenants_are_isolated() {
        let (_dir, store) = test_store();
        let (t1, k1) = store.create_tenant("a").expect("t1");
        let (t2, k2) = store.create_tenant("b").expect("t2");
        assert_ne!(t1, t2);

        let phc1 = store.load_hash(&t1).expect("t1 hash");
        assert!(store.verify_key(k1.as_str(), &phc1));
        assert!(!store.verify_key(k2.as_str(), &phc1), "cross-tenant key");
    }

    #[test]
    fn same_key_hashes_differently_each_call() {
        // Fresh salt per hash: two hashes of the same token must differ.
        let (_dir, store) = test_store();
        // Exactly 48 base62 chars: 10 digits + 26 uppercase + 12 lowercase.
        let key = "memo_00000000-0000-7000-8000-000000000000_0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijkl";
        assert_eq!(key.len(), TOKEN_PREFIX.len() + 36 + 1 + SECRET_LEN);
        let h1 = hash_key(key).expect("h1");
        let h2 = hash_key(key).expect("h2");
        assert_ne!(h1, h2);
        assert!(store.verify_key(key, &h1));
        assert!(store.verify_key(key, &h2));
    }

    #[test]
    fn corrupt_hash_line_is_an_error() {
        let (_dir, store) = test_store();
        let (tenant_id, _key) = store.create_tenant("dev").expect("provision");
        // Corrupt the credential file.
        std::fs::write(store.credentials_path(&tenant_id), "hash = \"not-a-phc\"\n").unwrap();
        let err = store.load_hash(&tenant_id).expect_err("corrupt");
        assert_eq!(err.code(), "INVALID_INPUT");
    }

    /// Minimal recursive walker (tempdirs are small; avoids a walkdir dep).
    fn walkdir_lite(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read_dir") {
                let entry = entry.expect("entry");
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
    }
}
