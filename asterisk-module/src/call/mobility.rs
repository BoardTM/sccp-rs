//! Transaction state for Extension Mobility line appearances.
//!
//! The registry is deliberately independent of the SCCP transport and the
//! Asterisk adapter. Preparing a change reserves its device/button slot and
//! logical line, but leaves the committed snapshot untouched while the caller
//! performs handset I/O. The caller commits only after every required handset
//! update succeeds, or aborts after restoring any appearance it already
//! removed. This keeps controller and registry locks out of transport I/O.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use sccp_protocol::{
    AppearanceId, CiscoIpPhoneInput, CiscoIpPhoneInputItem, DeviceId, LineAppearance,
    LineDefinition, PhoneInputFlags, PhoneInputParameterName, PhoneServiceSubmission,
    PhoneXmlError, Tone,
};
use thiserror::Error;

use crate::config::{LineBinding, LineConfig, MAX_MOBILITY_PIN_DIGITS, ModuleConfig};

/// Maximum line instance that can be represented in the station button
/// template. The SCCP definition boundary enforces the same 56-button limit.
pub const MAX_MOBILITY_LINE_INSTANCE: u32 = 56;
pub const MOBILITY_APPLICATION_ID: u32 = 9_092;

const MOBILITY_LINE_PARAMETER: &str = "LINE";
const MOBILITY_PIN_PARAMETER: &str = "PIN";

/// A bounded PIN candidate whose diagnostics never expose the submitted text.
#[derive(Clone, Eq, PartialEq)]
pub struct MobilityCredential(String);

impl MobilityCredential {
    pub fn new(value: impl Into<String>) -> Result<Self, MobilityCredentialError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_MOBILITY_PIN_DIGITS
            || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(MobilityCredentialError::InvalidFormat);
        }
        Ok(Self(value))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for MobilityCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MobilityCredential(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MobilityCredentialError {
    #[error("mobility credential must contain one to seven ASCII digits")]
    InvalidFormat,
}

/// Authenticate a logical line without distinguishing an unknown line, a
/// disabled mobility policy, and an incorrect PIN.
pub fn authenticate_line(
    config: &ModuleConfig,
    line_number: &str,
    credential: &MobilityCredential,
) -> Result<LineConfig, MobilityAuthenticationError> {
    let Some(line) = config.lines.get(line_number) else {
        return Err(MobilityAuthenticationError::Denied);
    };
    let Some(pin) = config
        .mobility_for_line(line_number)
        .and_then(|mobility| mobility.pin.as_ref())
    else {
        return Err(MobilityAuthenticationError::Denied);
    };
    pin.verify(credential.as_str())
        .then(|| line.clone())
        .ok_or(MobilityAuthenticationError::Denied)
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MobilityAuthenticationError {
    #[error("mobility credentials were not accepted")]
    Denied,
}

/// Typed login data extracted from the redaction-safe phone-service boundary.
#[derive(Eq, PartialEq)]
pub struct MobilityLoginRequest {
    line_number: String,
    credential: MobilityCredential,
}

impl MobilityLoginRequest {
    pub fn line_number(&self) -> &str {
        &self.line_number
    }

    pub fn credential(&self) -> &MobilityCredential {
        &self.credential
    }
}

impl fmt::Debug for MobilityLoginRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MobilityLoginRequest")
            .field("line_number", &self.line_number)
            .field("credential", &"<redacted>")
            .finish()
    }
}

/// Build the known phone input schema. The adapter sends the returned typed
/// value through the shared Serde/quick-xml boundary.
pub fn mobility_login_document(button_instance: u32) -> Result<CiscoIpPhoneInput, PhoneXmlError> {
    CiscoIpPhoneInput::new(
        "Extension Mobility",
        "Enter line and PIN",
        format!("mobility/{button_instance}/login"),
        vec![
            CiscoIpPhoneInputItem {
                display_name: Some("Line".into()),
                parameter: PhoneInputParameterName::new(MOBILITY_LINE_PARAMETER)?,
                flags: PhoneInputFlags::Telephone,
                default_value: None,
            },
            CiscoIpPhoneInputItem {
                display_name: Some("PIN".into()),
                parameter: PhoneInputParameterName::new(MOBILITY_PIN_PARAMETER)?,
                flags: PhoneInputFlags::NumericPassword,
                default_value: None,
            },
        ],
    )
}

/// Accept exactly the route and two unique fields issued for one button.
pub fn parse_mobility_login_submission(
    button_instance: u32,
    submission: &PhoneServiceSubmission,
) -> Result<MobilityLoginRequest, MobilitySubmissionError> {
    let expected_instance = button_instance.to_string();
    if submission.route.as_slice() != ["mobility", expected_instance.as_str(), "login"] {
        return Err(MobilitySubmissionError::InvalidEnvelope);
    }
    let lines = submission
        .values_named(MOBILITY_LINE_PARAMETER)
        .collect::<Vec<_>>();
    let pins = submission
        .values_named(MOBILITY_PIN_PARAMETER)
        .collect::<Vec<_>>();
    if submission.values.len() != 2 || lines.len() != 1 || pins.len() != 1 || lines[0].is_empty() {
        return Err(MobilitySubmissionError::InvalidEnvelope);
    }
    let credential =
        MobilityCredential::new(pins[0]).map_err(|_| MobilitySubmissionError::InvalidCredential)?;
    Ok(MobilityLoginRequest {
        line_number: lines[0].to_owned(),
        credential,
    })
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MobilitySubmissionError {
    #[error("mobility submission route or fields were not accepted")]
    InvalidEnvelope,
    #[error("mobility credential format was not accepted")]
    InvalidCredential,
}

/// One configured Mobility feature button on one station.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MobilitySlot {
    pub device_id: DeviceId,
    pub button_instance: u32,
}

impl MobilitySlot {
    pub fn new(device_id: DeviceId, button_instance: u32) -> Result<Self, MobilityRegistryError> {
        if button_instance == 0 {
            return Err(MobilityRegistryError::InvalidButtonInstance);
        }
        Ok(Self {
            device_id,
            button_instance,
        })
    }
}

/// One committed dynamic line appearance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoamingAppearance {
    pub slot: MobilitySlot,
    pub binding: LineBinding,
}

/// Monotonic identity for a prepared mutation. It prevents a late completion
/// from committing a newer transaction that reused the same slot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MobilityTransactionId(u64);

/// A prepared mutation. `previous` is removed first; `next` is then installed.
/// On install failure the caller must restore `previous` before aborting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedMobilityTransaction {
    id: MobilityTransactionId,
    previous: Option<RoamingAppearance>,
    next: Option<RoamingAppearance>,
}

impl PreparedMobilityTransaction {
    pub fn id(&self) -> MobilityTransactionId {
        self.id
    }

    pub fn previous(&self) -> Option<&RoamingAppearance> {
        self.previous.as_ref()
    }

    pub fn next(&self) -> Option<&RoamingAppearance> {
        self.next.as_ref()
    }
}

/// Policy-free handset writer used by the transaction executor. Implementors
/// may treat removal from an offline source as success, but installation must
/// confirm that the target phone accepted the complete update.
pub trait MobilityAppearanceWriter {
    type Error;

    fn write<'a>(
        &'a mut self,
        appearance: &'a RoamingAppearance,
        install: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>>;
}

/// Apply the phone-facing portion of a prepared transaction. If installation
/// fails after removing a previous appearance, that appearance is restored
/// before an error is returned.
pub async fn execute_mobility_io<W: MobilityAppearanceWriter>(
    writer: &mut W,
    transaction: &PreparedMobilityTransaction,
) -> Result<(), MobilityIoError> {
    let removed = if let Some(previous) = transaction.previous() {
        writer
            .write(previous, false)
            .await
            .map_err(|_| MobilityIoError::RemoveFailed)?;
        true
    } else {
        false
    };
    if let Some(next) = transaction.next()
        && writer.write(next, true).await.is_err()
    {
        if removed
            && writer
                .write(
                    transaction
                        .previous()
                        .expect("removed transaction has a previous appearance"),
                    true,
                )
                .await
                .is_err()
        {
            return Err(MobilityIoError::RollbackFailed);
        }
        return Err(MobilityIoError::InstallFailed);
    }
    Ok(())
}

/// Reverse the phone-facing part after an unexpected commit failure.
pub async fn rollback_mobility_io<W: MobilityAppearanceWriter>(
    writer: &mut W,
    transaction: &PreparedMobilityTransaction,
) -> Result<(), MobilityIoError> {
    if let Some(next) = transaction.next() {
        writer
            .write(next, false)
            .await
            .map_err(|_| MobilityIoError::RollbackFailed)?;
    }
    if let Some(previous) = transaction.previous() {
        writer
            .write(previous, true)
            .await
            .map_err(|_| MobilityIoError::RollbackFailed)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MobilityIoError {
    #[error("unable to remove the previous roaming appearance")]
    RemoveFailed,
    #[error("unable to install the next roaming appearance")]
    InstallFailed,
    #[error("unable to restore roaming appearance state after a failed mutation")]
    RollbackFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MobilityPreparation {
    Unchanged(Box<RoamingAppearance>),
    Transaction(Box<PreparedMobilityTransaction>),
}

/// Committed and pending Extension Mobility state.
#[derive(Debug, Default)]
pub struct MobilityRegistry {
    next_transaction_id: u64,
    by_slot: BTreeMap<MobilitySlot, RoamingAppearance>,
    slot_by_line: HashMap<String, MobilitySlot>,
    pending: HashMap<MobilityTransactionId, PreparedMobilityTransaction>,
    reserved_slots: HashMap<MobilitySlot, MobilityTransactionId>,
    reserved_lines: HashMap<String, MobilityTransactionId>,
}

impl MobilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Prepare an authenticated login. A line already roaming elsewhere is
    /// moved atomically; an occupied destination button is never overwritten.
    pub fn prepare_login(
        &mut self,
        slot: MobilitySlot,
        line: LineConfig,
        configured_line_instances: impl IntoIterator<Item = u32>,
    ) -> Result<MobilityPreparation, MobilityRegistryError> {
        if let Some(existing) = self.by_slot.get(&slot) {
            if existing.binding.line.number == line.number {
                return Ok(MobilityPreparation::Unchanged(Box::new(existing.clone())));
            }
            return Err(MobilityRegistryError::SlotOccupied);
        }

        let previous = self
            .slot_by_line
            .get(&line.number)
            .and_then(|source| self.by_slot.get(source))
            .cloned();
        self.ensure_available(
            &slot,
            &line.number,
            previous.as_ref().map(|value| &value.slot),
        )?;

        let mut occupied: BTreeSet<u32> = configured_line_instances.into_iter().collect();
        occupied.extend(
            self.by_slot
                .values()
                .filter(|appearance| appearance.slot.device_id == slot.device_id)
                .map(|appearance| appearance.binding.line_instance),
        );
        let line_instance = (1..=MAX_MOBILITY_LINE_INSTANCE)
            .find(|instance| !occupied.contains(instance))
            .ok_or(MobilityRegistryError::NoLineInstanceAvailable)?;
        let appearance = LineAppearance {
            id: AppearanceId::new(line_instance),
            instance: line_instance,
            line: LineDefinition {
                number: line.number.clone(),
                display_name: line.label.clone(),
            },
            label: None,
            caller_id: Default::default(),
            ring_mode: Default::default(),
            initial_tone: Tone::InsideDial,
            subscription_identity: None,
            privacy: false,
        };
        let next = RoamingAppearance {
            slot: slot.clone(),
            binding: LineBinding {
                device_id: slot.device_id.clone(),
                line_instance,
                appearance,
                line,
            },
        };
        let transaction = self.reserve(previous, Some(next))?;
        Ok(MobilityPreparation::Transaction(Box::new(transaction)))
    }

    /// Prepare logout from one exact Mobility button.
    pub fn prepare_logout(
        &mut self,
        slot: &MobilitySlot,
    ) -> Result<PreparedMobilityTransaction, MobilityRegistryError> {
        let previous = self
            .by_slot
            .get(slot)
            .cloned()
            .ok_or(MobilityRegistryError::NotLoggedIn)?;
        self.ensure_available(slot, &previous.binding.line.number, None)?;
        self.reserve(Some(previous), None)
    }

    /// Commit only the exact still-pending transaction.
    pub fn commit(
        &mut self,
        transaction: &PreparedMobilityTransaction,
    ) -> Result<(), MobilityRegistryError> {
        let Some(pending) = self.pending.get(&transaction.id) else {
            return Err(MobilityRegistryError::StaleTransaction);
        };
        if pending != transaction {
            return Err(MobilityRegistryError::StaleTransaction);
        }
        let pending = self
            .pending
            .remove(&transaction.id)
            .expect("pending transaction disappeared while locked");
        self.release_reservations(&pending);
        if let Some(previous) = pending.previous {
            self.by_slot.remove(&previous.slot);
            self.slot_by_line.remove(&previous.binding.line.number);
        }
        if let Some(next) = pending.next {
            self.slot_by_line
                .insert(next.binding.line.number.clone(), next.slot.clone());
            self.by_slot.insert(next.slot.clone(), next);
        }
        Ok(())
    }

    /// Abort one exact pending transaction without changing committed state.
    pub fn abort(
        &mut self,
        transaction: &PreparedMobilityTransaction,
    ) -> Result<(), MobilityRegistryError> {
        let Some(pending) = self.pending.get(&transaction.id) else {
            return Err(MobilityRegistryError::StaleTransaction);
        };
        if pending != transaction {
            return Err(MobilityRegistryError::StaleTransaction);
        }
        let pending = self
            .pending
            .remove(&transaction.id)
            .expect("pending transaction disappeared while locked");
        self.release_reservations(&pending);
        Ok(())
    }

    pub fn appearance_for_slot(&self, slot: &MobilitySlot) -> Option<&RoamingAppearance> {
        self.by_slot.get(slot)
    }

    pub fn binding_for_device(
        &self,
        device_id: &DeviceId,
        line_instance: u32,
    ) -> Option<&LineBinding> {
        self.by_slot
            .values()
            .find(|appearance| {
                &appearance.slot.device_id == device_id
                    && appearance.binding.line_instance == line_instance
            })
            .map(|appearance| &appearance.binding)
    }

    pub fn appearances_for_device(
        &self,
        device_id: &DeviceId,
    ) -> impl Iterator<Item = &RoamingAppearance> {
        self.by_slot
            .values()
            .filter(move |appearance| &appearance.slot.device_id == device_id)
    }

    pub fn appearances_for_line(
        &self,
        line_number: &str,
    ) -> impl Iterator<Item = &RoamingAppearance> {
        self.slot_by_line
            .get(line_number)
            .and_then(|slot| self.by_slot.get(slot))
            .into_iter()
    }

    pub fn committed(&self) -> impl Iterator<Item = &RoamingAppearance> {
        self.by_slot.values()
    }

    pub fn has_pending_transaction(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Remove committed appearances that are no longer valid in a replacement
    /// configuration. Reload calls this only after rejecting an in-flight
    /// transaction, so no reserved slot can be invalidated underneath I/O.
    pub fn remove_invalid(
        &mut self,
        mut valid: impl FnMut(&RoamingAppearance) -> bool,
    ) -> Result<Vec<RoamingAppearance>, MobilityRegistryError> {
        if self.has_pending_transaction() {
            return Err(MobilityRegistryError::TransactionInProgress);
        }
        let invalid = self
            .by_slot
            .values()
            .filter(|appearance| !valid(appearance))
            .cloned()
            .collect::<Vec<_>>();
        for appearance in &invalid {
            self.by_slot.remove(&appearance.slot);
            self.slot_by_line.remove(&appearance.binding.line.number);
        }
        Ok(invalid)
    }

    fn ensure_available(
        &self,
        slot: &MobilitySlot,
        line_number: &str,
        previous_slot: Option<&MobilitySlot>,
    ) -> Result<(), MobilityRegistryError> {
        if self.reserved_slots.contains_key(slot)
            || previous_slot.is_some_and(|source| self.reserved_slots.contains_key(source))
            || self.reserved_lines.contains_key(line_number)
        {
            return Err(MobilityRegistryError::TransactionInProgress);
        }
        Ok(())
    }

    fn reserve(
        &mut self,
        previous: Option<RoamingAppearance>,
        next: Option<RoamingAppearance>,
    ) -> Result<PreparedMobilityTransaction, MobilityRegistryError> {
        self.next_transaction_id = self
            .next_transaction_id
            .checked_add(1)
            .ok_or(MobilityRegistryError::TransactionIdExhausted)?;
        let id = MobilityTransactionId(self.next_transaction_id);
        let transaction = PreparedMobilityTransaction { id, previous, next };
        if let Some(previous) = &transaction.previous {
            self.reserved_slots.insert(previous.slot.clone(), id);
            self.reserved_lines
                .insert(previous.binding.line.number.clone(), id);
        }
        if let Some(next) = &transaction.next {
            self.reserved_slots.insert(next.slot.clone(), id);
            self.reserved_lines
                .insert(next.binding.line.number.clone(), id);
        }
        self.pending.insert(id, transaction.clone());
        Ok(transaction)
    }

    fn release_reservations(&mut self, transaction: &PreparedMobilityTransaction) {
        if let Some(previous) = &transaction.previous {
            self.reserved_slots.remove(&previous.slot);
            self.reserved_lines.remove(&previous.binding.line.number);
        }
        if let Some(next) = &transaction.next {
            self.reserved_slots.remove(&next.slot);
            self.reserved_lines.remove(&next.binding.line.number);
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum MobilityRegistryError {
    #[error("mobility button instance must be nonzero")]
    InvalidButtonInstance,
    #[error("mobility button already has a roaming appearance")]
    SlotOccupied,
    #[error("mobility button has no roaming appearance")]
    NotLoggedIn,
    #[error("mobility mutation conflicts with another in-flight mutation")]
    TransactionInProgress,
    #[error("device has no free line instance for a roaming appearance")]
    NoLineInstanceAvailable,
    #[error("mobility transaction identity space is exhausted")]
    TransactionIdExhausted,
    #[error("mobility transaction is no longer pending")]
    StaleTransaction,
}

#[cfg(test)]
mod tests {
    use super::*;
    use sccp_protocol::PhoneServiceSubmittedValue;

    #[derive(Default)]
    struct FakeWriter {
        writes: Vec<(MobilitySlot, bool)>,
        fail_at: Vec<usize>,
    }

    impl MobilityAppearanceWriter for FakeWriter {
        type Error = ();

        fn write<'a>(
            &'a mut self,
            appearance: &'a RoamingAppearance,
            install: bool,
        ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
            Box::pin(async move {
                self.writes.push((appearance.slot.clone(), install));
                if self.fail_at.contains(&self.writes.len()) {
                    Err(())
                } else {
                    Ok(())
                }
            })
        }
    }

    const CONFIG: &str = r#"
[general]
bind = 127.0.0.1:2000
advertised_address = 127.0.0.1

[1001]
type = line
label = Alice
pin = 0123456

[1002]
type = line
label = No Mobility

[SEP001122334455]
type = device
button = line,1001
button = feature, Mobility, mobility

[SEP112233445566]
type = device
button = line,1002
button = feature, Mobility, mobility
"#;

    fn device(value: &str) -> DeviceId {
        DeviceId::new(value).unwrap()
    }

    fn slot(value: &str, instance: u32) -> MobilitySlot {
        MobilitySlot::new(device(value), instance).unwrap()
    }

    fn line(config: &ModuleConfig, number: &str) -> LineConfig {
        config.lines.get(number).unwrap().clone()
    }

    fn transaction(preparation: MobilityPreparation) -> PreparedMobilityTransaction {
        let MobilityPreparation::Transaction(transaction) = preparation else {
            panic!("expected a prepared transaction");
        };
        *transaction
    }

    #[test]
    fn credential_is_bounded_redacted_and_authentication_is_uniform() {
        let config = ModuleConfig::parse(CONFIG).unwrap();
        let exact = MobilityCredential::new("0123456").unwrap();
        assert_eq!(format!("{exact:?}"), "MobilityCredential(<redacted>)");
        assert_eq!(
            authenticate_line(&config, "1001", &exact).unwrap().number,
            "1001"
        );

        for candidate in ["1123456", "0123450", "012345"] {
            let credential = MobilityCredential::new(candidate).unwrap();
            assert_eq!(
                authenticate_line(&config, "1001", &credential),
                Err(MobilityAuthenticationError::Denied)
            );
        }
        assert_eq!(
            authenticate_line(&config, "1002", &MobilityCredential::new("1234").unwrap()),
            Err(MobilityAuthenticationError::Denied)
        );
        assert_eq!(
            authenticate_line(&config, "9999", &MobilityCredential::new("1234").unwrap()),
            Err(MobilityAuthenticationError::Denied)
        );
        assert_eq!(
            MobilityCredential::new("12x4"),
            Err(MobilityCredentialError::InvalidFormat)
        );
        assert_eq!(
            MobilityCredential::new("01234567"),
            Err(MobilityCredentialError::InvalidFormat)
        );
        assert_eq!(
            MobilityCredential::new(""),
            Err(MobilityCredentialError::InvalidFormat)
        );
    }

    #[test]
    fn typed_prompt_and_submission_enforce_exact_route_fields_and_redaction() {
        let document = mobility_login_document(4).unwrap();
        assert_eq!(document.url, "mobility/4/login");
        assert_eq!(document.items[0].flags, PhoneInputFlags::Telephone);
        assert_eq!(document.items[1].flags, PhoneInputFlags::NumericPassword);
        let xml = document.to_xml().unwrap();
        assert!(xml.contains("<CiscoIPPhoneInput>"));
        assert!(xml.contains("<InputFlags>NP</InputFlags>"));

        let submission = PhoneServiceSubmission {
            route: vec!["mobility".into(), "4".into(), "login".into()],
            values: vec![
                PhoneServiceSubmittedValue {
                    name: "LINE".into(),
                    value: "1001".into(),
                },
                PhoneServiceSubmittedValue {
                    name: "PIN".into(),
                    value: "0123456".into(),
                },
            ],
        };
        let request = parse_mobility_login_submission(4, &submission).unwrap();
        assert_eq!(request.line_number(), "1001");
        assert_eq!(
            format!("{request:?}"),
            "MobilityLoginRequest { line_number: \"1001\", credential: \"<redacted>\" }"
        );
        assert!(!format!("{request:?}").contains("0123456"));

        let mut malformed = submission.clone();
        malformed.values.push(PhoneServiceSubmittedValue {
            name: "PIN".into(),
            value: "9999".into(),
        });
        assert_eq!(
            parse_mobility_login_submission(4, &malformed),
            Err(MobilitySubmissionError::InvalidEnvelope)
        );
        let mut replayed = submission;
        replayed.route[1] = "5".into();
        assert_eq!(
            parse_mobility_login_submission(4, &replayed),
            Err(MobilitySubmissionError::InvalidEnvelope)
        );
    }

    #[test]
    fn login_uses_first_free_instance_and_commits_only_after_explicit_success() {
        let config = ModuleConfig::parse(CONFIG).unwrap();
        let mut registry = MobilityRegistry::new();
        let target = slot("SEP112233445566", 1);
        let prepared = transaction(
            registry
                .prepare_login(target.clone(), line(&config, "1001"), [1, 3])
                .unwrap(),
        );
        assert!(registry.appearance_for_slot(&target).is_none());
        assert!(prepared.previous().is_none());
        assert_eq!(prepared.next().unwrap().binding.line_instance, 2);
        registry.commit(&prepared).unwrap();
        assert_eq!(
            registry
                .appearance_for_slot(&target)
                .unwrap()
                .binding
                .line
                .number,
            "1001"
        );

        let unchanged = registry
            .prepare_login(target, line(&config, "1001"), [1, 3])
            .unwrap();
        assert!(matches!(unchanged, MobilityPreparation::Unchanged(_)));
    }

    #[test]
    fn moving_a_line_reserves_both_slots_and_abort_preserves_the_source() {
        let config = ModuleConfig::parse(CONFIG).unwrap();
        let source = slot("SEP001122334455", 1);
        let target = slot("SEP112233445566", 1);
        let mut registry = MobilityRegistry::new();
        let initial = transaction(
            registry
                .prepare_login(source.clone(), line(&config, "1001"), [1])
                .unwrap(),
        );
        registry.commit(&initial).unwrap();

        let movement = transaction(
            registry
                .prepare_login(target.clone(), line(&config, "1001"), [1])
                .unwrap(),
        );
        assert_eq!(movement.previous().unwrap().slot, source);
        assert_eq!(movement.next().unwrap().slot, target);
        assert_eq!(
            registry.prepare_logout(&movement.previous().unwrap().slot),
            Err(MobilityRegistryError::TransactionInProgress)
        );
        registry.abort(&movement).unwrap();
        assert!(registry.appearance_for_slot(&source).is_some());
        assert!(registry.appearance_for_slot(&target).is_none());
    }

    #[test]
    fn committed_move_is_unique_by_line_and_runtime_lookups_are_deterministic() {
        let config = ModuleConfig::parse(CONFIG).unwrap();
        let source = slot("SEP001122334455", 2);
        let target = slot("SEP112233445566", 1);
        let mut registry = MobilityRegistry::new();
        let initial = transaction(
            registry
                .prepare_login(source.clone(), line(&config, "1001"), [1])
                .unwrap(),
        );
        registry.commit(&initial).unwrap();
        let movement = transaction(
            registry
                .prepare_login(target.clone(), line(&config, "1001"), [1])
                .unwrap(),
        );
        let target_instance = movement.next().unwrap().binding.line_instance;
        registry.commit(&movement).unwrap();

        assert!(registry.appearance_for_slot(&source).is_none());
        assert_eq!(registry.appearances_for_line("1001").count(), 1);
        assert_eq!(
            registry
                .binding_for_device(&target.device_id, target_instance)
                .unwrap()
                .line
                .number,
            "1001"
        );
        assert_eq!(
            registry
                .appearances_for_device(&target.device_id)
                .map(|appearance| appearance.slot.button_instance)
                .collect::<Vec<_>>(),
            [1]
        );
    }

    #[test]
    fn logout_is_prepared_then_committed_and_late_completions_are_rejected() {
        let config = ModuleConfig::parse(CONFIG).unwrap();
        let target = slot("SEP112233445566", 1);
        let mut registry = MobilityRegistry::new();
        let login = transaction(
            registry
                .prepare_login(target.clone(), line(&config, "1001"), [1])
                .unwrap(),
        );
        registry.commit(&login).unwrap();
        assert_eq!(
            registry.commit(&login),
            Err(MobilityRegistryError::StaleTransaction)
        );

        let logout = registry.prepare_logout(&target).unwrap();
        assert!(logout.next().is_none());
        assert!(registry.appearance_for_slot(&target).is_some());
        registry.commit(&logout).unwrap();
        assert!(registry.appearance_for_slot(&target).is_none());
        assert_eq!(
            registry.prepare_logout(&target),
            Err(MobilityRegistryError::NotLoggedIn)
        );
    }

    #[test]
    fn capacity_and_slot_replacement_fail_without_mutating_committed_state() {
        let config = ModuleConfig::parse(CONFIG).unwrap();
        let target = slot("SEP112233445566", 1);
        let mut registry = MobilityRegistry::new();
        assert_eq!(
            registry.prepare_login(
                target.clone(),
                line(&config, "1001"),
                1..=MAX_MOBILITY_LINE_INSTANCE,
            ),
            Err(MobilityRegistryError::NoLineInstanceAvailable)
        );
        let login = transaction(
            registry
                .prepare_login(target.clone(), line(&config, "1001"), [1])
                .unwrap(),
        );
        registry.commit(&login).unwrap();
        assert_eq!(
            registry.prepare_login(target.clone(), line(&config, "1002"), [1]),
            Err(MobilityRegistryError::SlotOccupied)
        );
        assert_eq!(
            registry
                .appearance_for_slot(&target)
                .unwrap()
                .binding
                .line
                .number,
            "1001"
        );
    }

    #[test]
    fn concurrent_transactions_serialize_shared_resources_but_allow_disjoint_lines() {
        let config = ModuleConfig::parse(CONFIG).unwrap();
        let first_slot = slot("SEP112233445566", 1);
        let second_slot = slot("SEP001122334455", 2);
        let mut registry = MobilityRegistry::new();
        let first = transaction(
            registry
                .prepare_login(first_slot, line(&config, "1001"), [1])
                .unwrap(),
        );
        assert_eq!(
            registry.prepare_login(second_slot.clone(), line(&config, "1001"), [1]),
            Err(MobilityRegistryError::TransactionInProgress)
        );
        let disjoint = transaction(
            registry
                .prepare_login(second_slot, line(&config, "1002"), [1])
                .unwrap(),
        );
        registry.commit(&disjoint).unwrap();
        registry.abort(&first).unwrap();
        assert_eq!(registry.committed().count(), 1);
    }

    #[tokio::test]
    async fn fake_writer_orders_move_and_restores_source_on_install_failure() {
        let config = ModuleConfig::parse(CONFIG).unwrap();
        let source = slot("SEP001122334455", 2);
        let target = slot("SEP112233445566", 1);
        let mut registry = MobilityRegistry::new();
        let initial = transaction(
            registry
                .prepare_login(source.clone(), line(&config, "1001"), [1])
                .unwrap(),
        );
        registry.commit(&initial).unwrap();
        let movement = transaction(
            registry
                .prepare_login(target.clone(), line(&config, "1001"), [1])
                .unwrap(),
        );

        let mut writer = FakeWriter {
            fail_at: vec![2],
            ..FakeWriter::default()
        };
        assert_eq!(
            execute_mobility_io(&mut writer, &movement).await,
            Err(MobilityIoError::InstallFailed)
        );
        assert_eq!(
            writer.writes,
            [
                (source.clone(), false),
                (target, true),
                (source.clone(), true),
            ]
        );
        registry.abort(&movement).unwrap();
        assert!(registry.appearance_for_slot(&source).is_some());

        let retry = transaction(
            registry
                .prepare_login(slot("SEP112233445566", 1), line(&config, "1001"), [1])
                .unwrap(),
        );
        let mut writer = FakeWriter {
            fail_at: vec![2, 3],
            ..FakeWriter::default()
        };
        assert_eq!(
            execute_mobility_io(&mut writer, &retry).await,
            Err(MobilityIoError::RollbackFailed)
        );
        registry.abort(&retry).unwrap();
        assert!(registry.appearance_for_slot(&source).is_some());
    }

    #[test]
    fn reload_reconciliation_is_all_or_nothing_while_a_transaction_is_pending() {
        let config = ModuleConfig::parse(CONFIG).unwrap();
        let first_slot = slot("SEP001122334455", 2);
        let second_slot = slot("SEP112233445566", 1);
        let mut registry = MobilityRegistry::new();
        let first = transaction(
            registry
                .prepare_login(first_slot.clone(), line(&config, "1001"), [1])
                .unwrap(),
        );
        registry.commit(&first).unwrap();
        let pending = transaction(
            registry
                .prepare_login(second_slot, line(&config, "1002"), [1])
                .unwrap(),
        );
        assert_eq!(
            registry.remove_invalid(|_| false),
            Err(MobilityRegistryError::TransactionInProgress)
        );
        assert!(registry.appearance_for_slot(&first_slot).is_some());
        registry.abort(&pending).unwrap();
        assert_eq!(registry.remove_invalid(|_| false).unwrap().len(), 1);
        assert!(registry.committed().next().is_none());
    }
}
