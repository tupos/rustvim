pub type Integer = i64;
pub type OptInt = i64;

#[derive(Debug, Clone, Copy)]
pub enum TriState {
    None = -1,
    False = 0,
    True = 1,
}

#[derive(Debug, Clone, Copy)]
pub struct VimString {
    data: *mut i32,
    size: usize,
}
