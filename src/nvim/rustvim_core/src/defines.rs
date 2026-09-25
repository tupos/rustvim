pub type Integer = i64;
pub type OptInt = i64;

#[derive(Debug, Clone, Copy)]
pub enum TriState {
    None = -1,
    False = 0,
    True = 1,
}

/// C ABI representation of Neovim's `String`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CVimString {
    pub(crate) data: *mut libc::c_char,
    pub(crate) size: libc::size_t,
}
