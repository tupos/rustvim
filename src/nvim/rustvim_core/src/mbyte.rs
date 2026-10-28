// vim: sw=4

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

    #[link_name = "mb_string2cells_len"]
    fn mb_string2cells_len_ffi(text: *const libc::c_char, len: libc::size_t) -> libc::size_t;

    /// Copies primitive results from the editor's bounded cluster decoder.
    fn rustvim_cluster_info(
        text: *const libc::c_char,
        size: libc::c_int,
        value: *mut libc::c_int,
        cells: *mut libc::c_int,
        first_compose: *mut bool,
    ) -> usize;
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

/// Decoded source metadata shared by grid, column, and virtual-text rendering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TextCluster {
    /// First decoded value; negative identifies a malformed byte.
    pub(crate) value: i32,
    /// Source bytes consumed by this complete bounded cluster.
    pub(crate) len: usize,
    /// Display cells occupied under the current editor options.
    pub(crate) cells: usize,
    /// A leading space is needed to display this cluster independently.
    pub(crate) first_compose: bool,
}

/// Decodes the first cluster, stopping at empty input or a leading NUL.
pub(crate) fn first_cluster(text: &[u8]) -> Option<TextCluster> {
    if text.first().is_none_or(|&byte| byte == 0) {
        return None;
    }
    let size = text.len().min(libc::c_int::MAX as usize) as libc::c_int;
    let mut value = 0;
    let mut cells = 0;
    let mut first_compose = false;
    // SAFETY: The bounded decoder reads only size bytes and writes these local outputs.
    let len = unsafe {
        rustvim_cluster_info(
            text.as_ptr().cast(),
            size,
            &mut value,
            &mut cells,
            &mut first_compose,
        )
    };
    assert!(len > 0 && len <= size as usize);
    assert!(cells > 0);
    Some(TextCluster {
        value,
        len,
        cells: cells as usize,
        first_compose,
    })
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

#[cfg(all(test, unix))]
mod tests {
    use super::first_cluster;

    /// A combining mark at the slice boundary must not trigger a lookahead read.
    #[test]
    fn bounded_cluster_stops_before_an_inaccessible_page() {
        // SAFETY: sysconf reads the process's fixed page size.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        assert!(page_size > 0);
        let page_size = page_size as usize;
        // SAFETY: Creates two private pages without referring to existing memory.
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page_size * 2,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(mapping, libc::MAP_FAILED);
        // SAFETY: The second page belongs exclusively to this mapping.
        let protected =
            unsafe { libc::mprotect(mapping.add(page_size), page_size, libc::PROT_NONE) };
        if protected != 0 {
            // SAFETY: Releases the complete mapping before reporting the failure.
            unsafe {
                libc::munmap(mapping, page_size * 2);
            }
            panic!("failed to protect the lookahead page");
        }
        // SAFETY: These three bytes are writable and end immediately before the protected page.
        let text =
            unsafe { std::slice::from_raw_parts_mut(mapping.cast::<u8>().add(page_size - 3), 3) };
        text.copy_from_slice(b"e\xcc\x81");
        let cluster = first_cluster(text).unwrap();
        // SAFETY: The borrowed bytes are no longer used; release both mapped pages.
        assert_eq!(unsafe { libc::munmap(mapping, page_size * 2) }, 0);
        assert_eq!(
            (cluster.value, cluster.len, cluster.cells),
            (i32::from(b'e'), 3, 1)
        );
    }
}
