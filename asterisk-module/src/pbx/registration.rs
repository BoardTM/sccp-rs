//! Registration-context extension publication and lifecycle policy.

use std::collections::{BTreeMap, BTreeSet};

use sccp_protocol::DeviceId;
use thiserror::Error;

use crate::config::{ModuleConfig, RegistrationTarget};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationContextPolicy {
    RequireExisting,
    CreateIfMissing,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RegistrationAppearanceOwner {
    pub device_id: DeviceId,
    pub line_instance: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistrationExtensionSpec {
    pub target: RegistrationTarget,
    pub line: String,
    pub context_policy: RegistrationContextPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistrationAppearance {
    pub owner: RegistrationAppearanceOwner,
    pub extensions: Vec<RegistrationExtensionSpec>,
}

/// Builds deterministic publication input for the configured appearances of
/// the supplied registered devices. Unknown and guest devices have no entries.
pub fn configured_registration_appearances<'a>(
    config: &ModuleConfig,
    registered_devices: impl IntoIterator<Item = &'a DeviceId>,
) -> Vec<RegistrationAppearance> {
    let registered: BTreeSet<_> = registered_devices.into_iter().cloned().collect();
    let mut appearances = Vec::new();
    for device_id in registered {
        for binding in config.appearances_for_device(&device_id) {
            let Some(registration) = config.registration_for_line(&binding.line.number) else {
                continue;
            };
            let mut extensions = Vec::new();
            for entry in &registration.extensions {
                if let Some(context) = &entry.context {
                    extensions.push(RegistrationExtensionSpec {
                        target: RegistrationTarget {
                            extension: entry.extension.clone(),
                            context: context.clone(),
                        },
                        line: binding.line.number.clone(),
                        context_policy: RegistrationContextPolicy::RequireExisting,
                    });
                } else {
                    extensions.extend(config.registration_contexts().iter().map(|context| {
                        RegistrationExtensionSpec {
                            target: RegistrationTarget {
                                extension: entry.extension.clone(),
                                context: context.clone(),
                            },
                            line: binding.line.number.clone(),
                            context_policy: RegistrationContextPolicy::CreateIfMissing,
                        }
                    }));
                }
            }
            if !extensions.is_empty() {
                appearances.push(RegistrationAppearance {
                    owner: RegistrationAppearanceOwner {
                        device_id: binding.device_id.clone(),
                        line_instance: binding.line_instance,
                    },
                    extensions,
                });
            }
        }
    }
    appearances
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RegistrationExtensionBackendError {
    #[error("registration extension input is invalid")]
    Invalid,
    #[error("the requested dialplan context or extension is absent")]
    NotFound,
    #[error("the dialplan target is already owned")]
    Conflict,
    #[error("registration extension publication is unavailable")]
    Unavailable,
    #[error("the dialplan backend rejected the registration extension operation")]
    Failed,
}

pub trait RegistrationExtensionBackend: Send + Sync + 'static {
    fn publish(
        &self,
        extension: &RegistrationExtensionSpec,
    ) -> Result<(), RegistrationExtensionBackendError>;

    fn replace(
        &self,
        extension: &RegistrationExtensionSpec,
    ) -> Result<(), RegistrationExtensionBackendError>;

    fn unpublish(
        &self,
        target: &RegistrationTarget,
    ) -> Result<(), RegistrationExtensionBackendError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationRegistryOperation {
    Publish,
    Replace,
    Unpublish,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RegistrationRegistryError {
    #[error("one registration target is assigned to conflicting logical lines")]
    ConflictingTarget,
    #[error("registration extension {operation:?} failed: {source}")]
    Backend {
        operation: RegistrationRegistryOperation,
        source: RegistrationExtensionBackendError,
    },
    #[error(
        "registration extension rollback failed after {operation:?}: operation failed: {source}; rollback failed: {rollback_source}"
    )]
    RollbackFailed {
        operation: RegistrationRegistryOperation,
        source: RegistrationExtensionBackendError,
        rollback_source: RegistrationExtensionBackendError,
    },
    #[error("registration extension state may differ from the dialplan backend")]
    Diverged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DesiredRegistration {
    extension: RegistrationExtensionSpec,
    owners: BTreeSet<RegistrationAppearanceOwner>,
}

enum AppliedOperation {
    Published(RegistrationExtensionSpec),
    Replaced { previous: RegistrationExtensionSpec },
    Unpublished(RegistrationExtensionSpec),
}

pub struct RegistrationContextRegistry<B: RegistrationExtensionBackend> {
    backend: B,
    active: BTreeMap<RegistrationTarget, DesiredRegistration>,
    cleanup: BTreeSet<RegistrationTarget>,
    diverged: bool,
}

impl<B: RegistrationExtensionBackend> RegistrationContextRegistry<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            active: BTreeMap::new(),
            cleanup: BTreeSet::new(),
            diverged: false,
        }
    }

    pub fn active_target_count(&self) -> usize {
        self.active.len()
    }

    pub fn reconcile(
        &mut self,
        appearances: impl IntoIterator<Item = RegistrationAppearance>,
    ) -> Result<(), RegistrationRegistryError> {
        if self.diverged {
            return Err(RegistrationRegistryError::Diverged);
        }
        let desired = desired_registrations(appearances)?;
        let mut applied = Vec::new();

        for (target, registration) in &desired {
            if self.active.contains_key(target) {
                continue;
            }
            if let Err(source) = self.backend.publish(&registration.extension) {
                return self.rollback(
                    RegistrationRegistryOperation::Publish,
                    source,
                    applied,
                    &desired,
                );
            }
            applied.push(AppliedOperation::Published(registration.extension.clone()));
        }

        for (target, registration) in &desired {
            let Some(previous) = self.active.get(target) else {
                continue;
            };
            if previous.extension == registration.extension {
                continue;
            }
            if let Err(source) = self.backend.replace(&registration.extension) {
                if source == RegistrationExtensionBackendError::NotFound {
                    applied.push(AppliedOperation::Unpublished(previous.extension.clone()));
                }
                return self.rollback(
                    RegistrationRegistryOperation::Replace,
                    source,
                    applied,
                    &desired,
                );
            }
            applied.push(AppliedOperation::Replaced {
                previous: previous.extension.clone(),
            });
        }

        for (target, registration) in &self.active {
            if desired.contains_key(target) {
                continue;
            }
            if let Err(source) = remove_target(&self.backend, target) {
                return self.rollback(
                    RegistrationRegistryOperation::Unpublish,
                    source,
                    applied,
                    &desired,
                );
            }
            applied.push(AppliedOperation::Unpublished(
                registration.extension.clone(),
            ));
        }

        self.active = desired;
        Ok(())
    }

    pub fn clear(&mut self) -> Result<(), RegistrationRegistryError> {
        self.reconcile(std::iter::empty())
    }

    fn rollback(
        &mut self,
        operation: RegistrationRegistryOperation,
        source: RegistrationExtensionBackendError,
        applied: Vec<AppliedOperation>,
        desired: &BTreeMap<RegistrationTarget, DesiredRegistration>,
    ) -> Result<(), RegistrationRegistryError> {
        let mut rollback_source = None;
        for applied in applied.into_iter().rev() {
            let result = match applied {
                AppliedOperation::Published(extension) => {
                    remove_target(&self.backend, &extension.target)
                }
                AppliedOperation::Replaced { previous } => self.backend.replace(&previous),
                AppliedOperation::Unpublished(previous) => self.backend.publish(&previous),
            };
            if let Err(error) = result {
                rollback_source.get_or_insert(error);
            }
        }
        if let Some(rollback_source) = rollback_source {
            self.diverged = true;
            self.cleanup.extend(self.active.keys().cloned());
            self.cleanup.extend(desired.keys().cloned());
            Err(RegistrationRegistryError::RollbackFailed {
                operation,
                source,
                rollback_source,
            })
        } else {
            Err(RegistrationRegistryError::Backend { operation, source })
        }
    }
}

impl<B: RegistrationExtensionBackend> Drop for RegistrationContextRegistry<B> {
    fn drop(&mut self) {
        let targets: BTreeSet<_> = self
            .active
            .keys()
            .chain(self.cleanup.iter())
            .cloned()
            .collect();
        for target in targets {
            let _ = remove_target(&self.backend, &target);
        }
    }
}

fn desired_registrations(
    appearances: impl IntoIterator<Item = RegistrationAppearance>,
) -> Result<BTreeMap<RegistrationTarget, DesiredRegistration>, RegistrationRegistryError> {
    let mut desired = BTreeMap::<RegistrationTarget, DesiredRegistration>::new();
    for appearance in appearances {
        for extension in appearance.extensions {
            match desired.entry(extension.target.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(DesiredRegistration {
                        extension,
                        owners: BTreeSet::from([appearance.owner.clone()]),
                    });
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    if entry.get().extension.line != extension.line
                        || entry.get().extension.context_policy != extension.context_policy
                    {
                        return Err(RegistrationRegistryError::ConflictingTarget);
                    }
                    entry.get_mut().owners.insert(appearance.owner.clone());
                }
            }
        }
    }
    Ok(desired)
}

fn remove_target<B: RegistrationExtensionBackend>(
    backend: &B,
    target: &RegistrationTarget,
) -> Result<(), RegistrationExtensionBackendError> {
    match backend.unpublish(target) {
        Ok(()) | Err(RegistrationExtensionBackendError::NotFound) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Call {
        Publish(RegistrationExtensionSpec),
        Replace(RegistrationExtensionSpec),
        Unpublish(RegistrationTarget),
    }

    #[derive(Default)]
    struct FakeState {
        calls: Vec<Call>,
        published: BTreeMap<RegistrationTarget, RegistrationExtensionSpec>,
        failures: HashMap<usize, RegistrationExtensionBackendError>,
        operation: usize,
    }

    #[derive(Clone, Default)]
    struct FakeBackend(Arc<Mutex<FakeState>>);

    impl FakeBackend {
        fn fail_at(&self, operation: usize, error: RegistrationExtensionBackendError) {
            self.0.lock().unwrap().failures.insert(operation, error);
        }

        fn calls(&self) -> Vec<Call> {
            self.0.lock().unwrap().calls.clone()
        }

        fn published(&self) -> BTreeMap<RegistrationTarget, RegistrationExtensionSpec> {
            self.0.lock().unwrap().published.clone()
        }

        fn remove_published(&self, target: &RegistrationTarget) {
            self.0.lock().unwrap().published.remove(target);
        }

        fn begin(&self, call: Call) -> Result<(), RegistrationExtensionBackendError> {
            let mut state = self.0.lock().unwrap();
            state.operation += 1;
            state.calls.push(call);
            let operation = state.operation;
            match state.failures.remove(&operation) {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    #[derive(Clone, Default)]
    struct BlockingBackend {
        backend: FakeBackend,
        gate: Arc<(Mutex<(bool, bool)>, Condvar)>,
    }

    impl BlockingBackend {
        fn wait_until_publish_started(&self) {
            let (lock, condition) = &*self.gate;
            let state = lock.lock().unwrap();
            drop(condition.wait_while(state, |state| !state.0).unwrap());
        }

        fn release_publish(&self) {
            let (lock, condition) = &*self.gate;
            lock.lock().unwrap().1 = true;
            condition.notify_all();
        }
    }

    impl RegistrationExtensionBackend for BlockingBackend {
        fn publish(
            &self,
            extension: &RegistrationExtensionSpec,
        ) -> Result<(), RegistrationExtensionBackendError> {
            let (lock, condition) = &*self.gate;
            let mut state = lock.lock().unwrap();
            state.0 = true;
            condition.notify_all();
            drop(condition.wait_while(state, |state| !state.1).unwrap());
            self.backend.publish(extension)
        }

        fn replace(
            &self,
            extension: &RegistrationExtensionSpec,
        ) -> Result<(), RegistrationExtensionBackendError> {
            self.backend.replace(extension)
        }

        fn unpublish(
            &self,
            target: &RegistrationTarget,
        ) -> Result<(), RegistrationExtensionBackendError> {
            self.backend.unpublish(target)
        }
    }

    impl RegistrationExtensionBackend for FakeBackend {
        fn publish(
            &self,
            extension: &RegistrationExtensionSpec,
        ) -> Result<(), RegistrationExtensionBackendError> {
            self.begin(Call::Publish(extension.clone()))?;
            let mut state = self.0.lock().unwrap();
            if state.published.contains_key(&extension.target) {
                return Err(RegistrationExtensionBackendError::Conflict);
            }
            state
                .published
                .insert(extension.target.clone(), extension.clone());
            Ok(())
        }

        fn replace(
            &self,
            extension: &RegistrationExtensionSpec,
        ) -> Result<(), RegistrationExtensionBackendError> {
            self.begin(Call::Replace(extension.clone()))?;
            let mut state = self.0.lock().unwrap();
            if !state.published.contains_key(&extension.target) {
                return Err(RegistrationExtensionBackendError::NotFound);
            }
            state
                .published
                .insert(extension.target.clone(), extension.clone());
            Ok(())
        }

        fn unpublish(
            &self,
            target: &RegistrationTarget,
        ) -> Result<(), RegistrationExtensionBackendError> {
            self.begin(Call::Unpublish(target.clone()))?;
            if self.0.lock().unwrap().published.remove(target).is_some() {
                Ok(())
            } else {
                Err(RegistrationExtensionBackendError::NotFound)
            }
        }
    }

    fn target(extension: &str, context: &str) -> RegistrationTarget {
        RegistrationTarget {
            extension: extension.into(),
            context: context.into(),
        }
    }

    fn spec(extension: &str, context: &str, line: &str) -> RegistrationExtensionSpec {
        RegistrationExtensionSpec {
            target: target(extension, context),
            line: line.into(),
            context_policy: RegistrationContextPolicy::CreateIfMissing,
        }
    }

    fn owner(device: &str, instance: u32) -> RegistrationAppearanceOwner {
        RegistrationAppearanceOwner {
            device_id: DeviceId::new(device).unwrap(),
            line_instance: instance,
        }
    }

    fn appearance(
        device: &str,
        instance: u32,
        extensions: Vec<RegistrationExtensionSpec>,
    ) -> RegistrationAppearance {
        RegistrationAppearance {
            owner: owner(device, instance),
            extensions,
        }
    }

    #[test]
    fn configured_appearances_preserve_global_and_explicit_context_policy() {
        let config = ModuleConfig::parse(
            r#"
            [general]
            advertised_address = 192.0.2.10
            regcontext = registrations&backup

            [1001]
            type = line
            regexten = 1001&91001@external

            [SEP001122334455]
            type = device
            line = 1001
            "#,
        )
        .unwrap();
        let device = DeviceId::new("SEP001122334455").unwrap();
        let appearances = configured_registration_appearances(&config, [&device]);

        assert_eq!(appearances.len(), 1);
        assert_eq!(appearances[0].owner, owner("SEP001122334455", 1));
        assert_eq!(
            appearances[0].extensions,
            [
                spec("1001", "registrations", "1001"),
                spec("1001", "backup", "1001"),
                RegistrationExtensionSpec {
                    target: target("91001", "external"),
                    line: "1001".into(),
                    context_policy: RegistrationContextPolicy::RequireExisting,
                },
            ]
        );
    }

    #[test]
    fn shared_line_and_duplicate_owner_targets_publish_and_remove_once() {
        let backend = FakeBackend::default();
        let mut registry = RegistrationContextRegistry::new(backend.clone());
        let extension = spec("1001", "registrations", "1001");
        let first = appearance(
            "SEP001122334455",
            1,
            vec![extension.clone(), extension.clone()],
        );
        let second = appearance("SEP112233445566", 2, vec![extension.clone()]);

        registry.reconcile([first.clone(), second]).unwrap();
        assert_eq!(registry.active_target_count(), 1);
        assert_eq!(backend.calls(), [Call::Publish(extension.clone())]);

        registry.reconcile([first]).unwrap();
        assert_eq!(backend.calls(), [Call::Publish(extension.clone())]);
        registry.clear().unwrap();
        assert_eq!(
            backend.calls(),
            [
                Call::Publish(extension.clone()),
                Call::Unpublish(extension.target),
            ]
        );
    }

    #[test]
    fn reconnect_is_idempotent_and_reload_add_replace_remove_is_ordered() {
        let backend = FakeBackend::default();
        let mut registry = RegistrationContextRegistry::new(backend.clone());
        let old = spec("1001", "old", "1001");
        let updated = spec("1001", "old", "line-one");
        let added = spec("1001", "new", "line-one");
        let current = appearance("SEP001122334455", 1, vec![old.clone()]);

        registry.reconcile([current.clone()]).unwrap();
        registry.reconcile([current]).unwrap();
        registry
            .reconcile([appearance(
                "SEP001122334455",
                1,
                vec![updated.clone(), added.clone()],
            )])
            .unwrap();
        registry
            .reconcile([appearance("SEP001122334455", 1, vec![added.clone()])])
            .unwrap();

        assert_eq!(
            backend.calls(),
            [
                Call::Publish(old),
                Call::Publish(added.clone()),
                Call::Replace(updated.clone()),
                Call::Unpublish(updated.target),
            ]
        );
        assert_eq!(
            backend.published().keys().cloned().collect::<Vec<_>>(),
            [added.target]
        );
    }

    #[test]
    fn conflicting_lines_fail_before_the_backend_is_touched() {
        let backend = FakeBackend::default();
        let mut registry = RegistrationContextRegistry::new(backend.clone());
        let error = registry
            .reconcile([
                appearance(
                    "SEP001122334455",
                    1,
                    vec![spec("shared", "registrations", "1001")],
                ),
                appearance(
                    "SEP112233445566",
                    1,
                    vec![spec("shared", "registrations", "1002")],
                ),
            ])
            .unwrap_err();

        assert_eq!(error, RegistrationRegistryError::ConflictingTarget);
        assert!(backend.calls().is_empty());
    }

    #[test]
    fn partial_publish_failure_rolls_back_every_staged_target() {
        let backend = FakeBackend::default();
        backend.fail_at(2, RegistrationExtensionBackendError::Conflict);
        let mut registry = RegistrationContextRegistry::new(backend.clone());
        let first = spec("1001", "one", "1001");
        let second = spec("1001", "two", "1001");

        let error = registry
            .reconcile([appearance(
                "SEP001122334455",
                1,
                vec![first.clone(), second],
            )])
            .unwrap_err();

        assert_eq!(
            error,
            RegistrationRegistryError::Backend {
                operation: RegistrationRegistryOperation::Publish,
                source: RegistrationExtensionBackendError::Conflict,
            }
        );
        assert_eq!(backend.calls().last(), Some(&Call::Unpublish(first.target)));
        assert!(backend.published().is_empty());
        assert_eq!(registry.active_target_count(), 0);
    }

    #[test]
    fn partial_remove_failure_restores_the_previous_snapshot() {
        let backend = FakeBackend::default();
        let mut registry = RegistrationContextRegistry::new(backend.clone());
        let first = spec("1001", "one", "1001");
        let second = spec("1001", "two", "1001");
        registry
            .reconcile([appearance("SEP001122334455", 1, vec![first, second])])
            .unwrap();
        backend.fail_at(4, RegistrationExtensionBackendError::Failed);

        let error = registry.clear().unwrap_err();

        assert_eq!(
            error,
            RegistrationRegistryError::Backend {
                operation: RegistrationRegistryOperation::Unpublish,
                source: RegistrationExtensionBackendError::Failed,
            }
        );
        assert_eq!(registry.active_target_count(), 2);
        assert_eq!(backend.published().len(), 2);
    }

    #[test]
    fn missing_replacement_target_is_repaired_before_reporting_failure() {
        let backend = FakeBackend::default();
        let mut registry = RegistrationContextRegistry::new(backend.clone());
        let old = spec("1001", "registrations", "1001");
        let updated = spec("1001", "registrations", "line-one");
        registry
            .reconcile([appearance("SEP001122334455", 1, vec![old.clone()])])
            .unwrap();
        backend.remove_published(&old.target);
        backend.fail_at(2, RegistrationExtensionBackendError::NotFound);

        assert_eq!(
            registry
                .reconcile([appearance("SEP001122334455", 1, vec![updated],)])
                .unwrap_err(),
            RegistrationRegistryError::Backend {
                operation: RegistrationRegistryOperation::Replace,
                source: RegistrationExtensionBackendError::NotFound,
            }
        );
        assert_eq!(backend.published().get(&old.target), Some(&old));
        assert_eq!(registry.active_target_count(), 1);
    }

    #[test]
    fn rollback_failure_poisoning_rejects_reuse_and_drop_retries_cleanup() {
        let backend = FakeBackend::default();
        let first = spec("1001", "one", "1001");
        let second = spec("1001", "two", "1001");
        {
            let mut registry = RegistrationContextRegistry::new(backend.clone());
            backend.fail_at(2, RegistrationExtensionBackendError::Conflict);
            backend.fail_at(3, RegistrationExtensionBackendError::Failed);
            assert_eq!(
                registry
                    .reconcile([appearance(
                        "SEP001122334455",
                        1,
                        vec![first.clone(), second],
                    )])
                    .unwrap_err(),
                RegistrationRegistryError::RollbackFailed {
                    operation: RegistrationRegistryOperation::Publish,
                    source: RegistrationExtensionBackendError::Conflict,
                    rollback_source: RegistrationExtensionBackendError::Failed,
                }
            );
            assert_eq!(
                registry.clear().unwrap_err(),
                RegistrationRegistryError::Diverged
            );
        }
        assert!(backend.published().is_empty());
        assert!(backend.calls().contains(&Call::Unpublish(first.target)));
    }

    #[test]
    fn drop_unpublishes_every_live_target_for_unload() {
        let backend = FakeBackend::default();
        let first = spec("1001", "one", "1001");
        let second = spec("1001", "two", "1001");
        {
            let mut registry = RegistrationContextRegistry::new(backend.clone());
            registry
                .reconcile([appearance(
                    "SEP001122334455",
                    1,
                    vec![first.clone(), second.clone()],
                )])
                .unwrap();
        }
        assert!(backend.published().is_empty());
        assert_eq!(
            backend.calls()[2..],
            [
                Call::Unpublish(first.target),
                Call::Unpublish(second.target),
            ]
        );
    }

    #[test]
    fn unload_waits_for_an_inflight_reconcile_before_removing_targets() {
        let backend = BlockingBackend::default();
        let calls = backend.backend.clone();
        let registry = Arc::new(Mutex::new(RegistrationContextRegistry::new(
            backend.clone(),
        )));
        let publishing_registry = Arc::clone(&registry);
        let publishing = thread::spawn(move || {
            publishing_registry
                .lock()
                .unwrap()
                .reconcile([appearance(
                    "SEP001122334455",
                    1,
                    vec![spec("1001", "registrations", "1001")],
                )])
                .unwrap();
        });
        backend.wait_until_publish_started();

        let (cleared_tx, cleared_rx) = mpsc::channel();
        let clearing_registry = registry;
        let clearing = thread::spawn(move || {
            clearing_registry.lock().unwrap().clear().unwrap();
            cleared_tx.send(()).unwrap();
        });
        assert!(cleared_rx.recv_timeout(Duration::from_millis(20)).is_err());

        backend.release_publish();
        publishing.join().unwrap();
        clearing.join().unwrap();
        cleared_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            calls.calls(),
            [
                Call::Publish(spec("1001", "registrations", "1001")),
                Call::Unpublish(target("1001", "registrations")),
            ]
        );
    }
}
