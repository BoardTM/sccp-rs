//! Rust-owned Asterisk module descriptor and ELF registration hooks.

use std::ffi::{c_char, c_int};
use std::mem;
use std::ptr;

use crate::asterisk::boundary::contain_panic as callback_guard;
use crate::asterisk::sys;

use super::channel_driver;
use crate::asterisk::StaticDescriptor;

const MODULE_NAME: &[u8] = b"chan_sccp2\0";
const DESCRIPTION: &[u8] = b"Rust SCCP Channel Driver\0";
const GPL_KEY: &[u8] = b"This paragraph is copyright (c) 2006 by Digium, Inc. In order for your module to load, it must return this key via a function called \"key\".  Any code which includes this paragraph must be licensed under the GNU General Public License version 2 or later (at your option).  In addition to Digium's general reservations of rights, Digium expressly reserves the right to allow other parties to license this paragraph under different terms. Any use of Digium, Inc. trademarks or logos (including \"Asterisk\" or \"Digium\") without express written permission of Digium, Inc. is prohibited.\n\0";

static MODULE_INFO: StaticDescriptor<sys::ast_module_info> = StaticDescriptor::uninit();

unsafe extern "C" fn load() -> sys::ast_module_load_result {
    callback_guard(sys::AST_MODULE_LOAD_DECLINE, || {
        channel_driver::load()
            .map(|()| sys::AST_MODULE_LOAD_SUCCESS)
            .unwrap_or(sys::AST_MODULE_LOAD_DECLINE)
    })
}

unsafe extern "C" fn unload() -> c_int {
    callback_guard(-1, || {
        crate::asterisk::boundary::CallbackStatus::from_result(channel_driver::unload()).as_raw()
    })
}

unsafe extern "C" fn reload() -> c_int {
    callback_guard(-1, || {
        crate::asterisk::boundary::CallbackStatus::from_result(channel_driver::reload()).as_raw()
    })
}

fn buildopt_sum() -> [c_char; 33] {
    let bytes = env!("SCCP_ASTERISK_BUILDOPT_SUM").as_bytes();
    assert!(
        bytes.len() <= 32,
        "Asterisk build option sum exceeds ABI field"
    );
    let mut output = [0; 33];
    for (target, source) in output.iter_mut().zip(bytes.iter().copied()) {
        *target = source as c_char;
    }
    output
}

unsafe extern "C" fn register_module() {
    callback_guard((), || {
        // Zero initialization exactly mirrors the C designated initializer:
        // reserved pointers and optional dependency strings remain null.
        let mut info = unsafe { mem::zeroed::<sys::ast_module_info>() };
        info.load = Some(load);
        info.reload = Some(reload);
        info.unload = Some(unload);
        info.name = MODULE_NAME.as_ptr().cast();
        info.description = DESCRIPTION.as_ptr().cast();
        info.key = GPL_KEY.as_ptr().cast();
        info.flags = sys::AST_MODFLAG_LOAD_ORDER;
        info.buildopt_sum = buildopt_sum();
        info.load_pri = sys::AST_MODPRI_CHANNEL_DRIVER as u8;
        info.support_level = sys::AST_MODULE_SUPPORT_EXTENDED;

        // SAFETY: the ELF constructor is the unique initializer, the storage
        // address is stable for the life of the DSO, and Asterisk retains it
        // until the matching destructor unregisters it.
        let info = unsafe { MODULE_INFO.write(info) };
        unsafe { sys::ast_module_register(info) };
    });
}

unsafe extern "C" fn unregister_module() {
    callback_guard((), || {
        // SAFETY: Asterisk runs the DSO destructor after the module lifecycle;
        // registration initialized the descriptor at this stable address.
        unsafe { sys::ast_module_unregister(MODULE_INFO.as_ptr()) };
    });
}

// These arrays are the Rust equivalent of Asterisk's constructor/destructor
// attributes used by AST_MODULE_INFO.
#[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
#[used]
static REGISTER_MODULE: unsafe extern "C" fn() = register_module;

#[cfg_attr(target_os = "linux", unsafe(link_section = ".fini_array"))]
#[used]
static UNREGISTER_MODULE: unsafe extern "C" fn() = unregister_module;

/// Asterisk's macros use this symbol to obtain the loader-populated module
/// handle when registering owned resources.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __internal_chan_sccp2_self() -> *mut sys::ast_module {
    callback_guard(ptr::null_mut(), || {
        // SAFETY: the loader calls this only after registration initialized the
        // descriptor; reading the loader-owned `self` field is atomic within
        // the serialized module lifecycle.
        unsafe { (*MODULE_INFO.as_ptr()).self_ }
    })
}

pub unsafe fn module_self() -> *mut sys::ast_module {
    // SAFETY: callers use this only while the module is registered.
    unsafe { __internal_chan_sccp2_self() }
}
