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

    #[link_name = "utf_ptr2len"]
    fn utf_ptr2len_ffi(p_in: *const libc::c_char) -> libc::c_int;

    #[link_name = "utf_char2bytes"]
    fn utf_char2bytes_ffi(c: libc::c_int, buf: *mut libc::c_char) -> libc::c_int;

    pub fn utfc_ptr2len_len(p: *const libc::c_char, size: libc::c_int) -> libc::c_int;
    pub fn utf_ptr2cells_len(p: *const libc::c_char, size: libc::c_int) -> libc::c_int;
    pub fn utfc_ptrlen2schar(
        p: *const libc::c_char,
        len: libc::c_int,
        firstc: *mut libc::c_int,
    ) -> VimSCharT;
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

/// Returns the byte length of the first codepoint using Neovim's UTF-8 rules.
///
/// Invalid leading bytes have length one. Neovim's legacy five- and six-byte
/// encodings are accepted to preserve compatibility with the C implementation.
pub(crate) fn utf_ptr2len(p_in: &CStr) -> usize {
    // SAFETY: CStr guarantees a non-null pointer to a readable,
    // NUL-terminated byte sequence for the duration of this call.
    unsafe { utf_ptr2len_ffi(p_in.as_ptr()) as usize }
}

/// A codepoint encoded with Neovim's extended UTF-8 representation.
///
/// The fixed-size storage also accommodates the legacy five- and six-byte
/// sequences supported by `utf_char2bytes`.
pub(crate) struct EncodedCodepoint {
    bytes: [u8; 6],
    len: usize,
}

impl EncodedCodepoint {
    /// Encodes `codepoint` with the same semantics as `utf_char2bytes`.
    pub(crate) fn new(codepoint: libc::c_int) -> Self {
        let mut bytes = [0; 6];
        // SAFETY: `bytes` provides the six writable bytes required for any
        // value accepted by Neovim's extended UTF-8 encoder.
        let len = unsafe {
            utf_char2bytes_ffi(codepoint, bytes.as_mut_ptr().cast::<libc::c_char>()) as usize
        };
        Self { bytes, len }
    }

    /// Returns only the initialized encoded bytes.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}
