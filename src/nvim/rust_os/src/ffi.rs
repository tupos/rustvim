//! Native ownership adapters for the shared safe OS implementation.

use std::borrow::Cow;
use std::ffi::CStr;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::ptr;

unsafe extern "C" {
    /// Allocates native storage or terminates on allocation failure.
    fn xmalloc(size: usize) -> *mut libc::c_void;
}

/// Borrows a native path without taking ownership of its storage.
///
/// # Safety
///
/// A non-null pointer must remain a readable NUL-terminated string for 'a.
unsafe fn native_path<'a>(pointer: *const libc::c_char) -> Option<Cow<'a, Path>> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: The caller supplies a readable string and its lifetime.
    let value = unsafe { CStr::from_ptr(pointer) };
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        Some(Cow::Borrowed(Path::new(OsStr::from_bytes(
            value.to_bytes(),
        ))))
    }
    #[cfg(windows)]
    {
        Some(Cow::Owned(PathBuf::from(
            value.to_string_lossy().into_owned(),
        )))
    }
}

/// Transfers a path to native-owned storage that callers release with xfree.
fn native_owned_path(path: &Path) -> *mut libc::c_char {
    #[cfg(unix)]
    let bytes = path.as_os_str().as_encoded_bytes();
    #[cfg(windows)]
    let text = path.to_string_lossy();
    #[cfg(windows)]
    let bytes = text.as_bytes();

    // SAFETY: xmalloc returns writable, non-null storage of the requested size.
    let output = unsafe { xmalloc(bytes.len() + 1) }.cast::<u8>();
    // SAFETY: The allocation has room for the bytes and the NUL; it cannot alias them.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), output, bytes.len());
        output.add(bytes.len()).write(0);
    }
    output.cast()
}

/// Borrows the Rust-owned native string without allocating or reporting errors.
///
/// # Safety
///
/// The caller must not modify or free the returned string. It must exclude cache
/// initialization and clearing for the entire time it uses a non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustvim_cached_homedir() -> *const libc::c_char {
    crate::env::home_directory_cache()
        .native_path()
        .map_or(ptr::null(), CStr::as_ptr)
}

/// Refreshes the shared home cache for native startup and HOME assignments.
///
/// # Safety
///
/// No native caller may still be using a string borrowed from the previous cache.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn init_homedir() {
    crate::env::initialize_home_directory();
}

/// Releases the Rust-owned home cache during native shutdown.
///
/// # Safety
///
/// No native caller may still be using a string borrowed from the cache.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn free_homedir() {
    crate::env::clear_home_directory();
}

/// Returns the shared application state directory in native-owned storage.
#[unsafe(no_mangle)]
pub extern "C" fn rustvim_state_directory() -> *mut libc::c_char {
    let home = crate::env::cached_home_directory();
    native_owned_path(&crate::stdpaths::state_directory(home.as_deref()))
}

/// Preserves the existing native state-subpath ABI using the shared Rust implementation.
///
/// # Safety
///
/// A non-null name must remain a readable NUL-terminated string until this call
/// returns. A null name is rejected with a null result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stdpaths_user_state_subpath(
    name: *const libc::c_char,
    trailing_separators: usize,
    escape_commas: bool,
) -> *mut libc::c_char {
    // SAFETY: The caller supplies a readable name that remains valid for this call.
    let Some(name) = (unsafe { native_path(name) }) else {
        return ptr::null_mut();
    };
    let home = crate::env::cached_home_directory();
    native_owned_path(&crate::stdpaths::user_state_subpath(
        home.as_deref(),
        name.as_ref(),
        trailing_separators,
        escape_commas,
    ))
}
