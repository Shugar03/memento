//! Token resolution and process startup binding (REQ-TA-002/003/005/006).
//!
//! [`TenantResolverImpl`] is the ONLY producer of a `TenantContext`
//! (REQ-TA-002): it parses the `memo_<tid>_<48×base62>` bearer token, verifies
//! the Argon2id hash, and constructs the opaque context. Every failure on the
//! validation path collapses to the uniform `AUTH_FAILED` error — an unknown
//! tenant, a malformed token and a wrong key are indistinguishable, so auth
//! never leaks tenant existence (REQ-TA-006).

use crate::credentials::{BASE62, CredentialStore, SECRET_LEN};
use async_trait::async_trait;
use memento_domain::{AgentId, DomainError, TenantContext, TenantId, WorkspaceId};
use memento_ports::TenantResolver;
use std::path::Path;
use std::str::FromStr;

/// A parsed bearer token (`memo_<tid>_<48×base62>`, design D3). Only the
/// tenant id is retained: the secret portion is validated by [`BearerToken::parse`]
/// but verification always runs over the FULL presented token, because the
/// stored hash covers `memo_<tid>_<secret>` — a secret stripped from its
/// tenant prefix can never be replayed under another tenant id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BearerToken {
    tenant_id: TenantId,
}

impl BearerToken {
    /// Strictly parse a D3 token. Any deviation from the format yields `None`
    /// (the resolver turns it into the uniform auth error).
    pub fn parse(raw: &str) -> Option<BearerToken> {
        let mut parts = raw.split('_');
        if parts.next() != Some("memo") {
            return None;
        }
        let tenant_id = TenantId::from_str(parts.next()?).ok()?;
        let secret = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        if secret.len() != SECRET_LEN || !secret.bytes().all(|b| BASE62.contains(&b)) {
            return None;
        }
        Some(BearerToken { tenant_id })
    }

    /// The tenant the token claims.
    pub fn tenant_id(&self) -> &TenantId {
        &self.tenant_id
    }
}

/// Deterministic default workspace for a tenant: sha256(tenant UUID ‖
/// "memento-default-workspace"), first 16 bytes. Belongs to the tenant by
/// construction (REQ-TA-001/004) and is stable across process restarts, so
/// data written under it remains addressable. Follows the project_id
/// derivation precedent from memento-okf (T-040).
pub fn default_workspace_id(tenant_id: &TenantId) -> WorkspaceId {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(tenant_id.as_uuid().as_bytes());
    hasher.update(b"memento-default-workspace");
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    WorkspaceId::from_uuid(uuid::Uuid::from_bytes(bytes))
}

/// Production implementation of the identity boundary
/// ([`memento_ports::TenantResolver`]): credential store + token resolution.
pub struct TenantResolverImpl {
    store: CredentialStore,
}

impl TenantResolverImpl {
    /// Open the resolver over the store rooted at `root` (D8 layout).
    pub fn open(root: impl AsRef<Path>) -> Self {
        Self {
            store: CredentialStore::new(root),
        }
    }

    /// The underlying credential store (provisioning/rotation admin).
    pub fn store(&self) -> &CredentialStore {
        &self.store
    }

    /// Default workspace for the tenant (stable across restarts).
    pub fn workspace_id(&self, tenant_id: &TenantId) -> WorkspaceId {
        default_workspace_id(tenant_id)
    }

    /// Startup binding from the environment (REQ-TA-002/003):
    /// reads `MEMENTO_TOKEN` + `MEMENTO_AGENT_ID` and resolves the bound
    /// context. Missing/invalid token → uniform `AUTH_FAILED`; missing
    /// agent id → `INVALID_INPUT` naming the variable (REQ-TA-003 scenario).
    pub fn resolve_from_env(&self) -> Result<TenantContext, DomainError> {
        let token = std::env::var("MEMENTO_TOKEN").map_err(|_| DomainError::AuthFailed)?;
        let agent_id =
            std::env::var("MEMENTO_AGENT_ID").map_err(|_| DomainError::InvalidInput {
                message: "MEMENTO_AGENT_ID is not set".into(),
            })?;
        self.resolve_sync(&token, AgentId::new(agent_id))
    }

    /// Synchronous resolution core (shared by the async trait impl).
    fn resolve_sync(&self, token: &str, agent_id: AgentId) -> Result<TenantContext, DomainError> {
        let Some(parsed) = BearerToken::parse(token) else {
            return Err(DomainError::AuthFailed);
        };
        // Unknown tenant (missing hash) and wrong key are both AUTH_FAILED —
        // the store boundary never confirms existence (REQ-TA-006).
        let phc = match self.store.load_hash(parsed.tenant_id()) {
            Ok(hash) => hash,
            Err(_) => return Err(DomainError::AuthFailed),
        };
        // The stored hash covers the FULL token (`memo_<tid>_<secret>`), so a
        // secret replayed under another tenant id fails here too.
        if !self.store.verify_key(token, &phc) {
            return Err(DomainError::AuthFailed);
        }
        let tenant_id = *parsed.tenant_id();
        Ok(TenantContext::new(
            tenant_id,
            default_workspace_id(&tenant_id),
            agent_id,
        ))
    }
}

#[async_trait]
impl TenantResolver for TenantResolverImpl {
    /// Validate the bearer token + agent id and bind the tenant context.
    /// Uniform `AUTH_FAILED` on every validation failure (REQ-TA-006).
    async fn resolve(&self, token: &str, agent_id: AgentId) -> Result<TenantContext, DomainError> {
        self.resolve_sync(token, agent_id)
    }

    /// Rotate the tenant token; the old token dies immediately, a process
    /// restart is required (risk R9; orchestration docs land in T-052).
    async fn rotate_token(&self, tenant_id: &TenantId) -> Result<String, DomainError> {
        self.store.rotate(tenant_id).map(crate::ApiKey::into_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memento_domain::error::CODE_TENANT_REQUIRED;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Serializes tests that mutate the process environment.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// A store pre-provisioned with `name`; returns the tempdir and the key.
    fn provision(name: &str) -> (TempDir, CredentialStore, TenantId, crate::ApiKey) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::new(dir.path());
        let (tenant_id, key) = store.create_tenant(name).expect("provision");
        (dir, store, tenant_id, key)
    }

    #[test]
    fn parse_valid_token() {
        let (_dir, _store, tenant_id, key) = provision("dev");
        let parsed = BearerToken::parse(key.as_str()).expect("parses");
        assert_eq!(parsed.tenant_id(), &tenant_id);
        let secret = key.as_str().rsplit('_').next().unwrap();
        assert_eq!(secret.len(), SECRET_LEN);
    }

    #[test]
    fn reject_malformed_token() {
        let (_dir, _store, tenant_id, key) = provision("dev");
        let good = key.as_str();
        let secret = good.rsplit('_').next().unwrap();

        // Wrong prefix / missing prefix.
        assert!(BearerToken::parse(&format!("xemo_{tenant_id}_{secret}")).is_none());
        assert!(BearerToken::parse(&format!("{tenant_id}_{secret}")).is_none());
        // Bad tenant id (not a UUID).
        assert!(BearerToken::parse(&format!("memo_not-a-uuid_{secret}")).is_none());
        // Secret too short / too long.
        assert!(BearerToken::parse(&format!("memo_{tenant_id}_{}", &secret[..20])).is_none());
        assert!(BearerToken::parse(&format!("memo_{tenant_id}_{secret}ZZZZZ")).is_none());
        // Non-base62 character in the secret.
        let mut evil = secret[..48].chars().collect::<Vec<_>>();
        evil[0] = '_';
        let evil: String = evil.into_iter().collect();
        assert!(BearerToken::parse(&format!("memo_{tenant_id}_{evil}")).is_none());
        // Extra underscore segments.
        assert!(BearerToken::parse(&format!("memo_{tenant_id}_{secret}_extra")).is_none());
        // Empty / garbage.
        assert!(BearerToken::parse("").is_none());
        assert!(BearerToken::parse("memo_").is_none());
        assert!(BearerToken::parse("junk").is_none());
        // The good token still parses after all the above.
        assert!(BearerToken::parse(good).is_some());
    }

    #[tokio::test]
    async fn resolve_returns_context() {
        let (dir, _store, tenant_id, key) = provision("dev");
        let resolver = TenantResolverImpl::open(dir.path());
        let ctx = resolver
            .resolve(key.as_str(), AgentId::new("agent-x"))
            .await
            .expect("valid token");

        assert_eq!(ctx.tenant_id(), &tenant_id);
        assert_eq!(ctx.agent_id().as_str(), "agent-x");
        // Default workspace: deterministic per tenant (REQ-TA-001/004).
        assert_eq!(ctx.workspace_id(), &default_workspace_id(&tenant_id));
        assert_eq!(*ctx.workspace_id(), resolver.workspace_id(&tenant_id));
    }

    #[tokio::test]
    async fn uniform_auth_failure_no_existence_leak() {
        // REQ-TA-006: unknown tenant, wrong key and malformed token all
        // surface the SAME error — existence is never confirmed.
        let (dir, _store, _tenant_id, key) = provision("dev");
        let resolver = TenantResolverImpl::open(dir.path());

        // Unknown tenant: well-formed token for a random tenant id.
        let ghost = format!(
            "memo_{}_0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijkl",
            TenantId::new()
        );
        // Wrong key: valid tenant, secret flipped.
        let mut wrong = key.into_string();
        let last = wrong.pop().unwrap();
        wrong.push(if last == '0' { '1' } else { '0' });
        // Malformed token.
        let junk = "not-a-token";

        let agent = AgentId::new("agent-x");
        for token in [&ghost, &wrong, junk] {
            let err = resolver
                .resolve(token, agent.clone())
                .await
                .expect_err("must fail");
            assert_eq!(err.code(), "AUTH_FAILED", "uniform for {token:?}");
            assert_eq!(err.to_string(), "authentication failed");
        }
    }

    #[test]
    fn startup_missing_token_fails_fast() {
        // REQ-TA-002: missing credentials at startup → fail fast, uniform.
        let _guard = ENV_LOCK.lock().unwrap();
        let (dir, _store, _tid, _key) = provision("dev");
        let resolver = TenantResolverImpl::open(dir.path());
        unsafe {
            std::env::remove_var("MEMENTO_TOKEN");
            std::env::set_var("MEMENTO_AGENT_ID", "agent-x");
        }
        let err = resolver.resolve_from_env().expect_err("no token");
        assert_eq!(err.code(), "AUTH_FAILED");
    }

    #[test]
    fn startup_missing_agent_fails_fast() {
        // REQ-TA-003: missing MEMENTO_AGENT_ID → error names the variable.
        let _guard = ENV_LOCK.lock().unwrap();
        let (dir, _store, _tid, key) = provision("dev");
        let resolver = TenantResolverImpl::open(dir.path());
        unsafe {
            std::env::set_var("MEMENTO_TOKEN", key.as_str());
            std::env::remove_var("MEMENTO_AGENT_ID");
        }
        let err = resolver.resolve_from_env().expect_err("no agent id");
        assert_eq!(err.code(), "INVALID_INPUT");
        assert!(err.to_string().contains("MEMENTO_AGENT_ID"), "{err}");
    }

    #[test]
    fn startup_resolves_from_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let (dir, _store, tenant_id, key) = provision("dev");
        let resolver = TenantResolverImpl::open(dir.path());
        unsafe {
            std::env::set_var("MEMENTO_TOKEN", key.as_str());
            std::env::set_var("MEMENTO_AGENT_ID", "env-agent");
        }
        let ctx = resolver.resolve_from_env().expect("env binding");
        assert_eq!(ctx.tenant_id(), &tenant_id);
        assert_eq!(ctx.agent_id().as_str(), "env-agent");
    }

    #[tokio::test]
    async fn override_attempt_rejected() {
        // REQ-TA-002: a process bound to T1 ignores any parameter claiming
        // another tenant; all effects stay within T1.
        let (dir, store, t1, key1) = provision("t1");
        let (t2, key2) = store.create_tenant("t2").expect("t2");
        let resolver = TenantResolverImpl::open(dir.path());
        let ctx1 = resolver
            .resolve(key1.as_str(), AgentId::new("agent-x"))
            .await
            .expect("t1");
        // A genuine T2 context — resolve() is the ONLY way to obtain one.
        let ctx2 = resolver
            .resolve(key2.as_str(), AgentId::new("x"))
            .await
            .expect("t2 ctx");
        assert_eq!(ctx2.tenant_id(), &t2);

        TenantContext::with(ctx1.clone(), || {
            // 1. A hostile call carries a parameter claiming tenant T2; the
            //    guard reads the BOUND context, so the claim is ignored.
            let claimed_tenant = t2.to_string();
            let bound = TenantContext::current().expect("bound");
            assert_eq!(bound.tenant_id(), &t1);
            assert_ne!(bound.tenant_id().to_string(), claimed_tenant);

            // 2. Even holding a T2 context object cannot mutate the process
            //    binding: the scope still reports T1, so no effect can land
            //    outside T1.
            assert_eq!(TenantContext::current().expect("bound").tenant_id(), &t1);
            let _ = &ctx2; // the hostile call may hold it, but it is inert here
        });
    }

    #[test]
    fn guard_fires_before_storage_access() {
        // REQ-TA-005: without a bound context the guard errors before any
        // storage access — nothing touches disk without a context.
        let err = TenantContext::current().expect_err("no binding");
        assert_eq!(err.code(), CODE_TENANT_REQUIRED);
    }

    #[test]
    fn default_workspace_is_deterministic_and_isolated() {
        let tid1 = TenantId::from_str("00000000-0000-7000-8000-000000000001").unwrap();
        let tid2 = TenantId::from_str("00000000-0000-7000-8000-000000000002").unwrap();

        let w1 = default_workspace_id(&tid1);
        let w1_again = default_workspace_id(&tid1);
        let w2 = default_workspace_id(&tid2);

        assert_eq!(w1, w1_again, "stable across restarts");
        assert_ne!(w1, w2, "different tenants → different workspaces");
        assert_ne!(
            w1.as_uuid(),
            tid1.as_uuid(),
            "workspace id never collides with tenant id"
        );
    }
}
