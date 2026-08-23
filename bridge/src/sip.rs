//! A small, single-owner wrapper around PJSUA.
//!
//! PJSUA is deliberately kept on one operating-system thread.  The bridge
//! sends control commands to that thread and receives owned Rust events back;
//! no PJSUA pointer crosses the boundary.  Media is not connected to a sound
//! device.  Instead, the SDP callback advertises either the handset or one of
//! the bridge's packet-relay sockets.

#![allow(non_upper_case_globals, unsafe_op_in_unsafe_fn)]

use std::ffi::CString;
use std::mem::MaybeUninit;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::ptr;
use std::sync::{Mutex, mpsc as std_mpsc};
use std::thread;
use std::time::Duration;

use pjsua::*;
use sccp_protocol::Codec;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

const COMMAND_CAPACITY: usize = 1024;
const PJ_SUCCESS: i32 = pj_constants__PJ_SUCCESS as i32;

static EVENT_SENDER: Mutex<Option<mpsc::UnboundedSender<SipEvent>>> = Mutex::new(None);

#[derive(Clone, Debug)]
pub struct StackConfig {
    pub bind: SocketAddr,
    pub advertised_address: Option<Ipv4Addr>,
    pub max_calls: u32,
}

#[derive(Clone, Debug)]
pub struct AccountConfig {
    pub identity_uri: String,
    pub registrar_uri: String,
    pub username: String,
    pub auth_username: String,
    pub password: String,
    pub outbound_proxy: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AccountId(pub i32);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SipCallId(pub i32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DialogState {
    Calling,
    Incoming,
    Early,
    Connecting,
    Confirmed,
    Disconnected,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SipMediaState {
    None,
    Active,
    LocalHold,
    RemoteHold,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoteMedia {
    pub endpoint: SocketAddrV4,
    pub codec: Codec,
    pub payload_type: u8,
}

#[derive(Debug)]
pub enum SipEvent {
    Registration {
        account: AccountId,
        registered: bool,
        status: u16,
        reason: String,
    },
    IncomingCall {
        account: AccountId,
        call: SipCallId,
        remote_uri: String,
        remote_media: Option<RemoteMedia>,
    },
    CallState {
        account: AccountId,
        call: SipCallId,
        state: DialogState,
        status: u16,
        reason: String,
        remote_uri: String,
    },
    MediaState {
        call: SipCallId,
        state: SipMediaState,
        remote: Option<RemoteMedia>,
    },
    MediaReofferCompleted {
        call: SipCallId,
        accepted: bool,
        status: u16,
        reason: String,
    },
    MessageWaiting {
        account: AccountId,
        waiting: bool,
    },
    /// PJSUA is creating the initial answer to an incoming offer.  The
    /// coordinator must reserve the media route and reply with the address to
    /// place in SDP.  This avoids ever advertising PJSUA's unused sound path.
    MediaAdvertisementRequired {
        account: AccountId,
        call: SipCallId,
        reply: std_mpsc::Sender<Option<SocketAddrV4>>,
    },
}

#[derive(Debug, Error)]
pub enum SipError {
    #[error("SIP requires an IPv4 UDP bind address")]
    Ipv4Required,
    #[error("SIP worker failed to start: {0}")]
    Start(String),
    #[error("SIP worker has stopped")]
    Stopped,
    #[error("PJSUA operation failed: {0}")]
    Operation(String),
}

#[derive(Clone)]
pub struct SipHandle {
    command_tx: std_mpsc::SyncSender<Command>,
}

impl SipHandle {
    pub async fn add_account(&self, config: AccountConfig) -> Result<AccountId, SipError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::AddAccount { config, reply })?;
        receive
            .await
            .map_err(|_| SipError::Stopped)?
            .map_err(SipError::Operation)
    }

    pub async fn remove_account(&self, account: AccountId) -> Result<(), SipError> {
        self.unit(|reply| Command::RemoveAccount { account, reply })
            .await
    }

    pub async fn make_call(
        &self,
        account: AccountId,
        destination: String,
        advertised_media: SocketAddrV4,
    ) -> Result<SipCallId, SipError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::MakeCall {
            account,
            destination,
            advertised_media,
            reply,
        })?;
        receive
            .await
            .map_err(|_| SipError::Stopped)?
            .map_err(SipError::Operation)
    }

    pub async fn set_media_advertisement(
        &self,
        call: SipCallId,
        advertised_media: SocketAddrV4,
    ) -> Result<(), SipError> {
        self.unit(|reply| Command::SetMediaAdvertisement {
            call,
            advertised_media,
            reply,
        })
        .await
    }

    /// Regenerate and send the local SDP for an established dialog.
    /// Completion is reported through `SipEvent::MediaReofferCompleted`.
    pub async fn reoffer_media(
        &self,
        call: SipCallId,
        advertised_media: SocketAddrV4,
    ) -> Result<(), SipError> {
        self.unit(|reply| Command::ReofferMedia {
            call,
            advertised_media,
            reply,
        })
        .await
    }

    pub async fn answer(&self, call: SipCallId) -> Result<(), SipError> {
        self.unit(|reply| Command::Answer { call, reply }).await
    }

    pub async fn ringing(&self, call: SipCallId) -> Result<(), SipError> {
        self.reject(call, 180).await
    }

    pub async fn reject(&self, call: SipCallId, status: u16) -> Result<(), SipError> {
        self.unit(|reply| Command::Reject {
            call,
            status,
            reply,
        })
        .await
    }

    pub async fn hangup(&self, call: SipCallId) -> Result<(), SipError> {
        self.unit(|reply| Command::Hangup { call, reply }).await
    }

    pub async fn hold(&self, call: SipCallId) -> Result<(), SipError> {
        self.unit(|reply| Command::Hold { call, reply }).await
    }

    pub async fn resume(&self, call: SipCallId) -> Result<(), SipError> {
        self.unit(|reply| Command::Resume { call, reply }).await
    }

    pub async fn blind_transfer(
        &self,
        call: SipCallId,
        destination: String,
    ) -> Result<(), SipError> {
        self.unit(|reply| Command::BlindTransfer {
            call,
            destination,
            reply,
        })
        .await
    }

    pub async fn attended_transfer(
        &self,
        call: SipCallId,
        replaces: SipCallId,
    ) -> Result<(), SipError> {
        self.unit(|reply| Command::AttendedTransfer {
            call,
            replaces,
            reply,
        })
        .await
    }

    pub async fn send_dtmf(&self, call: SipCallId, digits: String) -> Result<(), SipError> {
        self.unit(|reply| Command::Dtmf {
            call,
            digits,
            reply,
        })
        .await
    }

    pub async fn shutdown(&self) -> Result<(), SipError> {
        self.unit(|reply| Command::Shutdown { reply }).await
    }

    fn send(&self, command: Command) -> Result<(), SipError> {
        self.command_tx.send(command).map_err(|_| SipError::Stopped)
    }

    async fn unit<F>(&self, build: F) -> Result<(), SipError>
    where
        F: FnOnce(oneshot::Sender<Result<(), String>>) -> Command,
    {
        let (reply, receive) = oneshot::channel();
        self.send(build(reply))?;
        receive
            .await
            .map_err(|_| SipError::Stopped)?
            .map_err(SipError::Operation)
    }
}

pub fn start(
    config: StackConfig,
) -> Result<(SipHandle, mpsc::UnboundedReceiver<SipEvent>), SipError> {
    if !config.bind.is_ipv4() {
        return Err(SipError::Ipv4Required);
    }
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (command_tx, command_rx) = std_mpsc::sync_channel(COMMAND_CAPACITY);
    let (startup_tx, startup_rx) = std_mpsc::sync_channel(1);
    thread::Builder::new()
        .name("pjsua-control".into())
        .spawn(move || run_worker(config, event_tx, command_rx, startup_tx))
        .map_err(|error| SipError::Start(error.to_string()))?;
    startup_rx
        .recv()
        .map_err(|_| SipError::Start("worker exited during initialization".into()))?
        .map_err(SipError::Start)?;
    Ok((SipHandle { command_tx }, event_rx))
}

enum Command {
    AddAccount {
        config: AccountConfig,
        reply: oneshot::Sender<Result<AccountId, String>>,
    },
    RemoveAccount {
        account: AccountId,
        reply: UnitReply,
    },
    MakeCall {
        account: AccountId,
        destination: String,
        advertised_media: SocketAddrV4,
        reply: oneshot::Sender<Result<SipCallId, String>>,
    },
    SetMediaAdvertisement {
        call: SipCallId,
        advertised_media: SocketAddrV4,
        reply: UnitReply,
    },
    ReofferMedia {
        call: SipCallId,
        advertised_media: SocketAddrV4,
        reply: UnitReply,
    },
    Answer {
        call: SipCallId,
        reply: UnitReply,
    },
    Reject {
        call: SipCallId,
        status: u16,
        reply: UnitReply,
    },
    Hangup {
        call: SipCallId,
        reply: UnitReply,
    },
    Hold {
        call: SipCallId,
        reply: UnitReply,
    },
    Resume {
        call: SipCallId,
        reply: UnitReply,
    },
    BlindTransfer {
        call: SipCallId,
        destination: String,
        reply: UnitReply,
    },
    AttendedTransfer {
        call: SipCallId,
        replaces: SipCallId,
        reply: UnitReply,
    },
    Dtmf {
        call: SipCallId,
        digits: String,
        reply: UnitReply,
    },
    Shutdown {
        reply: UnitReply,
    },
}

type UnitReply = oneshot::Sender<Result<(), String>>;

#[derive(Debug)]
struct CallMediaData {
    advertised_media: SocketAddrV4,
    reoffer_pending: bool,
}

fn run_worker(
    config: StackConfig,
    event_tx: mpsc::UnboundedSender<SipEvent>,
    command_rx: std_mpsc::Receiver<Command>,
    startup_tx: std_mpsc::SyncSender<Result<(), String>>,
) {
    *EVENT_SENDER.lock().expect("event sender mutex poisoned") = Some(event_tx);
    if let Err(error) = initialise(&config) {
        let _ = startup_tx.send(Err(error));
        *EVENT_SENDER.lock().expect("event sender mutex poisoned") = None;
        return;
    }
    let _ = startup_tx.send(Ok(()));
    info!(bind = %config.bind, "PJSUA control plane started");

    let mut running = true;
    let mut shutdown_reply = None;
    while running {
        while let Ok(command) = command_rx.try_recv() {
            if let Command::Shutdown { reply } = command {
                shutdown_reply = Some(reply);
                running = false;
            } else {
                execute(command);
            }
            if !running {
                break;
            }
        }
        if running {
            // With thread_cnt=0 this is the only place PJSIP callbacks run.
            unsafe { pjsua_handle_events(10) };
        }
    }

    unsafe {
        pjsua_call_hangup_all();
        pjsua_destroy();
    }
    *EVENT_SENDER.lock().expect("event sender mutex poisoned") = None;
    info!("PJSUA control plane stopped");
    if let Some(reply) = shutdown_reply {
        let _ = reply.send(Ok(()));
    }
}

fn initialise(config: &StackConfig) -> Result<(), String> {
    let SocketAddr::V4(bind) = config.bind else {
        return Err("IPv4 bind address required".into());
    };
    unsafe {
        check(pjsua_create(), "pjsua_create")?;

        let mut ua = MaybeUninit::<pjsua_config>::zeroed().assume_init();
        pjsua_config_default(&mut ua);
        ua.thread_cnt = 0;
        ua.max_calls = config.max_calls.max(2);
        ua.enable_unsolicited_mwi = pj_constants__PJ_TRUE as i32;
        ua.cb.on_incoming_call = Some(on_incoming_call);
        ua.cb.on_call_state = Some(on_call_state);
        ua.cb.on_call_tsx_state = Some(on_call_tsx_state);
        ua.cb.on_call_media_state = Some(on_call_media_state);
        ua.cb.on_call_sdp_created = Some(on_call_sdp_created);
        ua.cb.on_reg_state = Some(on_reg_state);
        ua.cb.on_mwi_info = Some(on_mwi_info);

        let mut logging = MaybeUninit::<pjsua_logging_config>::zeroed().assume_init();
        pjsua_logging_config_default(&mut logging);
        logging.console_level = 3;

        let mut media = MaybeUninit::<pjsua_media_config>::zeroed().assume_init();
        pjsua_media_config_default(&mut media);
        media.clock_rate = 8_000;
        media.snd_clock_rate = 8_000;
        media.channel_count = 1;
        media.audio_frame_ptime = 20;
        media.no_vad = pj_constants__PJ_TRUE as i32;
        media.thread_cnt = 0;
        check(pjsua_init(&ua, &logging, &media), "pjsua_init")?;

        let mut transport = MaybeUninit::<pjsua_transport_config>::zeroed().assume_init();
        pjsua_transport_config_default(&mut transport);
        transport.port = u32::from(bind.port());
        let bound = CString::new(bind.ip().to_string()).map_err(|error| error.to_string())?;
        transport.bound_addr = pj_string(&bound);
        let advertised = config
            .advertised_address
            .map(|ip| CString::new(ip.to_string()).unwrap());
        if let Some(address) = advertised.as_ref() {
            transport.public_addr = pj_string(address);
        }
        let mut transport_id = -1;
        check(
            pjsua_transport_create(
                pjsip_transport_type_e_PJSIP_TRANSPORT_UDP,
                &transport,
                &mut transport_id,
            ),
            "pjsua_transport_create",
        )?;
        check(pjsua_start(), "pjsua_start")?;
        pjsua_set_no_snd_dev();

        set_codec_priority("*", 0)?;
        set_codec_priority("PCMU/8000", 255)?;
        set_codec_priority("PCMA/8000", 254)?;
        // RFC 4733 telephone-event is negotiated by PJSUA independently of
        // the audio codec registry.
    }
    Ok(())
}

fn execute(command: Command) {
    match command {
        Command::AddAccount { config, reply } => {
            let _ = reply.send(add_account(&config));
        }
        Command::RemoveAccount { account, reply } => reply_unit(reply, unsafe {
            check(pjsua_acc_del(account.0), "pjsua_acc_del")
        }),
        Command::MakeCall {
            account,
            destination,
            advertised_media,
            reply,
        } => {
            let _ = reply.send(make_call(account, &destination, advertised_media));
        }
        Command::SetMediaAdvertisement {
            call,
            advertised_media,
            reply,
        } => {
            reply_unit(reply, set_media_advertisement(call, advertised_media));
        }
        Command::ReofferMedia {
            call,
            advertised_media,
            reply,
        } => reply_unit(reply, reoffer_media(call, advertised_media)),
        Command::Answer { call, reply } => reply_unit(reply, unsafe {
            check(
                pjsua_call_answer(call.0, 200, ptr::null(), ptr::null()),
                "pjsua_call_answer",
            )
        }),
        Command::Reject {
            call,
            status,
            reply,
        } => reply_unit(reply, unsafe {
            check(
                pjsua_call_answer(call.0, u32::from(status), ptr::null(), ptr::null()),
                "pjsua_call_answer(reject)",
            )
        }),
        Command::Hangup { call, reply } => reply_unit(reply, unsafe {
            check(
                pjsua_call_hangup(call.0, 0, ptr::null(), ptr::null()),
                "pjsua_call_hangup",
            )
        }),
        Command::Hold { call, reply } => reply_unit(reply, unsafe {
            check(
                pjsua_call_set_hold(call.0, ptr::null()),
                "pjsua_call_set_hold",
            )
        }),
        Command::Resume { call, reply } => reply_unit(reply, unsafe {
            check(
                pjsua_call_reinvite(call.0, pjsua_call_flag_PJSUA_CALL_UNHOLD, ptr::null()),
                "pjsua_call_reinvite(unhold)",
            )
        }),
        Command::BlindTransfer {
            call,
            destination,
            reply,
        } => {
            reply_unit(
                reply,
                with_pj_string(&destination, |uri| unsafe {
                    check(pjsua_call_xfer(call.0, uri, ptr::null()), "pjsua_call_xfer")
                }),
            );
        }
        Command::AttendedTransfer {
            call,
            replaces,
            reply,
        } => reply_unit(reply, unsafe {
            check(
                pjsua_call_xfer_replaces(call.0, replaces.0, 0, ptr::null()),
                "pjsua_call_xfer_replaces",
            )
        }),
        Command::Dtmf {
            call,
            digits,
            reply,
        } => {
            reply_unit(
                reply,
                with_pj_string(&digits, |value| unsafe {
                    check(pjsua_call_dial_dtmf(call.0, value), "pjsua_call_dial_dtmf")
                }),
            );
        }
        Command::Shutdown { .. } => unreachable!("shutdown is handled by the worker loop"),
    }
}

fn add_account(config: &AccountConfig) -> Result<AccountId, String> {
    let identity = CString::new(config.identity_uri.as_str()).map_err(|error| error.to_string())?;
    let registrar =
        CString::new(config.registrar_uri.as_str()).map_err(|error| error.to_string())?;
    let realm = CString::new("*").unwrap();
    let scheme = CString::new("digest").unwrap();
    let username =
        CString::new(config.auth_username.as_str()).map_err(|error| error.to_string())?;
    let password = CString::new(config.password.as_str()).map_err(|error| error.to_string())?;
    let proxy = config
        .outbound_proxy
        .as_deref()
        .map(CString::new)
        .transpose()
        .map_err(|error| error.to_string())?;

    unsafe {
        let mut account = MaybeUninit::<pjsua_acc_config>::zeroed().assume_init();
        pjsua_acc_config_default(&mut account);
        account.id = pj_string(&identity);
        account.reg_uri = pj_string(&registrar);
        account.mwi_enabled = pj_constants__PJ_TRUE as i32;
        account.register_on_acc_add = pj_constants__PJ_TRUE as i32;
        account.cred_count = 1;
        account.cred_info[0].realm = pj_string(&realm);
        account.cred_info[0].scheme = pj_string(&scheme);
        account.cred_info[0].username = pj_string(&username);
        account.cred_info[0].data_type = pjsip_cred_data_type_PJSIP_CRED_DATA_PLAIN_PASSWD as i32;
        account.cred_info[0].data = pj_string(&password);
        if let Some(proxy) = proxy.as_ref() {
            account.proxy_cnt = 1;
            account.proxy[0] = pj_string(proxy);
        }
        let mut id = -1;
        check(
            pjsua_acc_add(&account, pj_constants__PJ_FALSE as i32, &mut id),
            "pjsua_acc_add",
        )?;
        debug!(account = id, identity = %config.identity_uri, "SIP account added");
        Ok(AccountId(id))
    }
}

fn make_call(
    account: AccountId,
    destination: &str,
    advertised_media: SocketAddrV4,
) -> Result<SipCallId, String> {
    let destination = normalize_uri(destination);
    let destination = CString::new(destination).map_err(|error| error.to_string())?;
    let mut id = -1;
    let media = Box::new(CallMediaData {
        advertised_media,
        reoffer_pending: false,
    });
    let media_ptr = Box::into_raw(media);
    let status = unsafe {
        pjsua_call_make_call(
            account.0,
            &pj_string(&destination),
            ptr::null(),
            media_ptr.cast(),
            ptr::null(),
            &mut id,
        )
    };
    if status != PJ_SUCCESS {
        unsafe { drop(Box::from_raw(media_ptr)) };
        return Err(status_error("pjsua_call_make_call", status));
    }
    Ok(SipCallId(id))
}

fn set_media_advertisement(call: SipCallId, advertised_media: SocketAddrV4) -> Result<(), String> {
    unsafe {
        let current = pjsua_call_get_user_data(call.0).cast::<CallMediaData>();
        if current.is_null() {
            let data = Box::into_raw(Box::new(CallMediaData {
                advertised_media,
                reoffer_pending: false,
            }));
            if let Err(error) = check(
                pjsua_call_set_user_data(call.0, data.cast()),
                "pjsua_call_set_user_data",
            ) {
                drop(Box::from_raw(data));
                return Err(error);
            }
        } else {
            (*current).advertised_media = advertised_media;
        }
    }
    Ok(())
}

fn reoffer_media(call: SipCallId, advertised_media: SocketAddrV4) -> Result<(), String> {
    unsafe {
        let data = pjsua_call_get_user_data(call.0).cast::<CallMediaData>();
        if data.is_null() {
            return Err("call has no media advertisement state".into());
        }
        if (*data).reoffer_pending {
            return Err("a media re-offer is already in progress".into());
        }
        let previous = (*data).advertised_media;
        (*data).advertised_media = advertised_media;
        (*data).reoffer_pending = true;
        if let Err(error) = check(
            pjsua_call_reinvite(call.0, 0, ptr::null()),
            "pjsua_call_reinvite(media re-offer)",
        ) {
            (*data).advertised_media = previous;
            (*data).reoffer_pending = false;
            return Err(error);
        }
    }
    Ok(())
}

unsafe extern "C" fn on_incoming_call(
    acc_id: pjsua_acc_id,
    call_id: pjsua_call_id,
    _rdata: *mut pjsip_rx_data,
) {
    let remote_uri = call_details(call_id).map_or_else(String::new, |details| details.remote_uri);
    send_event(SipEvent::IncomingCall {
        account: AccountId(acc_id),
        call: SipCallId(call_id),
        remote_uri,
        remote_media: remote_media(call_id),
    });
}

unsafe extern "C" fn on_call_state(call_id: pjsua_call_id, _event: *mut pjsip_event) {
    let Some(details) = call_details(call_id) else {
        return;
    };
    send_event(SipEvent::CallState {
        account: details.account,
        call: SipCallId(call_id),
        state: details.state,
        status: details.status,
        reason: details.reason,
        remote_uri: details.remote_uri,
    });
    if details.state == DialogState::Disconnected {
        let data = pjsua_call_get_user_data(call_id).cast::<CallMediaData>();
        if !data.is_null() {
            let _ = pjsua_call_set_user_data(call_id, ptr::null_mut());
            drop(Box::from_raw(data));
        }
    }
}

unsafe extern "C" fn on_call_tsx_state(
    call_id: pjsua_call_id,
    transaction: *mut pjsip_transaction,
    _event: *mut pjsip_event,
) {
    if transaction.is_null()
        || (*transaction).method.id != pjsip_method_e_PJSIP_INVITE_METHOD
        || (*transaction).role != pjsip_role_e_PJSIP_ROLE_UAC
    {
        return;
    }
    let data = pjsua_call_get_user_data(call_id).cast::<CallMediaData>();
    if data.is_null() || !(*data).reoffer_pending {
        return;
    }
    let Some(status) =
        completed_transaction_status((*transaction).state, (*transaction).status_code)
    else {
        return;
    };
    // PJSIP may transparently retry an authenticated re-INVITE as a new
    // transaction, so these challenges are not the operation's completion.
    if matches!(status, 401 | 407) {
        return;
    }
    (*data).reoffer_pending = false;
    send_event(SipEvent::MediaReofferCompleted {
        call: SipCallId(call_id),
        accepted: (200..300).contains(&status),
        status,
        reason: pj_to_string((*transaction).status_text),
    });
}

fn completed_transaction_status(state: pjsip_tsx_state_e, status: i32) -> Option<u16> {
    if state == pjsip_tsx_state_e_PJSIP_TSX_STATE_COMPLETED && status >= 200 {
        return Some(status.try_into().unwrap_or(u16::MAX));
    }
    if state == pjsip_tsx_state_e_PJSIP_TSX_STATE_TERMINATED {
        return Some(if status >= 200 {
            status.try_into().unwrap_or(u16::MAX)
        } else {
            408
        });
    }
    None
}

unsafe extern "C" fn on_call_media_state(call_id: pjsua_call_id) {
    let mut info = MaybeUninit::<pjsua_call_info>::zeroed().assume_init();
    if pjsua_call_get_info(call_id, &mut info) != PJ_SUCCESS {
        return;
    }
    let state = match info.media_status {
        pjsua_call_media_status_PJSUA_CALL_MEDIA_ACTIVE => SipMediaState::Active,
        pjsua_call_media_status_PJSUA_CALL_MEDIA_LOCAL_HOLD => SipMediaState::LocalHold,
        pjsua_call_media_status_PJSUA_CALL_MEDIA_REMOTE_HOLD => SipMediaState::RemoteHold,
        pjsua_call_media_status_PJSUA_CALL_MEDIA_ERROR => SipMediaState::Error,
        _ => SipMediaState::None,
    };
    send_event(SipEvent::MediaState {
        call: SipCallId(call_id),
        state,
        remote: remote_media(call_id),
    });
}

unsafe extern "C" fn on_call_sdp_created(
    call_id: pjsua_call_id,
    sdp: *mut pjmedia_sdp_session,
    pool: *mut pj_pool_t,
    _remote_sdp: *const pjmedia_sdp_session,
) {
    if sdp.is_null() || pool.is_null() {
        return;
    }
    let mut data = pjsua_call_get_user_data(call_id).cast::<CallMediaData>();
    if data.is_null()
        && let Some(details) = call_details(call_id)
    {
        let (reply, receive) = std_mpsc::channel();
        send_event(SipEvent::MediaAdvertisementRequired {
            account: details.account,
            call: SipCallId(call_id),
            reply,
        });
        if let Ok(Some(address)) = receive.recv_timeout(Duration::from_secs(2)) {
            data = Box::into_raw(Box::new(CallMediaData {
                advertised_media: address,
                reoffer_pending: false,
            }));
            if pjsua_call_set_user_data(call_id, data.cast()) != PJ_SUCCESS {
                drop(Box::from_raw(data));
                return;
            }
        }
    }
    if !data.is_null() {
        rewrite_sdp(sdp, pool, (*data).advertised_media);
    }
}

unsafe extern "C" fn on_reg_state(acc_id: pjsua_acc_id) {
    let mut info = MaybeUninit::<pjsua_acc_info>::zeroed().assume_init();
    if pjsua_acc_get_info(acc_id, &mut info) != PJ_SUCCESS {
        return;
    }
    send_event(SipEvent::Registration {
        account: AccountId(acc_id),
        registered: (200..300).contains(&info.status) && info.expires > 0,
        status: info.status.try_into().unwrap_or(u16::MAX),
        reason: pj_to_string(info.status_text),
    });
}

unsafe extern "C" fn on_mwi_info(acc_id: pjsua_acc_id, info: *mut pjsua_mwi_info) {
    let waiting = if info.is_null() || (*info).rdata.is_null() {
        true
    } else {
        let rdata = &*(*info).rdata;
        let len = rdata.pkt_info.len.max(0) as usize;
        let packet = std::slice::from_raw_parts(
            rdata.pkt_info.packet.as_ptr().cast::<u8>(),
            len.min(rdata.pkt_info.packet.len()),
        );
        let text = String::from_utf8_lossy(packet).to_ascii_lowercase();
        !text.contains("messages-waiting: no")
    };
    send_event(SipEvent::MessageWaiting {
        account: AccountId(acc_id),
        waiting,
    });
}

struct CallDetails {
    account: AccountId,
    state: DialogState,
    status: u16,
    reason: String,
    remote_uri: String,
}

unsafe fn call_details(call_id: i32) -> Option<CallDetails> {
    let mut info = MaybeUninit::<pjsua_call_info>::zeroed().assume_init();
    if pjsua_call_get_info(call_id, &mut info) != PJ_SUCCESS {
        return None;
    }
    let state = match info.state {
        pjsip_inv_state_PJSIP_INV_STATE_CALLING => DialogState::Calling,
        pjsip_inv_state_PJSIP_INV_STATE_INCOMING => DialogState::Incoming,
        pjsip_inv_state_PJSIP_INV_STATE_EARLY => DialogState::Early,
        pjsip_inv_state_PJSIP_INV_STATE_CONNECTING => DialogState::Connecting,
        pjsip_inv_state_PJSIP_INV_STATE_CONFIRMED => DialogState::Confirmed,
        pjsip_inv_state_PJSIP_INV_STATE_DISCONNECTED => DialogState::Disconnected,
        _ => DialogState::Unknown,
    };
    Some(CallDetails {
        account: AccountId(info.acc_id),
        state,
        status: info.last_status.try_into().unwrap_or(u16::MAX),
        reason: pj_to_string(info.last_status_text),
        remote_uri: pj_to_string(info.remote_info),
    })
}

unsafe fn remote_media(call_id: i32) -> Option<RemoteMedia> {
    let mut stream = MaybeUninit::<pjsua_stream_info>::zeroed().assume_init();
    if pjsua_call_get_stream_info(call_id, 0, &mut stream) != PJ_SUCCESS
        || stream.type_ != pjmedia_type_PJMEDIA_TYPE_AUDIO
    {
        return None;
    }
    let audio = stream.info.aud;
    let remote_address = (&audio.rem_addr as *const pj_sockaddr).cast::<pj_sockaddr_t>();
    if pj_sockaddr_get_addr_len(remote_address) != 4 || pj_sockaddr_has_addr(remote_address) == 0 {
        return None;
    }
    let address = pj_sockaddr_get_addr(remote_address).cast::<u8>();
    if address.is_null() {
        return None;
    }
    let octets = std::slice::from_raw_parts(address, 4);
    let endpoint = SocketAddrV4::new(
        Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3]),
        pj_sockaddr_get_port(remote_address),
    );
    let payload_type = audio.tx_pt.try_into().ok()?;
    let codec = match payload_type {
        0 => Codec::Pcmu,
        8 => Codec::Pcma,
        _ => return None,
    };
    Some(RemoteMedia {
        endpoint,
        codec,
        payload_type,
    })
}

unsafe fn rewrite_sdp(sdp: *mut pjmedia_sdp_session, pool: *mut pj_pool_t, address: SocketAddrV4) {
    let ip = CString::new(address.ip().to_string()).unwrap();
    pj_strdup2(pool, &mut (*sdp).origin.addr, ip.as_ptr());
    if !(*sdp).conn.is_null() {
        pj_strdup2(pool, &mut (*(*sdp).conn).addr, ip.as_ptr());
    }
    for index in 0..((*sdp).media_count as usize).min((*sdp).media.len()) {
        let media = (*sdp).media[index];
        if media.is_null() || pj_to_string((*media).desc.media) != "audio" {
            continue;
        }
        (*media).desc.port = address.port();
        if !(*media).conn.is_null() {
            pj_strdup2(pool, &mut (*(*media).conn).addr, ip.as_ptr());
        }
    }
}

fn send_event(event: SipEvent) {
    let sender = EVENT_SENDER
        .lock()
        .expect("event sender mutex poisoned")
        .clone();
    if let Some(sender) = sender
        && let Err(error) = sender.send(event)
    {
        warn!(%error, "dropping SIP event because coordinator stopped");
    }
}

fn reply_unit(reply: UnitReply, result: Result<(), String>) {
    let _ = reply.send(result);
}

fn normalize_uri(value: &str) -> String {
    if value.starts_with("sip:") || value.starts_with("sips:") {
        value.to_string()
    } else {
        format!("sip:{value}")
    }
}

fn with_pj_string<T>(
    value: &str,
    operation: impl FnOnce(*const pj_str_t) -> Result<T, String>,
) -> Result<T, String> {
    let value = CString::new(value).map_err(|error| error.to_string())?;
    operation(&pj_string(&value))
}

fn pj_string(value: &CString) -> pj_str_t {
    pj_str_t {
        ptr: value.as_ptr().cast_mut(),
        slen: value.as_bytes().len() as i64,
    }
}

unsafe fn pj_to_string(value: pj_str_t) -> String {
    if value.ptr.is_null() || value.slen <= 0 {
        String::new()
    } else {
        String::from_utf8_lossy(std::slice::from_raw_parts(
            value.ptr.cast::<u8>(),
            value.slen as usize,
        ))
        .into_owned()
    }
}

unsafe fn set_codec_priority(codec: &str, priority: u8) -> Result<(), String> {
    with_pj_string(codec, |value| {
        check(
            pjsua_codec_set_priority(value, priority),
            "pjsua_codec_set_priority",
        )
    })
}

fn check(status: i32, operation: &str) -> Result<(), String> {
    if status == PJ_SUCCESS {
        Ok(())
    } else {
        Err(status_error(operation, status))
    }
}

fn status_error(operation: &str, status: i32) -> String {
    error!(%operation, %status, "PJSUA operation failed");
    format!("{operation} returned status {status}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_bare_destination() {
        assert_eq!(normalize_uri("1001@example.test"), "sip:1001@example.test");
        assert_eq!(
            normalize_uri("sip:1001@example.test"),
            "sip:1001@example.test"
        );
    }

    #[test]
    fn classifies_completed_and_timed_out_transactions() {
        assert_eq!(
            completed_transaction_status(pjsip_tsx_state_e_PJSIP_TSX_STATE_COMPLETED, 200,),
            Some(200)
        );
        assert_eq!(
            completed_transaction_status(pjsip_tsx_state_e_PJSIP_TSX_STATE_TERMINATED, 0,),
            Some(408)
        );
        assert_eq!(
            completed_transaction_status(pjsip_tsx_state_e_PJSIP_TSX_STATE_PROCEEDING, 183,),
            None
        );
    }
}
