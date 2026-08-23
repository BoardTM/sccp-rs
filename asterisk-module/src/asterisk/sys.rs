//! Bindgen output for the configured upstream Asterisk headers.
//!
//! This module is private to the Asterisk integration. Domain modules must use
//! owned ports and values from their own crates/modules instead of importing
//! upstream layouts or raw pointers from here.

#![allow(
    dead_code,
    improper_ctypes,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unnecessary_transmutes,
    unsafe_op_in_unsafe_fn
)]

include!(concat!(env!("OUT_DIR"), "/asterisk_sys.rs"));
