//! Scratch per-tenant store (design D8 storage layout).
//!
//! [`TempStore`] owns a `tempfile::TempDir` and lays out the production
//! directory structure under it — `db/tenants/<tenant_id>/lancedb` — so
//! adapter tests exercise the real path resolution without touching user data.
//! It does NOT depend on memento-lancedb: the storage adapter opens the path
//! itself, which keeps testkit cycle-free and usable by every adapter.

use memento_domain::{AgentId, TenantContext, TenantId, WorkspaceId};
use std::path::{Path, PathBuf};

/// A disposable per-tenant store on a temp dir.
#[derive(Debug)]
pub struct TempStore {
    _tempdir: tempfile::TempDir,
    tenant_id: TenantId,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
}

impl TempStore {
    /// Create a fresh scratch store with a brand-new tenant/workspace/agent.
    pub fn new() -> Self {
        let _tempdir = tempfile::tempdir().expect("create test temp dir");
        Self {
            _tempdir,
            tenant_id: TenantId::new(),
            workspace_id: WorkspaceId::new(),
            agent_id: AgentId::new("test-agent"),
        }
    }

    /// The tenant owning this scratch store.
    pub fn tenant_id(&self) -> &TenantId {
        &self.tenant_id
    }

    /// The default workspace bound to this scratch store.
    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    /// The agent stamping this scratch store's rows.
    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    /// The temp dir root (production layout root: `~/.memento` equivalent).
    pub fn root(&self) -> &Path {
        self._tempdir.path()
    }

    /// The LanceDB dir for this tenant: `<root>/db/tenants/<tid>/lancedb`.
    pub fn lancedb_dir(&self) -> PathBuf {
        self.root()
            .join("db")
            .join("tenants")
            .join(self.tenant_id.to_string())
            .join("lancedb")
    }

    /// A bound [`TenantContext`] for this scratch tenant (testkit feature of
    /// memento-domain; production code has no such constructor).
    pub fn ctx(&self) -> TenantContext {
        TenantContext::new_for_tests(
            self.tenant_id,
            self.workspace_id,
            self.agent_id.clone(),
        )
    }
}

impl Default for TempStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_tenant_is_unique_per_call() {
        let a = TempStore::new();
        let b = TempStore::new();
        assert_ne!(a.tenant_id(), b.tenant_id());
        assert_ne!(a.root(), b.root());
        assert_ne!(a.lancedb_dir(), b.lancedb_dir());
    }

    #[test]
    fn layout_matches_production_d8() {
        let store = TempStore::new();
        let expected = store
            .root()
            .join("db")
            .join("tenants")
            .join(store.tenant_id().to_string())
            .join("lancedb");
        assert_eq!(store.lancedb_dir(), expected);
        assert!(store.lancedb_dir().is_absolute());
    }

    #[test]
    fn ctx_matches_store_identity() {
        let store = TempStore::new();
        let ctx = store.ctx();
        assert_eq!(ctx.tenant_id(), store.tenant_id());
        assert_eq!(ctx.workspace_id(), store.workspace_id());
        assert_eq!(ctx.agent_id(), store.agent_id());
    }
}
