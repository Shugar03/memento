//! Workspace commands (REQ-WS-001/002): create and list isolation scopes.

use clap::ArgMatches;
use memento_domain::DomainError;
use memento_tenant::WorkspaceRegistry;
use serde_json::json;

use crate::output::emit_json_value;
use crate::startup::CliApp;

/// Dispatch `memento workspace <sub>`.
pub async fn run(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    match m.subcommand() {
        Some(("create", sub)) => create(sub, app),
        Some(("list", sub)) => list(sub, app),
        _ => Err(DomainError::InvalidInput {
            message: "unknown workspace subcommand; run 'memento workspace --help'".into(),
        }),
    }
}

fn create(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let name = m.get_one::<String>("name").cloned();
    let reg = WorkspaceRegistry::open(&app.root, app.ctx.tenant_id());
    let record = reg.create(name)?;
    if m.get_flag("json") {
        emit_json_value(&json!({
            "workspace_id": record.id,
            "name": record.name,
            "created_at": record.created_at,
            "is_default": false,
        }));
        Ok(())
    } else {
        println!("workspace {}", record.id);
        Ok(())
    }
}

fn list(m: &ArgMatches, app: &CliApp) -> Result<(), DomainError> {
    let reg = WorkspaceRegistry::open(&app.root, app.ctx.tenant_id());
    let workspaces = reg.list()?;
    if m.get_flag("json") {
        emit_json_value(&json!({
            "workspaces": workspaces.iter().map(|w| json!({
                "workspace_id": w.id,
                "name": w.name,
                "created_at": w.created_at,
                "is_default": w.is_default,
            })).collect::<Vec<_>>(),
        }));
        Ok(())
    } else {
        for w in &workspaces {
            let marker = if w.is_default { " (default)" } else { "" };
            let name = w.name.as_deref().unwrap_or("-");
            println!("{}  {name}{marker}", w.id);
        }
        Ok(())
    }
}
