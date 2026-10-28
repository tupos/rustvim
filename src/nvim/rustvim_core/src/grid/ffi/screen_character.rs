//! C boundary adapters for screen-character operations.

use super::super::GlyphMapping;
use super::super::MAX_SCHAR_SIZE;
use super::super::NULL_TERMINATOR;
use super::super::TextGlyph;
use super::super::VimSCharT;
use super::super::check_chars_options;
use super::super::decor_check_invalid_glyphs;
use super::super::lock_glyph_mapping;
use super::super::schar_cells_impl;
use super::super::schar_from_bytes;
use super::super::schar_get_first_codepoint_impl;
use super::super::schar_len_impl;
use super::super::schar_to_ascii;
use crate::mbyte;

/// Constructs a screen character from an already decoded cluster.
///
/// # Safety
/// buf must reference len readable bytes of a valid cluster for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn schar_from_cluster_impl(
    buf: *const libc::c_char,
    len: usize,
    first_compose: bool,
) -> VimSCharT {
    // SAFETY: The caller retains this readable cluster; an empty cluster needs no pointer.
    let text = if len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(buf.cast(), len) }
    };
    TextGlyph::screen_character(text, first_compose, &mut lock_glyph_mapping())
}

/// Exposes cluster extraction to the existing native compatibility tests.
///
/// # Safety
/// buf must be a readable NUL-terminated string; firstc must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unittest_schar_from_cluster(
    buf: *const libc::c_char,
    firstc: *mut libc::c_int,
) -> VimSCharT {
    // SAFETY: The caller retains the terminated input and exclusive output storage.
    unsafe {
        let text = std::ffi::CStr::from_ptr(buf).to_bytes();
        let Some(cluster) = mbyte::first_cluster(text) else {
            *firstc = 0;
            return 0;
        };
        *firstc = cluster.value;
        if cluster.value < 0 {
            return 0;
        }
        TextGlyph::screen_character(
            &text[..cluster.len],
            cluster.first_compose,
            &mut lock_glyph_mapping(),
        )
    }
}

/// @param buf need not be NUL terminated, but may not contain embedded NULs.
///
/// caller must ensure len < MAX_SCHAR_SIZE (not =, as NUL needs a byte)
#[unsafe(no_mangle)]
pub extern "C" fn schar_from_buf(buf: *const libc::c_char, len: libc::size_t) -> VimSCharT {
    assert!(len < MAX_SCHAR_SIZE);
    let buf = buf.cast::<u8>();
    let s = if buf.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(buf, len) }
    };
    let mut glyph_mapping = lock_glyph_mapping();
    schar_from_bytes(s, &mut glyph_mapping)
}

/// # Safety
///
/// `buf` must be null or point to a valid NUL-terminated string that remains
/// readable for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn schar_from_str(buf: *const libc::c_char) -> VimSCharT {
    // note: critical max is really (1<<24)-1. This gives us some marginal
    // until next time update_screen() is called
    if buf.is_null() {
        return 0;
    }
    let len = unsafe { libc::strlen(buf) };
    schar_from_buf(buf, len)
}

/// Check if cache is full, and if it is, clear it.
///
/// This should normally only be called in update_screen()
///
/// @return true if cache was clered, and all your screen buffers now are hosed
/// and you need to use UPD_CLEAR
#[unsafe(no_mangle)]
pub extern "C" fn schar_cache_clear_if_full() -> bool {
    let is_full = lock_glyph_mapping().is_full();
    if is_full {
        schar_cache_clear();
        return true;
    }
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_cache_clear() {
    unsafe { decor_check_invalid_glyphs() };
    lock_glyph_mapping().clear();
    if unsafe { check_chars_options() } {
        panic!("Erroc checking chars options");
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_high(sc: VimSCharT) -> bool {
    GlyphMapping::is_glyph_mapping_schart(sc)
}

/// advance buf_out. do NOT set final NUL
///
/// # Safety
///
/// `buf_out` must point to a valid writable pointer. That pointer must
/// reference writable storage for the bytes represented by `sc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn schar_get_adv(
    buf_out: *mut *mut libc::c_char,
    sc: VimSCharT,
) -> libc::size_t {
    let glyph_mapping = lock_glyph_mapping();
    let glyph = glyph_mapping.resolve(sc);
    let bytes = glyph.as_bytes();
    let len = bytes.len();
    let slice = unsafe { std::slice::from_raw_parts_mut((*buf_out).cast::<u8>(), len) };
    slice.copy_from_slice(bytes);
    unsafe {
        *buf_out = (*buf_out).add(len);
    }
    len
}

/// sets final NUL
///
/// # Safety
///
/// `buf_out` must point to writable storage for the bytes represented by `sc`
/// and a trailing NUL byte.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn schar_get(mut buf_out: *mut libc::c_char, sc: VimSCharT) -> libc::size_t {
    unsafe {
        let len = schar_get_adv(&raw mut buf_out, sc);
        *buf_out = NULL_TERMINATOR as libc::c_char;
        len
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_cells(sc: VimSCharT) -> libc::c_int {
    let glyph_mapping = lock_glyph_mapping();
    schar_cells_impl(sc, &glyph_mapping)
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_get_first_codepoint(sc: VimSCharT) -> libc::c_int {
    let glyph_mapping = lock_glyph_mapping();
    schar_get_first_codepoint_impl(sc, &glyph_mapping)
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_len(sc: VimSCharT) -> libc::size_t {
    let glyph_mapping = lock_glyph_mapping();
    schar_len_impl(sc, &glyph_mapping)
}

/// @return ascii char or NULL_TERMINATOR if not ascii
#[unsafe(no_mangle)]
pub extern "C" fn schar_get_ascii(sc: VimSCharT) -> libc::c_char {
    schar_to_ascii(sc).unwrap_or(NULL_TERMINATOR) as libc::c_char
}

/// Exposes screen-character construction to C.
#[unsafe(no_mangle)]
pub extern "C" fn schar_from_char(c: libc::c_int) -> VimSCharT {
    super::super::schar_from_char(c)
}
