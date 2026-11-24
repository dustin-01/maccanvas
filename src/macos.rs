//! AppKit, Security and LocalAuthentication bridge for the macOS viewer.
use std::ffi::{c_char, c_int};

unsafe extern "C" {
    pub(crate) fn mc_dark() -> bool;
    pub(crate) fn mc_motion() -> bool;
    pub(crate) fn mc_glass() -> bool;
    pub(crate) fn mc_foreground() -> bool;
    fn mc_pointer_y() -> f32;
    pub(crate) fn mc_clip_sequence() -> u32;
    pub(crate) fn mc_clip_read(dst: *mut u8, limit: usize, sequence: *mut u32) -> c_int;
    pub(crate) fn mc_clip_write(
        src: *const u8,
        size: usize,
        expected: i64,
        sequence: *mut u32,
    ) -> c_int;
    pub(crate) fn mc_file_dialog(name: *const c_char, dst: *mut c_char, limit: usize) -> c_int;
    pub(crate) fn mc_vault_load(
        account: *const c_char,
        dst: *mut u8,
        limit: usize,
        size: *mut usize,
    ) -> i32;
    pub(crate) fn mc_vault_save(account: *const c_char, src: *const u8, size: usize) -> i32;
    pub(crate) fn mc_vault_delete(account: *const c_char) -> i32;
    pub(crate) fn mc_consent() -> bool;
}

/// Vertical cursor position in the key window's content coordinates.
pub fn pointer_y() -> f32 {
    unsafe { mc_pointer_y() }
}
