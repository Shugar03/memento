//! Tenant identity and the opaque execution context (REQ-TA-001..005).
//!
//! `TenantContext` is the process-bound identity resolved at startup from the
//! bearer credential (REQ-TA-002). It is opaque: the constructor is
//! `pub(crate)`, so production code outside memento-domain cannot fabricate a
//! context — only the tenant resolver (`memento-tenant`) binds one. No request
//! parameter can override it.

use crate::error::DomainError;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// Tenant identifier — machine-generated (UUID v7).
uuid_newtype!(TenantId);
// Workspace identifier — machine-generated (UUID v7).
uuid_newtype!(WorkspaceId);
// Agent identifier — human-chosen name (MEMENTO_AGENT_ID, REQ-TA-003).
string_newtype!(AgentId);

/// Opaque tenant execution context bound for the lifetime of a process.
#[derive(Debug, Clone)]
pub struct TenantContext {
    tenant_id: TenantId,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
}

impl TenantContext {
    /// Internal constructor: only domain-internal code (in practice, the
    /// tenant resolver) may create a context (REQ-TA-002/005).
    /// Dead-code allowed until memento-tenant (batch 6) consumes it.
    #[allow(dead_code)]
    pub(crate) fn new(tenant_id: TenantId, workspace_id: WorkspaceId, agent_id: AgentId) -> Self {
        Self {
            tenant_id,
            workspace_id,
            agent_id,
        }
    }

    /// Test-only constructor behind the `testkit` feature (design D2):
    /// memento-testkit and adapter test suites need a bound context before the
    /// memento-tenant resolver (batch 6) exists. The feature is never enabled
    /// in production builds, so the resolver-only invariant still holds there.
    #[cfg(feature = "testkit")]
    pub fn new_for_tests(
        tenant_id: TenantId,
        workspace_id: WorkspaceId,
        agent_id: AgentId,
    ) -> Self {
        Self::new(tenant_id, workspace_id, agent_id)
    }

    /// The bound tenant id.
    pub fn tenant_id(&self) -> &TenantId {
        &self.tenant_id
    }

    /// The bound workspace id.
    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    /// The bound agent id.
    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    /// The context bound to the current task, or `TENANT_REQUIRED` when none
    /// is bound (REQ-TA-005: the guard fires before any storage access).
    pub fn current() -> Result<Self, DomainError> {
        CURRENT_TENANT
            .try_with(|ctx| ctx.clone())
            .map_err(|_| DomainError::TenantRequired)
    }

    /// Run `f` with `ctx` bound as the current tenant context for the duration
    /// of the call. The previous binding (if any) is restored afterwards.
    pub fn with<F, R>(ctx: TenantContext, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        CURRENT_TENANT.sync_scope(ctx, f)
    }
}

tokio::task_local! {
    /// Task-local tenant context. One process serves exactly one tenant
    /// (REQ-TA-001): the resolver binds this at startup and nothing overrides it.
    static CURRENT_TENANT: TenantContext;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CODE_TENANT_REQUIRED;

    /// Test-only construction path. Production code has no such path: the
    /// constructor is `pub(crate)` and the fields are private, so a context can
    /// only originate from the tenant resolver.
    fn test_context() -> TenantContext {
        TenantContext::new(TenantId::new(), WorkspaceId::new(), AgentId::new("agent-a"))
    }

    #[test]
    fn tenant_context_requires_resolver() {
        // Without a bound context, current() fails with TENANT_REQUIRED —
        // contexts only enter the process through the resolver path.
        let err = TenantContext::current().expect_err("no context bound");
        assert_eq!(err.code(), CODE_TENANT_REQUIRED);
    }

    #[test]
    fn tenant_context_with_scope() {
        let ctx = test_context();
        TenantContext::with(ctx.clone(), || {
            let current = TenantContext::current().expect("context bound by scope");
            assert_eq!(current.tenant_id(), ctx.tenant_id());
            assert_eq!(current.workspace_id(), ctx.workspace_id());
            assert_eq!(current.agent_id(), ctx.agent_id());
        });
        // The scope restored the previous state: no context outside it.
        let err = TenantContext::current().expect_err("scope ended");
        assert_eq!(err.code(), CODE_TENANT_REQUIRED);
    }
}
