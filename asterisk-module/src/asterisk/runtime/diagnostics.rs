//! Runtime snapshot composition for native media and session diagnostics.

use thiserror::Error;

use super::super::controller_step;
use super::{Access, RuntimeInventoryProvider};
use crate::ami::diagnostics::{
    CliDiagnosticCommand, CliDiagnosticError, CliDiagnosticSnapshot, CliSessionCall,
    complete_cli_diagnostics, render_cli_diagnostics,
};
use crate::ami::inventory::InventoryProvider;
use crate::ami::runtime::RuntimeStatusProvider;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RuntimeCliDiagnosticError {
    #[error("CLI diagnostics are unavailable")]
    Unavailable,
    #[error(transparent)]
    Diagnostic(#[from] CliDiagnosticError),
}

pub fn render_runtime_cli_diagnostics(
    access: &Access,
    command: CliDiagnosticCommand,
    arguments: &[&str],
) -> Result<String, RuntimeCliDiagnosticError> {
    let snapshot = runtime_cli_diagnostic_snapshot(access)?;
    render_cli_diagnostics(command, arguments, &snapshot).map_err(Into::into)
}

pub fn complete_runtime_cli_diagnostics(
    access: &Access,
    command: CliDiagnosticCommand,
    arguments: &[&str],
    prefix: &str,
    ordinal: usize,
) -> Option<String> {
    let snapshot = runtime_cli_diagnostic_snapshot(access).ok()?;
    complete_cli_diagnostics(command, arguments, prefix, ordinal, &snapshot)
}

fn runtime_cli_diagnostic_snapshot(
    access: &Access,
) -> Result<CliDiagnosticSnapshot, RuntimeCliDiagnosticError> {
    let provider = RuntimeInventoryProvider {
        shared: std::sync::Arc::downgrade(&access.shared),
        phone: access.phone.clone(),
    };
    let inventory = InventoryProvider::snapshot(&provider)
        .map_err(|_| RuntimeCliDiagnosticError::Unavailable)?;
    let runtime = RuntimeStatusProvider::snapshot(&provider)
        .map_err(|_| RuntimeCliDiagnosticError::Unavailable)?;
    let session_calls = controller_step(&access.shared.controller, |controller| {
        controller
            .calls()
            .map(|call| CliSessionCall {
                device_id: call.device_id,
                pbx_id: call.pbx_id,
                call_id: call.sccp_id,
            })
            .collect()
    });
    Ok(CliDiagnosticSnapshot {
        inventory,
        runtime,
        session_calls,
    })
}
