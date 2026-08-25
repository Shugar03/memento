//! Per-tenant workspace registry (REQ-WS-001/002).
//!
//! Workspaces are isolation boundaries inside a tenant (`tenant_id +
//! workspace_id`). The process-bound default workspace is deterministic
//! ([`default_workspace_id`]); additional workspaces are minted by
//! [`WorkspaceRegistry::create`] and persisted as JSONL under
//! `<root>/db/tenants/<tid>/workspaces.jsonl` (design D8).
//!
//! Listing always includes the default workspace, even when the registry
//! file is missing — existing tenants stay compatible without migration.

use chrono::{DateTime, Utc};
use memento_domain::{DomainError, TenantId, WorkspaceId};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::resolver::default_workspace_id;

/// One registered workspace (REQ-WS-001).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub id: WorkspaceId,
    /// Optional human label (never used as a key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
    /// True when this is the process-bound default for the tenant.
    #[serde(default)]
    pub is_default: bool,
}

/// Filesystem-backed workspace registry for one tenant.
pub struct WorkspaceRegistry {
    path: PathBuf,
    tenant_id: TenantId,
}

impl WorkspaceRegistry {
    /// Open the registry for `tenant_id` under `root` (does not create the
    /// file until the first [`Self::create`]).
    pub fn open(root: &Path, tenant_id: &TenantId) -> Self {
        Self {
            path: root
                .join("db")
                .join("tenants")
                .join(tenant_id.to_string())
                .join("workspaces.jsonl"),
            tenant_id: *tenant_id,
        }
    }

    /// Mint a new workspace id, persist it, and return the record (REQ-WS-001).
    pub fn create(&self, name: Option<String>) -> Result<WorkspaceRecord, DomainError> {
        let record = WorkspaceRecord {
            id: WorkspaceId::new(),
            name,
            created_at: Utc::now(),
            is_default: false,
        };
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(DomainError::from)?;
        }
        let line = serde_json::to_string(&record).map_err(|err| DomainError::Internal {
            message: format!("serializing workspace record: {err}"),
        })?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(DomainError::from)?;
        writeln!(file, "{line}").map_err(DomainError::from)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
        }
        Ok(record)
    }

    /// List registered workspaces, always including the default (REQ-WS-002).
    pub fn list(&self) -> Result<Vec<WorkspaceRecord>, DomainError> {
        let default_id = default_workspace_id(&self.tenant_id);
        let mut out = vec![WorkspaceRecord {
            id: default_id,
            name: Some("default".into()),
            created_at: DateTime::<Utc>::UNIX_EPOCH,
            is_default: true,
        }];
        for ws in self.load_registered()? {
            if ws.id != default_id {
                out.push(WorkspaceRecord {
                    is_default: false,
                    ..ws
                });
            }
        }
        Ok(out)
    }

    /// True when `id` is the default or appears in the registry.
    pub fn contains(&self, id: &WorkspaceId) -> Result<bool, DomainError> {
        if *id == default_workspace_id(&self.tenant_id) {
            return Ok(true);
        }
        Ok(self.load_registered()?.iter().any(|w| w.id == *id))
    }

    fn load_registered(&self) -> Result<Vec<WorkspaceRecord>, DomainError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let file = fs::File::open(&self.path).map_err(DomainError::from)?;
        let mut out = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line.map_err(DomainError::from)?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record: WorkspaceRecord =
                serde_json::from_str(line).map_err(|err| DomainError::InvalidInput {
                    message: format!("corrupt workspaces.jsonl: {err}"),
                })?;
            out.push(record);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn create_persists_and_list_includes_default() {
        let dir = tempdir().unwrap();
        let tid = TenantId::new();
        let reg = WorkspaceRegistry::open(dir.path(), &tid);
        let created = reg.create(Some("pilot".into())).unwrap();
        let listed = reg.list().unwrap();
        assert!(listed.iter().any(|w| w.is_default));
        assert!(
            listed
                .iter()
                .any(|w| w.id == created.id && w.name.as_deref() == Some("pilot"))
        );
        assert!(reg.contains(&created.id).unwrap());
    }

    #[test]
    fn list_without_file_still_returns_default() {
        let dir = tempdir().unwrap();
        let tid = TenantId::new();
        let reg = WorkspaceRegistry::open(dir.path(), &tid);
        let listed = reg.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].is_default);
        assert_eq!(listed[0].id, default_workspace_id(&tid));
    }
}
