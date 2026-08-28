use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

const MAX_DIAGNOSTIC_BYTES: usize = 1536;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigReconciliationState {
    Unavailable,
    Reconciling,
    Converged,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigReconciliationOperation {
    Startup,
    Reload,
    Create,
    Update,
    Delete,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigReconciliationObjectType {
    Device,
    Line,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigReconciliationTrigger {
    operation: ConfigReconciliationOperation,
    object_type: Option<ConfigReconciliationObjectType>,
    object_id: Option<String>,
}

impl ConfigReconciliationTrigger {
    pub const fn startup() -> Self {
        Self {
            operation: ConfigReconciliationOperation::Startup,
            object_type: None,
            object_id: None,
        }
    }

    pub const fn reload() -> Self {
        Self {
            operation: ConfigReconciliationOperation::Reload,
            object_type: None,
            object_id: None,
        }
    }

    pub fn mutation(
        operation: ConfigReconciliationOperation,
        object_type: ConfigReconciliationObjectType,
        object_id: String,
    ) -> Self {
        Self {
            operation,
            object_type: Some(object_type),
            object_id: Some(object_id),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConfigReconciliationStatus {
    pub generation: u64,
    pub state: ConfigReconciliationState,
    pub operation: Option<ConfigReconciliationOperation>,
    pub object_type: Option<ConfigReconciliationObjectType>,
    pub object_id: Option<String>,
    pub diagnostic: Option<String>,
}

impl Default for ConfigReconciliationStatus {
    fn default() -> Self {
        Self {
            generation: 0,
            state: ConfigReconciliationState::Unavailable,
            operation: None,
            object_type: None,
            object_id: None,
            diagnostic: None,
        }
    }
}

#[derive(Default)]
struct ConfigReconciliationStateData {
    current: ConfigReconciliationStatus,
}

#[derive(Default)]
pub struct ConfigReconciliation {
    serial: Mutex<()>,
    state: Mutex<ConfigReconciliationStateData>,
}

impl ConfigReconciliation {
    pub fn status(&self) -> ConfigReconciliationStatus {
        lock_unpoisoned(&self.state).current.clone()
    }

    pub fn reconcile<F>(&self, trigger: ConfigReconciliationTrigger, apply: F) -> Result<(), String>
    where
        F: FnOnce() -> Result<(), String>,
    {
        self.reconcile_with(trigger, apply, |_| {})
    }

    pub fn reconcile_with<F, P>(
        &self,
        trigger: ConfigReconciliationTrigger,
        apply: F,
        publish: P,
    ) -> Result<(), String>
    where
        F: FnOnce() -> Result<(), String>,
        P: FnOnce(ConfigReconciliationStatus),
    {
        let _serial = lock_unpoisoned(&self.serial);
        let generation = {
            let mut state = lock_unpoisoned(&self.state);
            let generation =
                state.current.generation.checked_add(1).ok_or_else(|| {
                    "configuration reconciliation generation exhausted".to_owned()
                })?;
            state.current = ConfigReconciliationStatus {
                generation,
                state: ConfigReconciliationState::Reconciling,
                operation: Some(trigger.operation),
                object_type: trigger.object_type,
                object_id: trigger.object_id,
                diagnostic: None,
            };
            generation
        };
        let result = apply();
        let status = {
            let mut state = lock_unpoisoned(&self.state);
            state.current.generation = generation;
            match &result {
                Ok(()) => {
                    state.current.state = ConfigReconciliationState::Converged;
                    state.current.diagnostic = None;
                }
                Err(error) => {
                    state.current.state = ConfigReconciliationState::Failed;
                    state.current.diagnostic = Some(bounded_diagnostic(error));
                }
            }
            state.current.clone()
        };
        publish(status);
        result
    }
}

fn bounded_diagnostic(value: &str) -> String {
    let mut bounded = String::with_capacity(value.len().min(MAX_DIAGNOSTIC_BYTES));
    for character in value.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if bounded.len() + character.len_utf8() > MAX_DIAGNOSTIC_BYTES {
            break;
        }
        bounded.push(character);
    }
    bounded
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_unavailable_and_records_terminal_generations() {
        let reconciliation = ConfigReconciliation::default();
        assert_eq!(
            reconciliation.status(),
            ConfigReconciliationStatus::default()
        );

        reconciliation
            .reconcile(ConfigReconciliationTrigger::startup(), || Ok(()))
            .unwrap();
        assert_eq!(
            reconciliation.status(),
            ConfigReconciliationStatus {
                generation: 1,
                state: ConfigReconciliationState::Converged,
                operation: Some(ConfigReconciliationOperation::Startup),
                object_type: None,
                object_id: None,
                diagnostic: None,
            }
        );

        let error = reconciliation
            .reconcile(
                ConfigReconciliationTrigger::mutation(
                    ConfigReconciliationOperation::Update,
                    ConfigReconciliationObjectType::Device,
                    "SEP001122334455".into(),
                ),
                || Err("unknown line 9999".into()),
            )
            .unwrap_err();
        assert_eq!(error, "unknown line 9999");
        assert_eq!(
            reconciliation.status(),
            ConfigReconciliationStatus {
                generation: 2,
                state: ConfigReconciliationState::Failed,
                operation: Some(ConfigReconciliationOperation::Update),
                object_type: Some(ConfigReconciliationObjectType::Device),
                object_id: Some("SEP001122334455".into()),
                diagnostic: Some("unknown line 9999".into()),
            }
        );
    }

    #[test]
    fn bounds_failure_diagnostics_on_utf8_boundaries() {
        let reconciliation = ConfigReconciliation::default();
        let diagnostic = "å".repeat(MAX_DIAGNOSTIC_BYTES);
        reconciliation
            .reconcile(ConfigReconciliationTrigger::reload(), || {
                Err(diagnostic.clone())
            })
            .unwrap_err();
        let stored = reconciliation.status().diagnostic.unwrap();
        assert!(stored.len() <= MAX_DIAGNOSTIC_BYTES);
        assert!(diagnostic.starts_with(&stored));

        assert_eq!(
            bounded_diagnostic("first\nsecond\tthird"),
            "first second third"
        );
    }

    #[test]
    fn status_has_a_stable_json_shape() {
        let reconciliation = ConfigReconciliation::default();
        reconciliation
            .reconcile(
                ConfigReconciliationTrigger::mutation(
                    ConfigReconciliationOperation::Delete,
                    ConfigReconciliationObjectType::Line,
                    "1001".into(),
                ),
                || Ok(()),
            )
            .unwrap();
        let status = serde_json::to_value(reconciliation.status()).unwrap();
        assert_eq!(status["generation"], 1);
        assert_eq!(status["state"], "converged");
        assert_eq!(status["operation"], "delete");
        assert_eq!(status["object_type"], "line");
        assert_eq!(status["object_id"], "1001");
        assert!(status["diagnostic"].is_null());
    }
}
