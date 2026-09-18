// vim: sw=4

use crate::grid::VimSCharT;
use std::ffi::CStr;

#[link(name = "nvim")]
#[link(name = "utf8proc")]
unsafe extern "C" {
    #[link_name = "utf_ptr2cells"]
    fn utf_ptr2cells_ffi(p_in: *const libc::c_char) -> libc::c_int;

    #[link_name = "utf_ptr2char"]
    fn utf_ptr2char_ffi(p_in: *const libc::c_char) -> libc::c_int;

    pub fn utfc_ptr2len_len(p: *const libc::c_char, size: libc::c_int) -> libc::c_int;
    pub fn utf_ptr2cells_len(p: *const libc::c_char, size: libc::c_int) -> libc::c_int;
    pub fn utfc_ptrlen2schar(
        p: *const libc::c_char,
        len: libc::c_int,
        firstc: *mut libc::c_int,
    ) -> VimSCharT;
    pub fn utf_char2bytes(c: libc::c_int, buf: *mut libc::c_char) -> libc::c_int;
}

pub fn utf_ptr2cells(p_in: &CStr) -> libc::c_int {
    // SAFETY: CStr guarantees a non-null pointer to a readable,
    // NUL-terminated byte sequence for the duration of this call.
    unsafe { utf_ptr2cells_ffi(p_in.as_ptr()) }
}

pub fn utf_ptr2char(p_in: &CStr) -> libc::c_int {
    // SAFETY: CStr guarantees a non-null pointer to a readable,
    // NUL-terminated byte sequence for the duration of this call.
    unsafe { utf_ptr2char_ffi(p_in.as_ptr()) }
}
