//! Token rotation (design D3, risk R9).
//!
//! `memento tenant rotate-token` (CLI, T-082) replaces the stored Argon2id
//! hash with a fresh token's hash. Consequences:
//!
//! * **The old token dies immediately** — its hash is gone, so any process
//!   started after the rotation with the old token fails auth.
//! * **A restart is required** — the running process keeps its already-bound
//!   `TenantContext` (bound at startup, never re-verified, REQ-TA-002), so it
//!   continues operating with the OLD credential until restarted. This is an
//!   accepted MVP tradeoff (risk R9, "pre-audit acceptable" per design).
//! * **Unknown tenants fail uniformly** (`AUTH_FAILED`) — rotation never
//!   confirms tenant existence (REQ-TA-006).
//!
//! Rotation events are emitted via `tracing`; the audit JSONL is the
//! application layer's job (T-066). Credential material is never logged.

use crate::credentials::{ApiKey, CredentialStore};
use memento_domain::{DomainError, TenantId};

/// Rotate the tenant's bearer token: generate a new `memo_<tid>_<48×base62>`
/// key, hash it, and atomically replace the stored hash. Returns the new
/// plaintext key (shown to the operator exactly once).
pub fn rotate_token(store: &CredentialStore, tenant_id: &TenantId) -> Result<ApiKey, DomainError> {
    let key = store.rotate(tenant_id)?;
    tracing::info!(tenant_id = %tenant_id, "tenant credential rotated; old token invalidated, restart required");
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::TenantResolverImpl;
    use memento_domain::{AgentId, TenantContext, TenantId};
    use memento_ports::TenantResolver;
    use tempfile::TempDir;

    fn provision() -> (TempDir, TenantResolverImpl, TenantId, ApiKey) {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolver = TenantResolverImpl::open(dir.path());
        let (tenant_id, key) = resolver.store().create_tenant("dev").expect("provision");
        (dir, resolver, tenant_id, key)
    }

    #[tokio::test]
    async fn old_token_fails_new_works_after_rotation() {
        // T-052 acceptance: integration — old token fails post-rotation,
        // the new token resolves.
        let (_dir, resolver, tenant_id, old_key) = provision();

        // Baseline: old token resolves before rotation.
        let ctx = resolver
            .resolve(old_key.as_str(), AgentId::new("agent-x"))
            .await
            .expect("old token before rotation");
        assert_eq!(ctx.tenant_id(), &tenant_id);

        // Rotate through the port (the CLI's path).
        let new_token = resolver
            .rotate_token(&tenant_id)
            .await
            .expect("rotation succeeds");
        assert_ne!(new_token, old_key.as_str(), "a fresh key is issued");

        // Old token dies immediately.
        let err = resolver
            .resolve(old_key.as_str(), AgentId::new("agent-x"))
            .await
            .expect_err("old token dead");
        assert_eq!(err.code(), "AUTH_FAILED");

        // New token works, binds the same tenant and workspace.
        let ctx = resolver
            .resolve(&new_token, AgentId::new("agent-x"))
            .await
            .expect("new token resolves");
        assert_eq!(ctx.tenant_id(), &tenant_id);
        assert_eq!(
            ctx.workspace_id(),
            &crate::resolver::default_workspace_id(&tenant_id)
        );
    }

    #[tokio::test]
    async fn rotation_never_leaks_tenant_existence() {
        let (_dir, resolver, _tid, _key) = provision();
        let err = resolver
            .rotate_token(&TenantId::new())
            .await
            .expect_err("unknown tenant");
        assert_eq!(err.code(), "AUTH_FAILED", "uniform like resolve");
    }

    #[tokio::test]
    async fn bound_process_keeps_working_after_rotation() {
        // Risk R9 documented behavior: the RUNNING process keeps its bound
        // context after rotation; a restart is required only for NEW
        // processes (the context is bound at startup and never re-verified).
        let (_dir, resolver, tenant_id, key) = provision();
        let ctx = resolver
            .resolve(key.as_str(), AgentId::new("agent-x"))
            .await
            .expect("bind at startup");
        resolver.rotate_token(&tenant_id).await.expect("rotate");

        // The already-bound scope still reports the tenant — rotation does
        // not yank the running process.
        TenantContext::with(ctx.clone(), || {
            let current = TenantContext::current().expect("bound");
            assert_eq!(current.tenant_id(), &tenant_id);
            assert_eq!(current.agent_id(), ctx.agent_id());
        });

        // But a NEW resolution with the old key fails (a restart must use the
        // new token — documented in the module docs and the CLI help, T-082).
        let err = resolver
            .resolve(key.as_str(), AgentId::new("agent-x"))
            .await
            .expect_err("old key dead for new processes");
        assert_eq!(err.code(), "AUTH_FAILED");
    }

    #[test]
    fn rotation_trace_is_static() {
        // Credential hygiene (REQ-TA-006, REQ-CG-003): the rotation trace
        // line must never interpolate credential material. Pin the static
        // format string so a future edit cannot accidentally log the key.
        let (_dir, resolver, tenant_id, _key) = provision();
        let _ = rotate_token(resolver.store(), &tenant_id).expect("rotate");
        let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/rotate.rs"))
            .expect("rotate.rs readable");
        assert!(
            source.contains(
                "tracing::info!(tenant_id = %tenant_id, \"tenant credential rotated; old token invalidated, restart required\");"
            ),
            "rotation trace must stay the static line"
        );
    }
}
