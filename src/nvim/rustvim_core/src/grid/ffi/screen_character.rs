//! C boundary adapters for screen-character operations.

use super::super::{
    ArabicShaper, GlyphMapping, MAX_SCHAR_SIZE, NULL_TERMINATOR, VimSCharT, check_chars_options,
    decor_check_invalid_glyphs, lock_glyph_mapping, schar_cells_impl, schar_from_bytes,
    schar_get_first_codepoint_impl, schar_in_arabic_block_impl, schar_len_impl, schar_to_ascii,
};

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

#[unsafe(no_mangle)]
pub extern "C" fn schar_in_arabic_block(sc: VimSCharT) -> bool {
    let glyph_mapping = lock_glyph_mapping();
    schar_in_arabic_block_impl(sc, &glyph_mapping)
}

/// Shape `cols` screen characters for Arabic display.
///
/// # Safety
///
/// For a positive `cols`, `buf` must point to `cols` initialized screen
/// characters that are exclusively writable for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn line_do_arabic_shape(buf: *mut VimSCharT, cols: libc::c_int) {
    if buf.is_null() || cols <= 0 {
        return;
    }

    // SAFETY: The caller guarantees that `buf` references `cols` initialized,
    // exclusively writable elements. `cols` was checked to be positive.
    let buf = unsafe { std::slice::from_raw_parts_mut(buf, cols as usize) };
    let mut glyph_mapping = lock_glyph_mapping();
    ArabicShaper::new(&mut glyph_mapping).shape(buf);
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
