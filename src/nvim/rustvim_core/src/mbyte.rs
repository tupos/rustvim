// vim: sw=4

use std::ffi::CStr;

use crate::grid::VimSCharT;

#[link(name = "nvim")]
#[link(name = "utf8proc")]
unsafe extern "C" {
    #[link_name = "utf_ptr2cells"]
    fn utf_ptr2cells_ffi(p_in: *const libc::c_char) -> libc::c_int;

    #[link_name = "utf_ptr2char"]
    fn utf_ptr2char_ffi(p_in: *const libc::c_char) -> libc::c_int;

    #[link_name = "utf_iscomposing_first"]
    fn utf_iscomposing_first_ffi(c: libc::c_int) -> bool;

    #[link_name = "utf_ptr2len"]
    fn utf_ptr2len_ffi(p_in: *const libc::c_char) -> libc::c_int;

    #[link_name = "utf_char2bytes"]
    fn utf_char2bytes_ffi(c: libc::c_int, buf: *mut libc::c_char) -> libc::c_int;

    #[link_name = "mb_string2cells_len"]
    fn mb_string2cells_len_ffi(text: *const libc::c_char, len: libc::size_t) -> libc::size_t;

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

/// Returns the first codepoint in a bounded byte slice.
///
/// The local trailing NUL gives Neovim's extended UTF-8 decoder enough
/// readable input for every supported sequence without requiring the source
/// slice itself to be NUL-terminated.
pub(crate) fn first_codepoint(text: &[u8]) -> libc::c_int {
    if text.is_empty() {
        return 0;
    }

    const MAX_UTF8_BYTES: usize = 6;
    let mut terminated = [0; MAX_UTF8_BYTES + 1];
    let copy_len = text.len().min(MAX_UTF8_BYTES);
    terminated[..copy_len].copy_from_slice(&text[..copy_len]);
    let input =
        CStr::from_bytes_until_nul(&terminated).expect("local UTF-8 input is NUL-terminated");
    utf_ptr2char(input)
}

/// Returns whether a codepoint needs a leading space in a screen cell.
pub(crate) fn is_composing_first(codepoint: libc::c_int) -> bool {
    // SAFETY: The C function is pure and accepts every integer value.
    unsafe { utf_iscomposing_first_ffi(codepoint) }
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

/// Returns the display width of a byte string using Neovim's multibyte rules.
pub(crate) fn string_cells(text: &[u8]) -> usize {
    // SAFETY: `text` supplies a valid pointer and length for the duration of
    // the call. The length-delimited C function does not require a trailing
    // NUL.
    unsafe { mb_string2cells_len_ffi(text.as_ptr().cast::<libc::c_char>(), text.len()) }
}

/// Returns the byte length of the first base character and its composing
/// characters using Neovim's multibyte rules.
pub(crate) fn first_cluster_len(text: &[u8]) -> usize {
    if text.is_empty() {
        return 0;
    }
    let len = libc::c_int::try_from(text.len()).unwrap_or(libc::c_int::MAX);
    // SAFETY: `text` is nonempty and valid for reads of `len` bytes. The
    // length-delimited C function cannot inspect beyond that range.
    unsafe { utfc_ptr2len_len(text.as_ptr().cast::<libc::c_char>(), len) as usize }
}

/// Returns the display width of the first character in `text`.
pub(crate) fn first_char_cells(text: &[u8]) -> libc::c_int {
    if text.is_empty() {
        return 0;
    }
    let len = libc::c_int::try_from(text.len()).unwrap_or(libc::c_int::MAX);
    // SAFETY: `text` is nonempty and valid for reads of `len` bytes. The
    // length-delimited C function cannot inspect beyond that range.
    unsafe { utf_ptr2cells_len(text.as_ptr().cast::<libc::c_char>(), len) }
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
