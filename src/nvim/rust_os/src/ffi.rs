//! Native ownership adapters for the shared safe OS implementation.

use std::borrow::Cow;
use std::ffi::CStr;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;

unsafe extern "C" {
    /// Borrows Neovim's existing home cache without changing its lifetime.
    fn os_homedir() -> *const libc::c_char;
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

/// Copies the existing native home cache into an owned Rust path.
///
/// # Safety
///
/// Home initialization must have completed. No native home-cache update may
/// run concurrently with this synchronous copy.
pub unsafe fn cached_home_directory() -> Option<PathBuf> {
    // SAFETY: The caller guarantees the cache remains stable during this copy.
    unsafe { native_path(os_homedir()) }.map(Cow::into_owned)
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

/// Resolves home for the native cache; the caller releases the result with xfree.
#[unsafe(no_mangle)]
pub extern "C" fn rustvim_resolve_homedir() -> *mut libc::c_char {
    crate::env::home_directory().map_or(ptr::null_mut(), |home| native_owned_path(&home))
}

/// Returns the shared application state directory in native-owned storage.
///
/// # Safety
///
/// Native home initialization must have completed, and no cache update may run
/// concurrently with this synchronous copy.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustvim_state_directory() -> *mut libc::c_char {
    // SAFETY: Native callers serialize home-cache changes with standard-path queries.
    let home = unsafe { cached_home_directory() };
    native_owned_path(&crate::stdpaths::state_directory(home.as_deref()))
}

/// Preserves the existing native state-subpath ABI using the shared Rust implementation.
///
/// # Safety
///
/// name must be a readable NUL-terminated string. Native home initialization must
/// have completed, and its cache must remain stable until this call returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stdpaths_user_state_subpath(
    name: *const libc::c_char,
    trailing_separators: usize,
    escape_commas: bool,
) -> *mut libc::c_char {
    // SAFETY: Both strings remain valid under the native caller's contract.
    let Some(name) = (unsafe { native_path(name) }) else {
        return ptr::null_mut();
    };
    // SAFETY: The native caller excludes home-cache updates during this call.
    let home = unsafe { cached_home_directory() };
    native_owned_path(&crate::stdpaths::user_state_subpath(
        home.as_deref(),
        name.as_ref(),
        trailing_separators,
        escape_commas,
    ))
}
