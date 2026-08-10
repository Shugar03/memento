//! Tenant resolution port (REQ-TA-002/003/005).

use async_trait::async_trait;
use memento_domain::{AgentId, DomainError, TenantContext, TenantId};

/// Identity boundary: binds the opaque `TenantContext` at process startup.
/// This is the ONLY producer of a tenant context (REQ-TA-002).
#[async_trait]
pub trait TenantResolver: Send + Sync {
    /// Validate a bearer token + agent id and bind the tenant context.
    /// Failures are uniform (`AUTH_FAILED`) — no tenant-existence leak
    /// (REQ-TA-006).
    async fn resolve(&self, token: &str, agent_id: AgentId) -> Result<TenantContext, DomainError>;

    /// Rotate the tenant token; returns the new token (old one dies
    /// immediately; process restart required, REQ-TA-008).
    async fn rotate_token(&self, tenant_id: &TenantId) -> Result<String, DomainError>;
}
