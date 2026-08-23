//! Typed Asterisk party and channel-metadata operations.
//!
//! Domain values cross this module as owned Rust structs. This adapter alone
//! converts them to Asterisk party fields and allocated strings. Channel locks,
//! paired locks, party IDs, and Asterisk-allocated strings are RAII-owned, so
//! every typed error path releases partial state without integer status codes.

use std::ffi::{CStr, CString, c_char};
use std::ptr::{self, NonNull};

use crate::asterisk::raw::handles::{
    AsteriskAllocation, AsteriskString as AstString, BorrowedChannelLock as ChannelLock,
    ChannelLockError,
};
use crate::asterisk::sys;
use crate::call::metadata::{CallMetadata, MAX_LANGUAGE_BYTES, MAX_PARTY_TEXT_BYTES};
use crate::pbx::channel_metadata::{ChannelMetadataBackend, ChannelMetadataError};
use crate::pbx::party::{
    AsteriskChannel, ConnectedLineSource, ConnectedLineUpdate, Delivery, NameCharset, NumberPlan,
    PartyIdentity, PartySnapshot, PartyUpdateBackend, PartyUpdateError, Presentation,
    RedirectReasonCode, RedirectingUpdate,
};

const PARTY_SNAPSHOT_MAX_BYTES: usize = 255;

struct ChannelPairLock {
    _second: ChannelLock,
    _first: ChannelLock,
}

impl ChannelPairLock {
    unsafe fn acquire(
        first: NonNull<sys::ast_channel>,
        second: NonNull<sys::ast_channel>,
    ) -> Result<Self, ChannelLockError> {
        let mut first_lock = unsafe { ChannelLock::acquire(first) }?;
        loop {
            if let Ok(second_lock) = unsafe { ChannelLock::try_acquire(second) } {
                return Ok(Self {
                    _second: second_lock,
                    _first: first_lock,
                });
            }
            drop(first_lock);
            std::thread::yield_now();
            first_lock = unsafe { ChannelLock::acquire(first) }?;
        }
    }
}

unsafe fn ast_strdup(value: &CStr) -> *mut c_char {
    unsafe { AstString::duplicate(value) }.map_or(ptr::null_mut(), AstString::take)
}

struct OwnedPartyId {
    value: sys::ast_party_id,
    owned: bool,
}

impl OwnedPartyId {
    unsafe fn new() -> Self {
        let mut value = unsafe { std::mem::zeroed() };
        unsafe { sys::ast_party_id_init(&mut value) };
        Self { value, owned: true }
    }

    fn build_party(
        source: &PartyIdentity,
        name_field: &'static str,
        number_field: &'static str,
    ) -> Result<Self, PartyUpdateError> {
        let name = party_text(name_field, source.name.as_deref())?;
        let number = party_text(number_field, source.number.as_deref())?;
        let mut party = unsafe { Self::new() };
        party.value.name.char_set = source.name_charset.raw();
        party.value.name.presentation = source.name_presentation.raw();
        party.value.name.valid = u8::from(name.is_some());
        if let Some(name) = name {
            party.value.name.str_ = unsafe { ast_strdup(&name) };
            if party.value.name.str_.is_null() {
                return Err(PartyUpdateError::NativeFailure {
                    operation: "party name allocation",
                });
            }
        }
        party.value.number.plan = source.number_plan.raw();
        party.value.number.presentation = source.number_presentation.raw();
        party.value.number.valid = u8::from(number.is_some());
        if let Some(number) = number {
            party.value.number.str_ = unsafe { ast_strdup(&number) };
            if party.value.number.str_.is_null() {
                return Err(PartyUpdateError::NativeFailure {
                    operation: "party number allocation",
                });
            }
        }
        Ok(party)
    }

    fn build_metadata(
        source: &PartyIdentity,
        operation: &'static str,
    ) -> Result<Self, ChannelMetadataError> {
        let name = metadata_text(source.name.as_deref(), operation)?;
        let number = metadata_text(source.number.as_deref(), operation)?;
        let mut party = unsafe { Self::new() };
        party.value.name.char_set = source.name_charset.raw();
        party.value.name.presentation = source.name_presentation.raw();
        party.value.name.valid = u8::from(name.is_some());
        if let Some(name) = name {
            party.value.name.str_ = unsafe { ast_strdup(&name) };
            if party.value.name.str_.is_null() {
                return Err(ChannelMetadataError::NativeFailure { operation });
            }
        }
        party.value.number.plan = source.number_plan.raw();
        party.value.number.presentation = source.number_presentation.raw();
        party.value.number.valid = u8::from(number.is_some());
        if let Some(number) = number {
            party.value.number.str_ = unsafe { ast_strdup(&number) };
            if party.value.number.str_.is_null() {
                return Err(ChannelMetadataError::NativeFailure { operation });
            }
        }
        Ok(party)
    }

    fn take(mut self) -> sys::ast_party_id {
        self.owned = false;
        unsafe { ptr::read(&self.value) }
    }
}

impl Drop for OwnedPartyId {
    fn drop(&mut self) {
        if self.owned {
            unsafe { sys::ast_party_id_free(&mut self.value) };
        }
    }
}

struct OwnedConnectedLine(sys::ast_party_connected_line);

impl OwnedConnectedLine {
    unsafe fn new() -> Self {
        let mut value = unsafe { std::mem::zeroed() };
        unsafe { sys::ast_party_connected_line_init(&mut value) };
        Self(value)
    }
}

impl Drop for OwnedConnectedLine {
    fn drop(&mut self) {
        unsafe { sys::ast_party_connected_line_free(&mut self.0) };
    }
}

struct OwnedRedirecting(sys::ast_party_redirecting);

impl OwnedRedirecting {
    unsafe fn new() -> Self {
        let mut value = unsafe { std::mem::zeroed() };
        unsafe { sys::ast_party_redirecting_init(&mut value) };
        Self(value)
    }
}

impl Drop for OwnedRedirecting {
    fn drop(&mut self) {
        unsafe { sys::ast_party_redirecting_free(&mut self.0) };
    }
}

fn party_text(
    field: &'static str,
    value: Option<&str>,
) -> Result<Option<CString>, PartyUpdateError> {
    value
        .map(|value| CString::new(value).map_err(|_| PartyUpdateError::InvalidText { field }))
        .transpose()
}

fn metadata_text(
    value: Option<&str>,
    operation: &'static str,
) -> Result<Option<CString>, ChannelMetadataError> {
    value
        .map(|value| {
            CString::new(value).map_err(|_| ChannelMetadataError::NativeFailure { operation })
        })
        .transpose()
}

unsafe fn decode_party_component(
    field: &'static str,
    value: *const c_char,
) -> Result<String, PartyUpdateError> {
    if value.is_null() {
        // Asterisk's validity bit and pointer are independent. The migration
        // boundary represented valid+null as a present empty component.
        return Ok(String::new());
    }
    let bytes = unsafe { CStr::from_ptr(value) }.to_bytes();
    let bytes = &bytes[..bytes.len().min(PARTY_SNAPSHOT_MAX_BYTES)];
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| PartyUpdateError::InvalidNativeText { field })
}

unsafe fn decode_party_identity(
    field: &'static str,
    source: *const sys::ast_party_id,
) -> Result<PartyIdentity, PartyUpdateError> {
    let source = unsafe { source.as_ref() }.ok_or(PartyUpdateError::NativeFailure {
        operation: "party snapshot",
    })?;
    Ok(PartyIdentity {
        name: (source.name.valid != 0)
            .then(|| unsafe { decode_party_component(field, source.name.str_) })
            .transpose()?,
        number: (source.number.valid != 0)
            .then(|| unsafe { decode_party_component(field, source.number.str_) })
            .transpose()?,
        name_charset: NameCharset::from_raw(source.name.char_set),
        name_presentation: Presentation::from_raw(source.name.presentation),
        number_plan: NumberPlan::from_raw(source.number.plan),
        number_presentation: Presentation::from_raw(source.number.presentation),
    })
}

unsafe fn decode_metadata_component(
    field: &'static str,
    value: *const c_char,
    maximum_bytes: usize,
) -> Result<String, ChannelMetadataError> {
    if value.is_null() {
        return Ok(String::new());
    }
    let bytes = unsafe { CStr::from_ptr(value) }.to_bytes();
    if bytes.len() > maximum_bytes
        || bytes
            .iter()
            .any(|byte| byte.is_ascii_control() && *byte != b'\t')
    {
        return Err(ChannelMetadataError::InvalidNativeText { field });
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| ChannelMetadataError::InvalidNativeText { field })
}

unsafe fn decode_metadata_identity(
    field: &'static str,
    source: *const sys::ast_party_id,
) -> Result<PartyIdentity, ChannelMetadataError> {
    let source = unsafe { source.as_ref() }.ok_or(ChannelMetadataError::NativeFailure {
        operation: "channel metadata snapshot",
    })?;
    Ok(PartyIdentity {
        name: (source.name.valid != 0)
            .then(|| unsafe {
                decode_metadata_component(field, source.name.str_, MAX_PARTY_TEXT_BYTES)
            })
            .transpose()?,
        number: (source.number.valid != 0)
            .then(|| unsafe {
                decode_metadata_component(field, source.number.str_, MAX_PARTY_TEXT_BYTES)
            })
            .transpose()?,
        name_charset: NameCharset::from_raw(source.name.char_set),
        name_presentation: Presentation::from_raw(source.name.presentation),
        number_plan: NumberPlan::from_raw(source.number.plan),
        number_presentation: Presentation::from_raw(source.number.presentation),
    })
}

pub struct NativePartyAdapter;

impl PartyUpdateBackend for NativePartyAdapter {
    fn snapshot(&self, channel: &AsteriskChannel<'_>) -> Result<PartySnapshot, PartyUpdateError> {
        let channel = NonNull::new(channel.as_raw().cast::<sys::ast_channel>()).ok_or(
            PartyUpdateError::NativeFailure {
                operation: "party snapshot",
            },
        )?;
        let _lock = unsafe { ChannelLock::acquire(channel) }.map_err(|_| {
            PartyUpdateError::NativeFailure {
                operation: "party snapshot",
            }
        })?;
        let channel = channel.as_ptr();
        let caller = unsafe { sys::ast_channel_caller(channel) };
        let connected = unsafe { sys::ast_channel_connected(channel) };
        let redirecting = unsafe { sys::ast_channel_redirecting(channel) };
        let (Some(caller), Some(connected), Some(redirecting)) = (
            unsafe { caller.as_ref() },
            unsafe { connected.as_ref() },
            unsafe { redirecting.as_ref() },
        ) else {
            return Err(PartyUpdateError::NativeFailure {
                operation: "party snapshot",
            });
        };
        let redirect_count = u32::try_from(redirecting.count).map_err(|_| {
            PartyUpdateError::InvalidNativeRedirectCount {
                count: redirecting.count,
            }
        })?;
        Ok(PartySnapshot {
            caller: unsafe { decode_party_identity("caller", &caller.id) }?,
            connected: unsafe { decode_party_identity("connected", &connected.id) }?,
            redirecting_original: unsafe {
                decode_party_identity("original redirecting party", &redirecting.orig)
            }?,
            redirecting_from: unsafe {
                decode_party_identity("redirecting-from party", &redirecting.from)
            }?,
            redirecting_to: unsafe {
                decode_party_identity("redirecting-to party", &redirecting.to)
            }?,
            private_redirecting_original: unsafe {
                decode_party_identity("private original redirecting party", &redirecting.priv_orig)
            }?,
            private_redirecting_from: unsafe {
                decode_party_identity("private redirecting-from party", &redirecting.priv_from)
            }?,
            private_redirecting_to: unsafe {
                decode_party_identity("private redirecting-to party", &redirecting.priv_to)
            }?,
            connected_source: ConnectedLineSource::from_raw(connected.source),
            redirect_reason: RedirectReasonCode::from_raw(redirecting.reason.code),
            original_redirect_reason: RedirectReasonCode::from_raw(redirecting.orig_reason.code),
            redirect_count,
        })
    }

    fn connected_line(
        &self,
        channel: &AsteriskChannel<'_>,
        update: &ConnectedLineUpdate,
        delivery: Delivery,
    ) -> Result<(), PartyUpdateError> {
        let party = OwnedPartyId::build_party(&update.party, "party name", "party number")?;
        let private_party = OwnedPartyId::build_party(
            &update.private_party,
            "private party name",
            "private party number",
        )?;
        let mut connected = unsafe { OwnedConnectedLine::new() };
        connected.0.id = party.take();
        connected.0.priv_ = private_party.take();
        connected.0.source = update.source.raw();
        let mut mask: sys::ast_set_party_connected_line = unsafe { std::mem::zeroed() };
        set_party_mask(&mut mask.id);
        set_party_mask(&mut mask.priv_);
        let channel = channel.as_raw().cast::<sys::ast_channel>();
        unsafe {
            match delivery {
                Delivery::Set => {
                    sys::ast_channel_set_connected_line(channel, &connected.0, &mask);
                }
                Delivery::Queue => {
                    sys::ast_channel_queue_connected_line_update(channel, &connected.0, &mask);
                }
            }
        }
        Ok(())
    }

    fn redirecting(
        &self,
        channel: &AsteriskChannel<'_>,
        update: &RedirectingUpdate,
        delivery: Delivery,
    ) -> Result<(), PartyUpdateError> {
        let count = i32::try_from(update.count).map_err(|_| PartyUpdateError::CountOutOfRange {
            count: update.count,
        })?;
        let original = OwnedPartyId::build_party(
            &update.original,
            "original party name",
            "original party number",
        )?;
        let from = OwnedPartyId::build_party(
            &update.from,
            "redirecting-from name",
            "redirecting-from number",
        )?;
        let to =
            OwnedPartyId::build_party(&update.to, "redirecting-to name", "redirecting-to number")?;
        let private_original = OwnedPartyId::build_party(
            &update.private_original,
            "private original party name",
            "private original party number",
        )?;
        let private_from = OwnedPartyId::build_party(
            &update.private_from,
            "private redirecting-from name",
            "private redirecting-from number",
        )?;
        let private_to = OwnedPartyId::build_party(
            &update.private_to,
            "private redirecting-to name",
            "private redirecting-to number",
        )?;
        let reason = party_text("redirect reason", update.reason.text.as_deref())?;
        let original_reason = party_text(
            "original redirect reason",
            update.original_reason.text.as_deref(),
        )?;

        let mut redirecting = unsafe { OwnedRedirecting::new() };
        redirecting.0.orig = original.take();
        redirecting.0.from = from.take();
        redirecting.0.to = to.take();
        redirecting.0.priv_orig = private_original.take();
        redirecting.0.priv_from = private_from.take();
        redirecting.0.priv_to = private_to.take();
        if let Some(reason) = reason {
            redirecting.0.reason.str_ = unsafe { ast_strdup(&reason) };
            if redirecting.0.reason.str_.is_null() {
                return Err(PartyUpdateError::NativeFailure {
                    operation: "redirect reason allocation",
                });
            }
        }
        if let Some(original_reason) = original_reason {
            redirecting.0.orig_reason.str_ = unsafe { ast_strdup(&original_reason) };
            if redirecting.0.orig_reason.str_.is_null() {
                return Err(PartyUpdateError::NativeFailure {
                    operation: "original redirect reason allocation",
                });
            }
        }
        redirecting.0.reason.code = update.reason.code.raw();
        redirecting.0.orig_reason.code = update.original_reason.code.raw();
        redirecting.0.count = count;

        let mut mask: sys::ast_set_party_redirecting = unsafe { std::mem::zeroed() };
        set_party_mask(&mut mask.orig);
        set_party_mask(&mut mask.from);
        set_party_mask(&mut mask.to);
        set_party_mask(&mut mask.priv_orig);
        set_party_mask(&mut mask.priv_from);
        set_party_mask(&mut mask.priv_to);
        let channel = channel.as_raw().cast::<sys::ast_channel>();
        unsafe {
            match delivery {
                Delivery::Set => {
                    sys::ast_channel_set_redirecting(channel, &redirecting.0, &mask);
                }
                Delivery::Queue => {
                    sys::ast_channel_queue_redirecting_update(channel, &redirecting.0, &mask);
                }
            }
        }
        Ok(())
    }
}

fn set_party_mask(mask: &mut sys::ast_set_party_id) {
    mask.name = 1;
    mask.number = 1;
    mask.subaddress = 0;
}

pub struct NativeChannelMetadataAdapter;

impl ChannelMetadataBackend for NativeChannelMetadataAdapter {
    fn snapshot(
        &self,
        channel: &AsteriskChannel<'_>,
    ) -> Result<CallMetadata, ChannelMetadataError> {
        let channel = NonNull::new(channel.as_raw().cast::<sys::ast_channel>()).ok_or(
            ChannelMetadataError::NativeFailure {
                operation: "metadata snapshot",
            },
        )?;
        let _lock = unsafe { ChannelLock::acquire(channel) }.map_err(|_| {
            ChannelMetadataError::NativeFailure {
                operation: "metadata snapshot",
            }
        })?;
        let channel = channel.as_ptr();
        let caller = unsafe { sys::ast_channel_caller(channel) };
        let redirecting = unsafe { sys::ast_channel_redirecting(channel) };
        let dialed = unsafe { sys::ast_channel_dialed(channel) };
        let (Some(caller), Some(redirecting), Some(dialed)) = (
            unsafe { caller.as_ref() },
            unsafe { redirecting.as_ref() },
            unsafe { dialed.as_ref() },
        ) else {
            return Err(ChannelMetadataError::NativeFailure {
                operation: "channel metadata snapshot",
            });
        };

        let account_code = unsafe { sys::ast_channel_accountcode(channel) };
        let language = unsafe { sys::ast_channel_language(channel) };
        let dnid = dialed.number.str_;
        let metadata = CallMetadata {
            ani: unsafe { decode_metadata_identity("ANI", &caller.ani) }?,
            dnid: (!dnid.is_null())
                .then(|| unsafe { decode_metadata_component("DNID", dnid, MAX_PARTY_TEXT_BYTES) })
                .transpose()?,
            dnid_plan: NumberPlan::from_raw(dialed.number.plan),
            rdnis: unsafe { decode_metadata_identity("RDNIS", &redirecting.from) }?,
            account_code: (!account_code.is_null() && unsafe { *account_code } != 0)
                .then(|| unsafe {
                    decode_metadata_component("account code", account_code, MAX_PARTY_TEXT_BYTES)
                })
                .transpose()?,
            language: (!language.is_null() && unsafe { *language } != 0)
                .then(|| unsafe {
                    decode_metadata_component("language", language, MAX_LANGUAGE_BYTES)
                })
                .transpose()?,
            variables: Vec::new(),
        };
        metadata.validate()?;
        Ok(metadata)
    }

    fn apply(
        &self,
        channel: &AsteriskChannel<'_>,
        metadata: &CallMetadata,
    ) -> Result<(), ChannelMetadataError> {
        metadata.validate()?;
        let ani = OwnedPartyId::build_metadata(&metadata.ani, "ANI allocation")?;
        let rdnis = OwnedPartyId::build_metadata(&metadata.rdnis, "RDNIS allocation")?;
        let dnid = match metadata.dnid.as_deref() {
            Some(value) => {
                let value = metadata_text(Some(value), "DNID encoding")?.ok_or(
                    ChannelMetadataError::NativeFailure {
                        operation: "DNID encoding",
                    },
                )?;
                unsafe { AstString::duplicate(&value) }.ok_or(
                    ChannelMetadataError::NativeFailure {
                        operation: "DNID allocation",
                    },
                )?
            }
            None => AstString::absent(),
        };
        let account_code =
            metadata_text(metadata.account_code.as_deref(), "account-code encoding")?;
        let language = metadata_text(metadata.language.as_deref(), "language encoding")?;
        let variables = metadata
            .variables
            .iter()
            .map(|variable| {
                let name = CString::new(variable.name()).map_err(|_| {
                    ChannelMetadataError::NativeFailure {
                        operation: "channel-variable name encoding",
                    }
                })?;
                let value = CString::new(variable.value()).map_err(|_| {
                    ChannelMetadataError::NativeFailure {
                        operation: "channel-variable value encoding",
                    }
                })?;
                Ok((name, value))
            })
            .collect::<Result<Vec<_>, ChannelMetadataError>>()?;

        let channel = NonNull::new(channel.as_raw().cast::<sys::ast_channel>()).ok_or(
            ChannelMetadataError::NativeFailure {
                operation: "metadata apply",
            },
        )?;
        {
            let _lock = unsafe { ChannelLock::acquire(channel) }.map_err(|_| {
                ChannelMetadataError::NativeFailure {
                    operation: "metadata apply",
                }
            })?;
            let channel = channel.as_ptr();
            let caller = unsafe { sys::ast_channel_caller(channel) };
            let redirecting = unsafe { sys::ast_channel_redirecting(channel) };
            let dialed = unsafe { sys::ast_channel_dialed(channel) };
            let (Some(caller), Some(redirecting), Some(dialed)) = (
                unsafe { caller.as_mut() },
                unsafe { redirecting.as_mut() },
                unsafe { dialed.as_mut() },
            ) else {
                return Err(ChannelMetadataError::NativeFailure {
                    operation: "channel metadata apply",
                });
            };
            unsafe {
                sys::ast_party_id_free(&mut caller.ani);
                caller.ani = ani.take();
                sys::ast_party_id_free(&mut redirecting.from);
                redirecting.from = rdnis.take();
                drop(AsteriskAllocation::from_owned(dialed.number.str_));
            }
            dialed.number.str_ = dnid.take();
            dialed.number.plan = metadata.dnid_plan.raw();
            if let Some(account_code) = account_code.as_ref() {
                unsafe { sys::ast_channel_accountcode_set(channel, account_code.as_ptr()) };
            }
            if let Some(language) = language.as_ref() {
                unsafe { sys::ast_channel_language_set(channel, language.as_ptr()) };
            }
        }

        for (name, value) in &variables {
            let _ = unsafe {
                sys::pbx_builtin_setvar_helper(channel.as_ptr(), name.as_ptr(), value.as_ptr())
            };
        }
        Ok(())
    }

    fn inherit(
        &self,
        parent: &AsteriskChannel<'_>,
        child: &AsteriskChannel<'_>,
    ) -> Result<(), ChannelMetadataError> {
        let parent = NonNull::new(parent.as_raw().cast::<sys::ast_channel>()).ok_or(
            ChannelMetadataError::NativeFailure {
                operation: "channel-variable inheritance",
            },
        )?;
        let child = NonNull::new(child.as_raw().cast::<sys::ast_channel>()).ok_or(
            ChannelMetadataError::NativeFailure {
                operation: "channel-variable inheritance",
            },
        )?;
        if parent == child {
            return Err(ChannelMetadataError::NativeFailure {
                operation: "channel-variable inheritance",
            });
        }
        let _lock = unsafe { ChannelPairLock::acquire(parent, child) }.map_err(|_| {
            ChannelMetadataError::NativeFailure {
                operation: "channel-variable inheritance",
            }
        })?;
        unsafe { sys::ast_channel_inherit_variables(parent.as_ptr(), child.as_ptr()) };
        Ok(())
    }
}

/// Reads one bounded channel variable for other Rust-native Asterisk adapters.
pub fn copy_channel_variable(
    channel: &AsteriskChannel<'_>,
    name: &CStr,
    capacity: usize,
) -> Result<Option<String>, ChannelMetadataError> {
    if capacity < 2 {
        return Err(ChannelMetadataError::NativeFailure {
            operation: "channel-variable read",
        });
    }
    let channel = NonNull::new(channel.as_raw().cast::<sys::ast_channel>()).ok_or(
        ChannelMetadataError::NativeFailure {
            operation: "channel-variable read",
        },
    )?;
    let _lock = unsafe { ChannelLock::acquire(channel) }.map_err(|_| {
        ChannelMetadataError::NativeFailure {
            operation: "channel-variable read",
        }
    })?;
    let value = unsafe { sys::pbx_builtin_getvar_helper(channel.as_ptr(), name.as_ptr()) };
    if value.is_null() {
        return Ok(None);
    }
    unsafe { decode_metadata_component("channel variable", value, capacity - 1) }.map(Some)
}
