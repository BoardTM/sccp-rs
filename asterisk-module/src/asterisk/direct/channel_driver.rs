//! Rust-owned Asterisk channel technology, RTP glue, and CLI descriptors.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::mem;
use std::ptr::{self, NonNull};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::ami::cli::{CliInventoryCommand, MAX_CLI_ARGUMENT_BYTES, MAX_CLI_ARGUMENTS};
use crate::ami::controls::{MAX_DEVICE_SELECTOR_BYTES, ResetMode};
use crate::ami::diagnostics::{
    CliDiagnosticCommand, MAX_CLI_DIAGNOSTIC_ARGUMENT_BYTES, MAX_CLI_DIAGNOSTIC_ARGUMENTS,
};
use crate::asterisk::boundary::{
    CallbackStatus, DeviceState, contain_panic as callback_guard, optional_c_text, read_c_int,
    required_c_text, write_c_int,
};
use crate::asterisk::native_channel::{
    audio_capability_mask, channel_private as private, destroy_channel_private,
    prepare_channel_private_teardown, private_owner, private_rtp, private_video_rtp,
    reassign_private_owner, retain_private_rtp, retain_private_video_rtp, video_capability_mask,
};
use crate::asterisk::sys;
use crate::call::auto_answer::InboundDialRequest;
use crate::call::completion::canonical_callback_target;
use crate::config::reload::{MAX_RELOAD_ARGUMENT_BYTES, MAX_RELOAD_ARGUMENTS};

use super::super::exports::{
    ChannelIndication, ChannelOperationError, ChannelRequest, ChannelRequestError, ChannelSecurity,
    ControlCliCommand, DirectMediaPeer, MediaPeerUpdate, ModuleLifecycleError, RequestedChannel,
    answer_channel, channel_security, complete_control_cli, complete_device_control_cli,
    complete_diagnostic_cli, complete_inventory_cli, complete_reload_cli, direct_media_allowed,
    execute_control_cli, execute_device_control_cli, execute_diagnostic_cli,
    execute_forwarding_cli, execute_inventory_cli, execute_reload_cli, fixup_channel,
    hangup_channel, has_active_channels, indicate_channel, line_device_state, place_call,
    reload_module, request_channel, resume_channel_operations, send_digit_begin_to_channel,
    send_digit_end_to_channel, send_text_to_channel, start_module, stop_module,
    suspend_channel_operations, update_rtp_peer,
};
use super::handles::{NativeChannelRegistration, TemporarilyUnlockedChannel};
use super::module_info::module_self;
use crate::asterisk::StaticDescriptor;

const SCCP_TYPE: &CStr = c"SCCP";
const SCCP_DESCRIPTION: &[u8] = b"Modern Cisco SCCP channel driver\0";
const SOURCE_FILE: &CStr = c"asterisk/direct/channel_driver.rs";
const SOURCE_FUNCTION: &CStr = c"sccp_channel_driver";
const CC_GENERIC_MONITOR: &CStr = c"generic";

static SCCP_TECH: StaticDescriptor<sys::ast_channel_tech> = StaticDescriptor::uninit();
static RTP_GLUE: StaticDescriptor<sys::ast_rtp_glue> = StaticDescriptor::uninit();
#[cfg(not(feature = "live-asterisk-tests"))]
const CLI_ENTRY_COUNT: usize = 15;
#[cfg(feature = "live-asterisk-tests")]
const CLI_ENTRY_COUNT: usize = 16;

static CLI_ENTRIES: StaticDescriptor<[sys::ast_cli_entry; CLI_ENTRY_COUNT]> =
    StaticDescriptor::uninit();
static NATIVE_REGISTRATION: Mutex<Option<NativeChannelRegistration>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ChannelDriverLoadError;

fn native_registration() -> MutexGuard<'static, Option<NativeChannelRegistration>> {
    NATIVE_REGISTRATION
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Returns the stable channel-technology descriptor after module startup has
/// initialized it. Native channel allocation is only called after that point.
pub unsafe fn technology_ptr() -> *mut sys::ast_channel_tech {
    unsafe { SCCP_TECH.as_ptr() }
}

/// Returns the running module scheduler required by Asterisk RTP instances.
/// Channel allocation is unavailable until native registration has installed
/// this owner, and unload is rejected while any channel remains active.
pub fn rtp_scheduler() -> Option<NonNull<sys::ast_sched_context>> {
    native_registration()
        .as_ref()
        .map(NativeChannelRegistration::rtp_scheduler)
}

fn request_from_asterisk(
    capabilities: *mut sys::ast_format_cap,
    assigned_ids: *const sys::ast_assigned_ids,
    requestor: *const sys::ast_channel,
    address: &CStr,
) -> Result<RequestedChannel, ChannelRequestError> {
    unsafe {
        request_channel(ChannelRequest {
            capabilities,
            assigned_ids,
            requestor,
            address,
        })
    }
}

unsafe extern "C" fn requester_with_stream_topology(
    type_: *const c_char,
    topology: *mut sys::ast_stream_topology,
    assigned_ids: *const sys::ast_assigned_ids,
    requestor: *const sys::ast_channel,
    address: *const c_char,
    cause: *mut c_int,
) -> *mut sys::ast_channel {
    callback_guard(ptr::null_mut(), || unsafe {
        let _ = type_;
        let Ok(address) = required_c_text(address, 256) else {
            return ptr::null_mut();
        };
        let Ok(address) = CString::new(address) else {
            return ptr::null_mut();
        };
        if topology.is_null() {
            return ptr::null_mut();
        }
        let Some(capabilities) = NonNull::new(sys::ast_stream_topology_get_formats(topology))
        else {
            return ptr::null_mut();
        };
        let result =
            request_from_asterisk(capabilities.as_ptr(), assigned_ids, requestor, &address);
        crate::asterisk::native_channel::release_format_cap(capabilities);
        match result {
            Ok(requested) => {
                if let Some(value) = requested.cause
                    && !cause.is_null()
                {
                    *cause = value;
                }
                requested.channel.as_ptr().cast()
            }
            Err(error) => {
                if let Some(value) = error.cause
                    && !cause.is_null()
                {
                    *cause = value;
                }
                ptr::null_mut()
            }
        }
    })
}

unsafe extern "C" fn call(
    channel: *mut sys::ast_channel,
    address: *const c_char,
    timeout: c_int,
) -> c_int {
    callback_guard(-1, || unsafe {
        let _ = (address, timeout);
        CallbackStatus::from_result(
            NonNull::new(channel)
                .ok_or(())
                .and_then(|channel| place_call(channel).map_err(|_| ())),
        )
        .as_raw()
    })
}

unsafe extern "C" fn hangup(channel: *mut sys::ast_channel) -> c_int {
    callback_guard(-1, || unsafe {
        let Some(channel) = NonNull::new(channel) else {
            return -1;
        };
        let private = private(channel.as_ptr());
        let result = CallbackStatus::from_result(hangup_channel(channel).map_err(|_| ())).as_raw();
        if let Some(private) = private {
            prepare_channel_private_teardown(channel, private);
        }
        sys::ast_channel_tech_pvt_set(channel.as_ptr(), ptr::null_mut());
        if let Some(private) = private {
            destroy_channel_private(private);
        }
        result
    })
}

unsafe extern "C" fn answer(channel: *mut sys::ast_channel) -> c_int {
    callback_guard(-1, || unsafe {
        let result = NonNull::new(channel).ok_or(()).and_then(|channel| {
            let receipt = answer_channel(channel).map_err(|_| ())?;
            {
                let _unlocked = TemporarilyUnlockedChannel::new(channel);
                receipt.wait().map_err(|_| ())
            }
        });
        if result.is_ok() {
            sys::ast_setstate(channel, sys::AST_STATE_UP);
        }
        CallbackStatus::from_result(result).as_raw()
    })
}

unsafe extern "C" fn read(channel: *mut sys::ast_channel) -> *mut sys::ast_frame {
    callback_guard(ptr::null_mut(), || unsafe {
        let Some(private) = private(channel) else {
            return ptr::addr_of_mut!(sys::ast_null_frame);
        };
        let rtp = private_rtp(private);
        match sys::ast_channel_fdno(channel) {
            0 => sys::ast_rtp_instance_read(rtp.as_ptr(), 0),
            1 => sys::ast_rtp_instance_read(rtp.as_ptr(), 1),
            2 => private_video_rtp(private).map_or(ptr::addr_of_mut!(sys::ast_null_frame), |rtp| {
                sys::ast_rtp_instance_read(rtp.as_ptr(), 0)
            }),
            3 => private_video_rtp(private).map_or(ptr::addr_of_mut!(sys::ast_null_frame), |rtp| {
                sys::ast_rtp_instance_read(rtp.as_ptr(), 1)
            }),
            _ => ptr::addr_of_mut!(sys::ast_null_frame),
        }
    })
}

unsafe extern "C" fn write(channel: *mut sys::ast_channel, frame: *mut sys::ast_frame) -> c_int {
    callback_guard(-1, || unsafe {
        if frame.is_null() {
            return -1;
        }
        let Some(private) = private(channel) else {
            return -1;
        };
        let rtp = if (*frame).frametype as u32 == sys::AST_FRAME_VIDEO {
            let Some(video) = private_video_rtp(private) else {
                return 0;
            };
            video
        } else {
            private_rtp(private)
        };
        sys::ast_rtp_instance_write(rtp.as_ptr(), frame)
    })
}

unsafe extern "C" fn get_rtp_info(
    channel: *mut sys::ast_channel,
    instance: *mut *mut sys::ast_rtp_instance,
) -> sys::ast_rtp_glue_result {
    callback_guard(sys::AST_RTP_GLUE_RESULT_FORBID, || unsafe {
        let Some(private) = private(channel) else {
            return sys::AST_RTP_GLUE_RESULT_FORBID;
        };
        if instance.is_null() {
            return sys::AST_RTP_GLUE_RESULT_FORBID;
        }
        let rtp = retain_private_rtp(private);
        *instance = rtp;
        if direct_media_allowed(NonNull::new_unchecked(channel)) {
            sys::AST_RTP_GLUE_RESULT_REMOTE
        } else {
            sys::AST_RTP_GLUE_RESULT_LOCAL
        }
    })
}

unsafe extern "C" fn get_vrtp_info(
    channel: *mut sys::ast_channel,
    instance: *mut *mut sys::ast_rtp_instance,
) -> sys::ast_rtp_glue_result {
    callback_guard(sys::AST_RTP_GLUE_RESULT_FORBID, || unsafe {
        let Some(private) = private(channel) else {
            return sys::AST_RTP_GLUE_RESULT_FORBID;
        };
        if instance.is_null() {
            return sys::AST_RTP_GLUE_RESULT_FORBID;
        }
        let Some(rtp) = retain_private_video_rtp(private) else {
            return sys::AST_RTP_GLUE_RESULT_FORBID;
        };
        *instance = rtp;
        sys::AST_RTP_GLUE_RESULT_LOCAL
    })
}

unsafe fn update_peer_from_asterisk(
    channel: NonNull<sys::ast_channel>,
    instance: NonNull<sys::ast_rtp_instance>,
    capabilities: Option<NonNull<sys::ast_format_cap>>,
    nat_active: bool,
) -> Result<(), ChannelOperationError> {
    let mut remote = unsafe { mem::zeroed::<sys::ast_sockaddr>() };
    unsafe { sys::ast_rtp_instance_get_requested_target_address(instance.as_ptr(), &mut remote) };
    let port = if remote.len == 0 {
        0
    } else {
        unsafe {
            sys::_ast_sockaddr_port(
                &remote,
                SOURCE_FILE.as_ptr().cast(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr().cast(),
            )
        }
    };
    let address = if port == 0 {
        None
    } else {
        let address = unsafe {
            sys::ast_sockaddr_stringify_fmt(&remote, sys::AST_SOCKADDR_STR_ADDR as c_int)
        };
        unsafe { optional_c_text(address, 64) }
            .ok()
            .flatten()
            .and_then(|address| address.parse().ok())
    };
    unsafe {
        update_rtp_peer(
            channel,
            MediaPeerUpdate::Direct(DirectMediaPeer {
                address,
                port,
                audio_capabilities: capabilities
                    .map(|capabilities| audio_capability_mask(Some(capabilities)).bits())
                    .unwrap_or(0),
                video_capabilities: capabilities
                    .map(|capabilities| video_capability_mask(Some(capabilities)).bits())
                    .unwrap_or(0),
                nat_active,
            }),
        )
    }
}

unsafe extern "C" fn update_peer(
    channel: *mut sys::ast_channel,
    instance: *mut sys::ast_rtp_instance,
    video: *mut sys::ast_rtp_instance,
    _text: *mut sys::ast_rtp_instance,
    capabilities: *const sys::ast_format_cap,
    nat_active: c_int,
) -> c_int {
    callback_guard(-1, || unsafe {
        let result = NonNull::new(channel).ok_or(()).and_then(|channel| {
            if let Some(instance) = NonNull::new(instance) {
                update_peer_from_asterisk(
                    channel,
                    instance,
                    NonNull::new(capabilities.cast_mut()),
                    nat_active != 0,
                )
                .map_err(|_| ())
            } else if !video.is_null() {
                // Video glue is always local, so a video-only peer refresh
                // leaves the independently routed audio stream unchanged.
                Ok(())
            } else {
                update_rtp_peer(channel, MediaPeerUpdate::Anchor).map_err(|_| ())
            }
        });
        CallbackStatus::from_result(result).as_raw()
    })
}

unsafe extern "C" fn get_codec(channel: *mut sys::ast_channel, result: *mut sys::ast_format_cap) {
    callback_guard((), || unsafe {
        if !channel.is_null() && !result.is_null() {
            sys::ast_format_cap_append_from_cap(
                result,
                sys::ast_channel_nativeformats(channel),
                sys::AST_MEDIA_TYPE_UNKNOWN,
            );
        }
    });
}

unsafe extern "C" fn indicate(
    channel: *mut sys::ast_channel,
    condition: c_int,
    data: *const c_void,
    data_length: usize,
) -> c_int {
    callback_guard(-1, || unsafe {
        let Some(channel) = NonNull::new(channel) else {
            return -1;
        };
        if condition as u32 == sys::AST_CONTROL_MASQUERADE_NOTIFY {
            let Ok(beginning) = read_c_int(data, data_length) else {
                return -1;
            };
            let beginning = beginning != 0;
            let result = if beginning {
                let _unlocked = TemporarilyUnlockedChannel::new(channel);
                suspend_channel_operations(channel)
            } else {
                resume_channel_operations(channel)
            };
            return CallbackStatus::from_result(result.map_err(|_| ())).as_raw();
        }
        let indication = match condition {
            -1 => ChannelIndication::StopTone,
            value if value as u32 == sys::AST_CONTROL_INCOMPLETE => ChannelIndication::Incomplete,
            value if value as u32 == sys::AST_CONTROL_SRCUPDATE => ChannelIndication::SourceUpdate,
            value if value as u32 == sys::AST_CONTROL_SRCCHANGE => ChannelIndication::SourceChange,
            value if value as u32 == sys::AST_CONTROL_UPDATE_RTP_PEER => {
                ChannelIndication::UpdateRtpPeer
            }
            value if value as u32 == sys::AST_CONTROL_VIDUPDATE => ChannelIndication::VideoUpdate,
            value if value as u32 == sys::AST_CONTROL_RINGING => ChannelIndication::Ringing,
            value if value as u32 == sys::AST_CONTROL_ANSWER => ChannelIndication::Answer,
            value if value as u32 == sys::AST_CONTROL_BUSY => ChannelIndication::Busy,
            value if value as u32 == sys::AST_CONTROL_CONGESTION => ChannelIndication::Congestion,
            value if value as u32 == sys::AST_CONTROL_PROGRESS => ChannelIndication::Progress,
            value if value as u32 == sys::AST_CONTROL_PROCEEDING => ChannelIndication::Proceeding,
            value if value as u32 == sys::AST_CONTROL_HOLD => ChannelIndication::Hold,
            value if value as u32 == sys::AST_CONTROL_UNHOLD => ChannelIndication::Unhold,
            value if value as u32 == sys::AST_CONTROL_CONNECTED_LINE => {
                ChannelIndication::ConnectedLine
            }
            value if value as u32 == sys::AST_CONTROL_REDIRECTING => ChannelIndication::Redirecting,
            _ => return -1,
        };
        CallbackStatus::from_result(indicate_channel(channel, indication).map_err(|_| ())).as_raw()
    })
}

unsafe extern "C" fn send_digit_begin(channel: *mut sys::ast_channel, digit: c_char) -> c_int {
    callback_guard(-1, || unsafe {
        let Some(digit) = dtmf_digit(digit) else {
            return -1;
        };
        CallbackStatus::from_result(
            NonNull::new(channel)
                .ok_or(())
                .and_then(|channel| send_digit_begin_to_channel(channel, digit).map_err(|_| ())),
        )
        .as_raw()
    })
}

fn dtmf_digit(digit: c_char) -> Option<u8> {
    let digit = digit as u8;
    matches!(digit, b'0'..=b'9' | b'*' | b'#' | b'A'..=b'D').then_some(digit)
}

unsafe extern "C" fn send_text(channel: *mut sys::ast_channel, text: *const c_char) -> c_int {
    callback_guard(-1, || unsafe {
        let result = NonNull::new(channel).ok_or(()).and_then(|channel| {
            required_c_text(text, 1_024)
                .map_err(|_| ())
                .and_then(|text| send_text_to_channel(channel, text).map_err(|_| ()))
        });
        CallbackStatus::from_result(result).as_raw()
    })
}

unsafe extern "C" fn send_digit_end(
    channel: *mut sys::ast_channel,
    digit: c_char,
    duration: u32,
) -> c_int {
    callback_guard(-1, || unsafe {
        let Some(digit) = dtmf_digit(digit) else {
            return -1;
        };
        let result = NonNull::new(channel).ok_or(()).and_then(|channel| {
            send_digit_end_to_channel(
                channel,
                digit,
                std::time::Duration::from_millis(duration.into()),
            )
            .map_err(|_| ())
        });
        CallbackStatus::from_result(result).as_raw()
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SecurityOption {
    Signaling,
    Media,
}

impl TryFrom<c_int> for SecurityOption {
    type Error = ();

    fn try_from(value: c_int) -> Result<Self, Self::Error> {
        match value as u32 {
            sys::AST_OPTION_SECURE_SIGNALING => Ok(Self::Signaling),
            sys::AST_OPTION_SECURE_MEDIA => Ok(Self::Media),
            _ => Err(()),
        }
    }
}

impl SecurityOption {
    const fn enabled(self, security: ChannelSecurity) -> bool {
        match self {
            Self::Signaling => security.signaling,
            Self::Media => security.media,
        }
    }
}

unsafe extern "C" fn set_option(
    channel: *mut sys::ast_channel,
    option: c_int,
    data: *mut c_void,
    data_length: c_int,
) -> c_int {
    callback_guard(-1, || unsafe {
        let (Some(channel), Ok(option)) = (NonNull::new(channel), SecurityOption::try_from(option))
        else {
            return -1;
        };
        let result = usize::try_from(data_length)
            .ok()
            .and_then(|length| read_c_int(data, length).ok())
            .ok_or(())
            .and_then(|requested| {
                let security = channel_security(channel).map_err(|_| ())?;
                (option.enabled(security) == (requested != 0))
                    .then_some(())
                    .ok_or(())
            });
        CallbackStatus::from_result(result).as_raw()
    })
}

unsafe extern "C" fn query_option(
    channel: *mut sys::ast_channel,
    option: c_int,
    data: *mut c_void,
    data_length: *mut c_int,
) -> c_int {
    callback_guard(-1, || unsafe {
        let (Some(channel), Ok(option)) = (NonNull::new(channel), SecurityOption::try_from(option))
        else {
            return -1;
        };
        let result = channel_security(channel)
            .map_err(|_| ())
            .and_then(|security| {
                write_c_int(data, data_length, c_int::from(option.enabled(security)))
                    .map_err(|_| ())
            });
        CallbackStatus::from_result(result).as_raw()
    })
}

unsafe extern "C" fn fixup(
    old_channel: *mut sys::ast_channel,
    new_channel: *mut sys::ast_channel,
) -> c_int {
    callback_guard(-1, || unsafe {
        let (Some(old_channel), Some(new_channel)) =
            (NonNull::new(old_channel), NonNull::new(new_channel))
        else {
            return -1;
        };
        let Some(private) = private(new_channel.as_ptr()) else {
            return -1;
        };
        if private_owner(private) != Some(old_channel) {
            return -1;
        }
        if fixup_channel(old_channel, new_channel).is_err() {
            return -1;
        }
        reassign_private_owner(private, new_channel);
        let uniqueid = sys::ast_channel_uniqueid(new_channel.as_ptr());
        sys::ast_rtp_instance_set_channel_id(private_rtp(private).as_ptr(), uniqueid);
        if let Some(video) = private_video_rtp(private) {
            sys::ast_rtp_instance_set_channel_id(video.as_ptr(), uniqueid);
        }
        0
    })
}

unsafe extern "C" fn device_state(line: *const c_char) -> c_int {
    callback_guard(sys::AST_DEVICE_UNKNOWN as c_int, || unsafe {
        let Ok(line) = required_c_text(line, 256) else {
            return sys::AST_DEVICE_UNKNOWN as c_int;
        };
        match line_device_state(&line) {
            DeviceState::NotInUse => sys::AST_DEVICE_NOT_INUSE as c_int,
            DeviceState::InUse => sys::AST_DEVICE_INUSE as c_int,
            DeviceState::Busy => sys::AST_DEVICE_BUSY as c_int,
            DeviceState::Removed => sys::AST_DEVICE_INVALID as c_int,
            DeviceState::Unavailable => sys::AST_DEVICE_UNAVAILABLE as c_int,
            DeviceState::Ringing => sys::AST_DEVICE_RINGING as c_int,
            DeviceState::RingInUse => sys::AST_DEVICE_RINGINUSE as c_int,
            DeviceState::OnHold => sys::AST_DEVICE_ONHOLD as c_int,
        }
    })
}

struct CallCompletionParameters(NonNull<sys::ast_cc_config_params>);

impl CallCompletionParameters {
    fn new() -> Option<Self> {
        NonNull::new(unsafe {
            sys::__ast_cc_config_params_init(
                SOURCE_FILE.as_ptr().cast(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr().cast(),
            )
        })
        .map(Self)
    }

    fn as_ptr(&self) -> *mut sys::ast_cc_config_params {
        self.0.as_ptr()
    }
}

impl Drop for CallCompletionParameters {
    fn drop(&mut self) {
        unsafe { sys::ast_cc_config_params_destroy(self.0.as_ptr()) };
    }
}

fn callback_target(destination: &CStr) -> Result<CString, ()> {
    let destination = destination.to_str().map_err(|_| ())?;
    let access = super::super::module_access().ok_or(())?;
    let request = InboundDialRequest::parse(destination).map_err(|_| ())?;
    let target = canonical_callback_target(
        access
            .inbound_line_bindings(request.target())
            .into_iter()
            .map(|binding| binding.line.number),
    )
    .map_err(|_| ())?;
    let target = CString::new(target).map_err(|_| ())?;
    if target.as_bytes_with_nul().len() > sys::AST_CHANNEL_NAME as usize {
        return Err(());
    }
    Ok(target)
}

unsafe fn register_completion_monitor(
    inbound: NonNull<sys::ast_channel>,
    destination: &CStr,
    callback: sys::ast_cc_callback_fn,
) -> Result<(), ()> {
    let Some(callback) = callback else {
        return Err(());
    };
    let target = callback_target(destination)?;
    let parameters = CallCompletionParameters::new().ok_or(())?;
    if unsafe { sys::ast_set_cc_monitor_policy(parameters.as_ptr(), sys::AST_CC_MONITOR_GENERIC) }
        != 0
    {
        return Err(());
    }
    unsafe {
        callback(
            inbound.as_ptr(),
            parameters.as_ptr(),
            CC_GENERIC_MONITOR.as_ptr().cast(),
            target.as_ptr(),
            target.as_ptr(),
            ptr::null_mut(),
        );
    }
    Ok(())
}

unsafe extern "C" fn call_completion(
    inbound: *mut sys::ast_channel,
    destination: *const c_char,
    callback: sys::ast_cc_callback_fn,
) -> c_int {
    callback_guard(-1, || unsafe {
        if callback.is_none() {
            return -1;
        }
        let Some(inbound) = NonNull::new(inbound) else {
            return -1;
        };
        let Ok(destination) = required_c_text(destination, 256) else {
            return -1;
        };
        let Ok(destination) = CString::new(destination) else {
            return -1;
        };
        CallbackStatus::from_result(register_completion_monitor(inbound, &destination, callback))
            .as_raw()
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CliPhase {
    Initialize,
    Generate,
    Execute,
}

impl CliPhase {
    const fn from_raw(command: c_int) -> Self {
        match command {
            -2 => Self::Initialize,
            -3 => Self::Generate,
            _ => Self::Execute,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CliDisposition {
    Complete,
    ShowUsage,
}

fn cli_disposition_pointer(disposition: CliDisposition) -> *mut c_char {
    match disposition {
        CliDisposition::Complete => ptr::null_mut(),
        CliDisposition::ShowUsage => 1usize as *mut c_char,
    }
}

#[derive(Clone, Copy)]
struct CliArgs<'a> {
    raw: &'a sys::ast_cli_args,
}

#[derive(Debug, Eq, PartialEq)]
struct CliInvocation {
    fd: c_int,
    arguments: Vec<String>,
}

#[derive(Debug, Eq, PartialEq)]
struct CliCompletion {
    position: usize,
    ordinal: usize,
    prefix: String,
    arguments: Vec<String>,
}

impl<'a> CliArgs<'a> {
    unsafe fn from_raw(arguments: *mut sys::ast_cli_args) -> Option<Self> {
        NonNull::new(arguments).map(|arguments| Self {
            raw: unsafe { arguments.as_ref() },
        })
    }

    fn argument_pointer(self, index: usize) -> Result<*const c_char, ()> {
        let count = usize::try_from(self.raw.argc).map_err(|_| ())?;
        if index >= count || self.raw.argv.is_null() {
            return Err(());
        }
        Ok(unsafe { *self.raw.argv.add(index) })
    }

    fn required_argument(self, index: usize, maximum_bytes: usize) -> Result<String, ()> {
        let argument = self.argument_pointer(index)?;
        unsafe { required_c_text(argument, maximum_bytes) }.map_err(|_| ())
    }

    fn optional_argument(self, index: usize, maximum_bytes: usize) -> Result<Option<String>, ()> {
        let argument = self.argument_pointer(index)?;
        unsafe { optional_c_text(argument, maximum_bytes) }.map_err(|_| ())
    }

    fn prefix(self, maximum_bytes: usize) -> Result<String, ()> {
        unsafe { optional_c_text(self.raw.word, maximum_bytes) }
            .map(Option::unwrap_or_default)
            .map_err(|_| ())
    }

    fn invocation(
        self,
        command_words: usize,
        accepts_count: impl FnOnce(usize) -> bool,
        argument_bound: impl Fn(usize) -> Option<usize>,
    ) -> Result<CliInvocation, ()> {
        let argument_count = usize::try_from(self.raw.argc)
            .ok()
            .and_then(|count| count.checked_sub(command_words))
            .ok_or(())?;
        if !accepts_count(argument_count) || (argument_count != 0 && self.raw.argv.is_null()) {
            return Err(());
        }
        let arguments = (0..argument_count)
            .map(|index| {
                self.required_argument(index + command_words, argument_bound(index).ok_or(())?)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CliInvocation {
            fd: self.raw.fd,
            arguments,
        })
    }

    fn completion_cursor(
        self,
        command_words: usize,
        prefix_bound: impl FnOnce(usize) -> Option<usize>,
    ) -> Result<CliCompletion, ()> {
        let position = usize::try_from(self.raw.pos).map_err(|_| ())?;
        let ordinal = usize::try_from(self.raw.n).map_err(|_| ())?;
        let argument_count = position.checked_sub(command_words).ok_or(())?;
        let prefix = self.prefix(prefix_bound(argument_count).ok_or(())?)?;
        Ok(CliCompletion {
            position,
            ordinal,
            prefix,
            arguments: Vec::new(),
        })
    }

    fn completion(
        self,
        command_words: usize,
        accepts_previous_count: impl FnOnce(usize) -> bool,
        argument_bound: impl Fn(usize) -> Option<usize>,
    ) -> Result<CliCompletion, ()> {
        let mut completion =
            self.completion_cursor(command_words, |index| argument_bound(index))?;
        let argument_count = completion.position - command_words;
        if !accepts_previous_count(argument_count)
            || (argument_count != 0 && self.raw.argv.is_null())
        {
            return Err(());
        }
        completion.arguments = (0..argument_count)
            .map(|index| {
                self.required_argument(index + command_words, argument_bound(index).ok_or(())?)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(completion)
    }
}

fn cli_completion(candidate: Option<String>) -> *mut c_char {
    candidate
        .and_then(|candidate| CString::new(candidate).ok())
        .map_or(ptr::null_mut(), |candidate| {
            crate::asterisk::raw::system::cli_completion(&candidate)
        })
}

unsafe fn run_reload_cli(
    entry: Option<NonNull<sys::ast_cli_entry>>,
    phase: CliPhase,
    arguments: Option<CliArgs<'_>>,
) -> *mut c_char {
    match phase {
        CliPhase::Initialize => {
            if let Some(mut entry) = entry {
                unsafe {
                    entry.as_mut().command = c"sccp reload".as_ptr().cast_mut();
                    entry.as_mut().usage =
                        c"Usage: sccp reload [device <id>|line <number>|profile <name>]\n".as_ptr();
                }
            }
            ptr::null_mut()
        }
        CliPhase::Generate => {
            let Some(arguments) = arguments else {
                return ptr::null_mut();
            };
            let Ok(completion) = arguments.completion(
                2,
                |count| count < MAX_RELOAD_ARGUMENTS,
                |_| Some(MAX_RELOAD_ARGUMENT_BYTES),
            ) else {
                return ptr::null_mut();
            };
            cli_completion(complete_reload_cli(
                &completion.arguments,
                &completion.prefix,
                completion.ordinal,
            ))
        }
        CliPhase::Execute => {
            let Some(arguments) = arguments else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            let Ok(invocation) = arguments.invocation(
                2,
                |count| count <= MAX_RELOAD_ARGUMENTS,
                |_| Some(MAX_RELOAD_ARGUMENT_BYTES),
            ) else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            execute_reload_cli(invocation.fd, &invocation.arguments);
            ptr::null_mut()
        }
    }
}

unsafe extern "C" fn cli_reload(
    entry: *mut sys::ast_cli_entry,
    command: c_int,
    arguments: *mut sys::ast_cli_args,
) -> *mut c_char {
    callback_guard(ptr::null_mut(), || unsafe {
        run_reload_cli(
            NonNull::new(entry),
            CliPhase::from_raw(command),
            CliArgs::from_raw(arguments),
        )
    })
}

unsafe fn run_inventory_cli(
    entry: Option<NonNull<sys::ast_cli_entry>>,
    phase: CliPhase,
    arguments: Option<CliArgs<'_>>,
    operation: CliInventoryCommand,
    command_text: &'static CStr,
    usage: &'static CStr,
) -> *mut c_char {
    match phase {
        CliPhase::Initialize => {
            if let Some(mut entry) = entry {
                unsafe {
                    entry.as_mut().command = command_text.as_ptr().cast_mut();
                    entry.as_mut().usage = usage.as_ptr();
                }
            }
            ptr::null_mut()
        }
        CliPhase::Generate => {
            let Some(arguments) = arguments else {
                return ptr::null_mut();
            };
            let Ok(completion) = arguments.completion(
                3,
                |count| count <= MAX_CLI_ARGUMENTS,
                |_| Some(MAX_CLI_ARGUMENT_BYTES),
            ) else {
                return ptr::null_mut();
            };
            cli_completion(complete_inventory_cli(
                operation,
                &completion.arguments,
                &completion.prefix,
                completion.ordinal,
            ))
        }
        CliPhase::Execute => {
            let Some(arguments) = arguments else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            let Ok(invocation) = arguments.invocation(
                3,
                |count| count <= MAX_CLI_ARGUMENTS,
                |_| Some(MAX_CLI_ARGUMENT_BYTES),
            ) else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            execute_inventory_cli(invocation.fd, operation, &invocation.arguments);
            ptr::null_mut()
        }
    }
}

macro_rules! inventory_cli_handler {
    ($name:ident, $operation:expr, $command:expr, $usage:expr) => {
        unsafe extern "C" fn $name(
            entry: *mut sys::ast_cli_entry,
            command: c_int,
            arguments: *mut sys::ast_cli_args,
        ) -> *mut c_char {
            callback_guard(ptr::null_mut(), || unsafe {
                run_inventory_cli(
                    NonNull::new(entry),
                    CliPhase::from_raw(command),
                    CliArgs::from_raw(arguments),
                    $operation,
                    $command,
                    $usage,
                )
            })
        }
    };
}

inventory_cli_handler!(
    cli_devices,
    CliInventoryCommand::Devices,
    c"sccp show devices",
    c"Usage: sccp show devices [device [appearances [device:instance]|buttons [position]|capabilities [position]|features [name]]]\n"
);
inventory_cli_handler!(
    cli_lines,
    CliInventoryCommand::Lines,
    c"sccp show lines",
    c"Usage: sccp show lines [line [appearances [device:instance]]]\n"
);
inventory_cli_handler!(
    cli_channels,
    CliInventoryCommand::Channels,
    c"sccp show channels",
    c"Usage: sccp show channels [pbx-call-id]\n"
);

unsafe fn run_diagnostic_cli(
    entry: Option<NonNull<sys::ast_cli_entry>>,
    phase: CliPhase,
    arguments: Option<CliArgs<'_>>,
    operation: CliDiagnosticCommand,
    command_words: usize,
    command_text: &'static CStr,
    usage: &'static CStr,
) -> *mut c_char {
    match phase {
        CliPhase::Initialize => {
            if let Some(mut entry) = entry {
                unsafe {
                    entry.as_mut().command = command_text.as_ptr().cast_mut();
                    entry.as_mut().usage = usage.as_ptr();
                }
            }
            ptr::null_mut()
        }
        CliPhase::Generate => {
            let Some(arguments) = arguments else {
                return ptr::null_mut();
            };
            let Ok(completion) = arguments.completion(
                command_words,
                |count| count <= MAX_CLI_DIAGNOSTIC_ARGUMENTS,
                |_| Some(MAX_CLI_DIAGNOSTIC_ARGUMENT_BYTES),
            ) else {
                return ptr::null_mut();
            };
            cli_completion(complete_diagnostic_cli(
                operation,
                &completion.arguments,
                &completion.prefix,
                completion.ordinal,
            ))
        }
        CliPhase::Execute => {
            let Some(arguments) = arguments else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            let Ok(invocation) = arguments.invocation(
                command_words,
                |count| count <= MAX_CLI_DIAGNOSTIC_ARGUMENTS,
                |_| Some(MAX_CLI_DIAGNOSTIC_ARGUMENT_BYTES),
            ) else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            execute_diagnostic_cli(invocation.fd, operation, &invocation.arguments);
            ptr::null_mut()
        }
    }
}

macro_rules! diagnostic_cli_handler {
    ($name:ident, $operation:expr, $words:expr, $command:expr, $usage:expr) => {
        unsafe extern "C" fn $name(
            entry: *mut sys::ast_cli_entry,
            command: c_int,
            arguments: *mut sys::ast_cli_args,
        ) -> *mut c_char {
            callback_guard(ptr::null_mut(), || unsafe {
                run_diagnostic_cli(
                    NonNull::new(entry),
                    CliPhase::from_raw(command),
                    CliArgs::from_raw(arguments),
                    $operation,
                    $words,
                    $command,
                    $usage,
                )
            })
        }
    };
}

diagnostic_cli_handler!(
    cli_media,
    CliDiagnosticCommand::Media,
    3,
    c"sccp show media",
    c"Usage: sccp show media [pbx-call-id [call-id [audio|video [receive|transmit]]]]\n"
);
diagnostic_cli_handler!(
    cli_media_statistics,
    CliDiagnosticCommand::MediaStatistics,
    4,
    c"sccp show media statistics",
    c"Usage: sccp show media statistics [device [call-id]]\n"
);
diagnostic_cli_handler!(
    cli_sessions,
    CliDiagnosticCommand::Sessions,
    3,
    c"sccp show sessions",
    c"Usage: sccp show sessions [device]\n"
);

unsafe fn run_device_control_cli(
    entry: Option<NonNull<sys::ast_cli_entry>>,
    phase: CliPhase,
    arguments: Option<CliArgs<'_>>,
    mode: ResetMode,
    command_text: &'static CStr,
    usage: &'static CStr,
) -> *mut c_char {
    match phase {
        CliPhase::Initialize => {
            if let Some(mut entry) = entry {
                unsafe {
                    entry.as_mut().command = command_text.as_ptr().cast_mut();
                    entry.as_mut().usage = usage.as_ptr();
                }
            }
            ptr::null_mut()
        }
        CliPhase::Generate => {
            let Some(arguments) = arguments else {
                return ptr::null_mut();
            };
            let Ok(completion) = arguments
                .completion_cursor(2, |index| (index == 0).then_some(MAX_DEVICE_SELECTOR_BYTES))
            else {
                return ptr::null_mut();
            };
            cli_completion(complete_device_control_cli(
                &completion.prefix,
                completion.ordinal,
            ))
        }
        CliPhase::Execute => {
            let Some(arguments) = arguments else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            let Ok(invocation) =
                arguments.invocation(2, |count| count == 1, |_| Some(MAX_DEVICE_SELECTOR_BYTES))
            else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            let [device] = invocation.arguments.as_slice() else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            execute_device_control_cli(invocation.fd, device, mode);
            ptr::null_mut()
        }
    }
}

macro_rules! device_control_cli_handler {
    ($name:ident, $mode:expr, $command:expr, $usage:expr) => {
        unsafe extern "C" fn $name(
            entry: *mut sys::ast_cli_entry,
            command: c_int,
            arguments: *mut sys::ast_cli_args,
        ) -> *mut c_char {
            callback_guard(ptr::null_mut(), || unsafe {
                run_device_control_cli(
                    NonNull::new(entry),
                    CliPhase::from_raw(command),
                    CliArgs::from_raw(arguments),
                    $mode,
                    $command,
                    $usage,
                )
            })
        }
    };
}

device_control_cli_handler!(
    cli_reset,
    ResetMode::Reset,
    c"sccp reset",
    c"Usage: sccp reset <device>\n"
);
device_control_cli_handler!(
    cli_restart,
    ResetMode::Restart,
    c"sccp restart",
    c"Usage: sccp restart <device>\n"
);

unsafe fn run_control_cli(
    entry: Option<NonNull<sys::ast_cli_entry>>,
    phase: CliPhase,
    arguments: Option<CliArgs<'_>>,
    operation: ControlCliCommand,
    command_text: &'static CStr,
    usage: &'static CStr,
) -> *mut c_char {
    match phase {
        CliPhase::Initialize => {
            if let Some(mut entry) = entry {
                unsafe {
                    entry.as_mut().command = command_text.as_ptr().cast_mut();
                    entry.as_mut().usage = usage.as_ptr();
                }
            }
            ptr::null_mut()
        }
        CliPhase::Generate => {
            let Some(arguments) = arguments else {
                return ptr::null_mut();
            };
            let Ok(completion) =
                arguments.completion_cursor(2, |index| operation.argument_bound(index))
            else {
                return ptr::null_mut();
            };
            let context = if operation == ControlCliCommand::Originate && completion.position == 4 {
                arguments
                    .optional_argument(2, MAX_DEVICE_SELECTOR_BYTES)
                    .ok()
                    .flatten()
            } else {
                None
            };
            cli_completion(complete_control_cli(
                operation,
                completion.position,
                &completion.prefix,
                completion.ordinal,
                context.as_deref(),
            ))
        }
        CliPhase::Execute => {
            let Some(arguments) = arguments else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            let Ok(invocation) = arguments.invocation(
                2,
                |count| operation.accepts_argument_count(count),
                |index| operation.argument_bound(index),
            ) else {
                return cli_disposition_pointer(CliDisposition::ShowUsage);
            };
            execute_control_cli(invocation.fd, operation, &invocation.arguments);
            ptr::null_mut()
        }
    }
}

macro_rules! control_cli_handler {
    ($name:ident, $operation:expr, $command:expr, $usage:expr) => {
        unsafe extern "C" fn $name(
            entry: *mut sys::ast_cli_entry,
            command: c_int,
            arguments: *mut sys::ast_cli_args,
        ) -> *mut c_char {
            callback_guard(ptr::null_mut(), || unsafe {
                run_control_cli(
                    NonNull::new(entry),
                    CliPhase::from_raw(command),
                    CliArgs::from_raw(arguments),
                    $operation,
                    $command,
                    $usage,
                )
            })
        }
    };
}

control_cli_handler!(
    cli_dnd,
    ControlCliCommand::Dnd,
    c"sccp dnd",
    c"Usage: sccp dnd <device> <off|silent|reject>\n"
);
control_cli_handler!(
    cli_message,
    ControlCliCommand::Message,
    c"sccp message",
    c"Usage: sccp message <device|all|system> <text> [yes|no] [timeout]\n"
);
control_cli_handler!(
    cli_answer,
    ControlCliCommand::Answer,
    c"sccp answer",
    c"Usage: sccp answer <call-id> [device]\n"
);
control_cli_handler!(
    cli_end,
    ControlCliCommand::End,
    c"sccp end",
    c"Usage: sccp end <call-id>\n"
);
control_cli_handler!(
    cli_originate,
    ControlCliCommand::Originate,
    c"sccp originate",
    c"Usage: sccp originate <device> <number> [line] [assigned-channel-id]\n"
);

unsafe fn run_forwarding_cli(
    entry: Option<NonNull<sys::ast_cli_entry>>,
    phase: CliPhase,
    arguments: Option<CliArgs<'_>>,
) -> CliDisposition {
    match phase {
        CliPhase::Initialize => {
            let Some(mut entry) = entry else {
                return CliDisposition::Complete;
            };
            unsafe {
                entry.as_mut().command = c"sccp set forwarding".as_ptr().cast_mut();
                entry.as_mut().usage = c"Usage: sccp set forwarding <device> <line> <all|busy|noanswer> <destination|off>\n".as_ptr();
            }
            CliDisposition::Complete
        }
        CliPhase::Generate => CliDisposition::Complete,
        CliPhase::Execute => {
            let Some(arguments) = arguments else {
                return CliDisposition::ShowUsage;
            };
            let Ok(invocation) = arguments.invocation(3, |count| count == 4, |_| Some(256)) else {
                return CliDisposition::ShowUsage;
            };
            let [device, line, kind, destination] = invocation.arguments.as_slice() else {
                return CliDisposition::ShowUsage;
            };
            execute_forwarding_cli(invocation.fd, device, line, kind, destination);
            CliDisposition::Complete
        }
    }
}

unsafe extern "C" fn cli_forwarding(
    entry: *mut sys::ast_cli_entry,
    command: c_int,
    arguments: *mut sys::ast_cli_args,
) -> *mut c_char {
    callback_guard(ptr::null_mut(), || unsafe {
        cli_disposition_pointer(run_forwarding_cli(
            NonNull::new(entry),
            CliPhase::from_raw(command),
            CliArgs::from_raw(arguments),
        ))
    })
}

fn cli_entry(
    summary: &'static [u8],
    handler: unsafe extern "C" fn(
        *mut sys::ast_cli_entry,
        c_int,
        *mut sys::ast_cli_args,
    ) -> *mut c_char,
) -> sys::ast_cli_entry {
    let mut entry = unsafe { mem::zeroed::<sys::ast_cli_entry>() };
    entry.summary = summary.as_ptr().cast();
    entry.handler = Some(handler);
    entry
}

fn channel_technology() -> sys::ast_channel_tech {
    let mut technology = unsafe { mem::zeroed::<sys::ast_channel_tech>() };
    technology.type_ = SCCP_TYPE.as_ptr().cast();
    technology.description = SCCP_DESCRIPTION.as_ptr().cast();
    technology.properties =
        (sys::AST_CHAN_TP_WANTSJITTER | sys::AST_CHAN_TP_CREATESJITTER) as c_int;
    technology.requester_with_stream_topology = Some(requester_with_stream_topology);
    technology.devicestate = Some(device_state);
    technology.send_digit_begin = Some(send_digit_begin);
    technology.send_digit_end = Some(send_digit_end);
    technology.send_text = Some(send_text);
    technology.setoption = Some(set_option);
    technology.queryoption = Some(query_option);
    technology.call = Some(call);
    technology.hangup = Some(hangup);
    technology.answer = Some(answer);
    technology.read = Some(read);
    technology.write = Some(write);
    technology.write_video = Some(write);
    technology.exception = Some(read);
    technology.indicate = Some(indicate);
    technology.fixup = Some(fixup);
    technology.cc_callback = Some(call_completion);
    technology
}

fn rtp_glue() -> sys::ast_rtp_glue {
    let mut glue = unsafe { mem::zeroed::<sys::ast_rtp_glue>() };
    glue.type_ = SCCP_TYPE.as_ptr().cast();
    glue.get_rtp_info = Some(get_rtp_info);
    glue.get_vrtp_info = Some(get_vrtp_info);
    glue.update_peer = Some(update_peer);
    glue.get_codec = Some(get_codec);
    glue
}

unsafe fn technology_formats() -> impl Iterator<Item = *mut sys::ast_format> {
    unsafe {
        [
            sys::ast_format_ulaw,
            sys::ast_format_alaw,
            sys::ast_format_g722,
            sys::ast_format_g723,
            sys::ast_format_g729,
            sys::ast_format_g726_aal2,
            sys::ast_format_gsm,
            sys::ast_format_slin16,
            sys::ast_format_ilbc,
            sys::ast_format_siren7,
            sys::ast_format_opus,
            sys::ast_format_h261,
            sys::ast_format_h263,
            sys::ast_format_h263p,
            sys::ast_format_h264,
        ]
    }
    .into_iter()
}

pub(super) fn load() -> Result<(), ChannelDriverLoadError> {
    let mut native_registration = native_registration();
    if native_registration.is_some() {
        return Ok(());
    }
    let (technology, glue, cli) = unsafe {
        let technology = NonNull::new_unchecked(SCCP_TECH.write(channel_technology()));
        let glue = NonNull::new_unchecked(RTP_GLUE.write(rtp_glue()));
        let cli = NonNull::new_unchecked(CLI_ENTRIES.write([
            cli_entry(b"Show registered SCCP devices\0", cli_devices),
            cli_entry(b"Show configured SCCP lines\0", cli_lines),
            cli_entry(b"Show active SCCP channels\0", cli_channels),
            cli_entry(b"Show correlated SCCP media\0", cli_media),
            cli_entry(
                b"Show correlated SCCP media statistics\0",
                cli_media_statistics,
            ),
            cli_entry(b"Show active SCCP sessions\0", cli_sessions),
            cli_entry(b"Reload SCCP configuration\0", cli_reload),
            cli_entry(b"Reset a registered SCCP device\0", cli_reset),
            cli_entry(b"Restart a registered SCCP device\0", cli_restart),
            cli_entry(b"Set DND on a registered SCCP device\0", cli_dnd),
            cli_entry(b"Display a message on SCCP devices\0", cli_message),
            cli_entry(b"Answer a ringing SCCP call\0", cli_answer),
            cli_entry(b"End an SCCP call\0", cli_end),
            cli_entry(b"Originate an SCCP call\0", cli_originate),
            cli_entry(b"Set SCCP line forwarding\0", cli_forwarding),
            #[cfg(feature = "live-asterisk-tests")]
            crate::asterisk::raw::live_bridge_cli_entry(),
        ]));
        (technology, glue, cli)
    };

    if start_module().is_err() {
        crate::asterisk::raw::dialplan::cleanup();
        return Err(ChannelDriverLoadError);
    }
    let registration = unsafe {
        NativeChannelRegistration::register(
            technology,
            glue,
            cli,
            module_self(),
            technology_formats(),
        )
    };
    let Some(registration) = registration else {
        let _ = stop_module();
        crate::asterisk::raw::dialplan::cleanup();
        return Err(ChannelDriverLoadError);
    };
    *native_registration = Some(registration);
    Ok(())
}

pub(super) fn unload() -> Result<(), ModuleLifecycleError> {
    if has_active_channels() {
        return Err(ModuleLifecycleError);
    }
    let registration = native_registration().take();
    drop(registration);
    let result = stop_module();
    crate::asterisk::raw::dialplan::cleanup();
    result
}

pub(super) fn reload() -> Result<(), ModuleLifecycleError> {
    reload_module()
}
