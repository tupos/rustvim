// vim: sw=4

use crate::grid::VimSCharT;

#[link(name = "nvim")]
#[link(name = "utf8proc")]
unsafe extern "C" {
    pub fn utf_ptr2cells(p_in: *const libc::c_char) -> libc::c_int;
    pub fn utfc_ptr2len_len(p: *const libc::c_char, size: libc::c_int) -> libc::c_int;
    pub fn utf_ptr2cells_len(p: *const libc::c_char, size: libc::c_int) -> libc::c_int;
    pub fn utfc_ptrlen2schar(
        p: *const libc::c_char,
        len: libc::c_int,
        firstc: *mut libc::c_int,
    ) -> VimSCharT;
    pub fn utf_char2bytes(c: libc::c_int, buf: *const libc::c_char) -> libc::c_int;
}
