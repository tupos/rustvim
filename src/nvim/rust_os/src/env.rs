//! Safe home-directory resolution and caching shared by Rust and native callers.
#![forbid(unsafe_code)]

use std::ffi::CStr;
use std::ffi::CString;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::MutexGuard;

/// Owns the cached Rust path and its NUL-terminated view for native callers.
///
/// Both values are replaced together. Rust readers receive owned path snapshots;
/// native readers borrow the string until the next cache update or clear.
#[derive(Default)]
pub(crate) struct HomeDirectoryCache {
    /// Cached path in the platform's native encoding, absent before initialization.
    path: Option<PathBuf>,
    /// Stable C string corresponding to path, absent whenever path is absent.
    native: Option<CString>,
}

impl HomeDirectoryCache {
    /// Replaces both representations, or clears them when no path is supplied.
    fn update(&mut self, path: Option<PathBuf>) {
        let native = path.as_ref().map(|path| {
            #[cfg(unix)]
            let bytes = path.as_os_str().as_encoded_bytes().to_vec();
            #[cfg(windows)]
            let bytes = path.to_string_lossy().as_bytes().to_vec();

            CString::new(bytes).expect("OS home directories cannot contain NUL")
        });
        self.path = path;
        self.native = native;
    }

    /// Returns an owned path that remains valid after cache updates.
    fn path(&self) -> Option<PathBuf> {
        self.path.clone()
    }

    /// Borrows the native string while the caller holds the cache lock.
    pub(crate) fn native_path(&self) -> Option<&CStr> {
        self.native.as_deref()
    }
}

/// Replaces the former C-owned cache with one shared, synchronized Rust cache.
static HOME_DIRECTORY: Mutex<HomeDirectoryCache> = Mutex::new(HomeDirectoryCache {
    path: None,
    native: None,
});

/// Locks the cache, preserving its contents after a previous panic.
pub(crate) fn home_directory_cache() -> MutexGuard<'static, HomeDirectoryCache> {
    HOME_DIRECTORY
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// Resolves and replaces the cached home at startup and whenever HOME is set.
///
/// Unsetting HOME does not call this function, so the previous value is retained.
/// Native callers must finish using borrowed strings before this cache changes.
pub fn initialize_home_directory() {
    let path = home_directory();
    home_directory_cache().update(path);
}

/// Returns an owned snapshot of the cached home, or None before initialization.
pub fn cached_home_directory() -> Option<PathBuf> {
    home_directory_cache().path()
}

/// Releases the cached home during shutdown.
///
/// Native callers must finish using borrowed strings before the cache is cleared.
pub fn clear_home_directory() {
    home_directory_cache().update(None);
}

/// Resolves the "real" user home directory.
///
/// A nonempty $HOME takes precedence.
///
/// On Windows:
///   1. If $HOME is unavailable, combine HOMEDRIVE and HOMEPATH.
///   2. If those are unavailable, try std::env::home_dir().
///   3. Expand a leading %VARIABLE% reference in the selected value.
///   4. Fall back to C:/ if no home is available.
///
/// On Unix:
///   1. If $HOME is unavailable, try std::env::home_dir().
///   2. If neither provides a home, use the current working directory.
///   3. Canonicalize the selected path when possible, resolving symbolic
///      links and mount paths without changing the working directory.
///      Keep the selected path if canonicalization fails.
///
/// Windows paths are normalized without canonicalization, which could
/// change the meaning of drive-relative paths.
pub fn home_directory() -> Option<PathBuf> {
    resolve_home_from(|name| std::env::var_os(name), std::env::home_dir)
}

/// Resolves Unix HOME, then the system home, then the current directory.
///
/// Existing paths are canonicalized to match native symlink resolution.
#[cfg(unix)]
fn resolve_home_from(
    environment: impl Fn(&str) -> Option<OsString>,
    system_home: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    let home = environment("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(system_home)
        .or_else(|| std::env::current_dir().ok())?;
    Some(home.canonicalize().unwrap_or(home))
}

/// Resolves Windows HOME, drive/path, or the system profile, in that order.
///
/// Expands a leading %VARIABLE% reference and uses C:/ if no home is available.
#[cfg(windows)]
fn resolve_home_from(
    environment: impl Fn(&str) -> Option<OsString>,
    system_home: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::ffi::OsStringExt;

    let mut home = environment("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| {
            let mut drive = environment("HOMEDRIVE").filter(|value| !value.is_empty())?;
            drive.push(
                environment("HOMEPATH")
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "/".into()),
            );
            Some(drive)
        })
        .or_else(|| system_home().map(PathBuf::into_os_string));
    if let Some(value) = home
        .as_deref()
        .and_then(|value| value.to_str())
        .and_then(|value| value.strip_prefix('%'))
    {
        if let Some((name, suffix)) = value.split_once('%') {
            home = environment(name)
                .filter(|value| !value.is_empty())
                .map(|mut value| {
                    value.push(suffix);
                    value
                });
        }
    }
    let home = home.unwrap_or_else(|| "C:/".into());
    let units: Vec<u16> = home
        .encode_wide()
        .map(|unit| {
            if unit == u16::from(b'\\') {
                u16::from(b'/')
            } else {
                unit
            }
        })
        .collect();
    Some(PathBuf::from(OsString::from_wide(&units)))
}

#[cfg(test)]
mod tests {
    //! Checks home precedence and cache ownership without changing process environment.

    use super::*;

    /// Checks replacement and clearing preserve previously returned Rust snapshots.
    #[test]
    fn cache_updates_preserve_owned_snapshots() {
        let mut cache = HomeDirectoryCache::default();
        assert_eq!(cache.path(), None);
        assert_eq!(cache.native_path(), None);

        cache.update(Some("first-home".into()));
        let snapshot = cache.path().unwrap();
        assert_eq!(cache.native_path(), Some(c"first-home"));

        cache.update(Some("second-home".into()));
        assert_eq!(cache.path(), Some(PathBuf::from("second-home")));
        assert_eq!(cache.native_path(), Some(c"second-home"));
        assert_eq!(snapshot, PathBuf::from("first-home"));

        cache.update(None);
        assert_eq!(cache.path(), None);
        assert_eq!(cache.native_path(), None);
        assert_eq!(snapshot, PathBuf::from("first-home"));
    }

    /// Checks Rust reads do not replace the string borrowed by native callers.
    #[test]
    fn cache_reads_preserve_native_storage() {
        let mut cache = HomeDirectoryCache::default();
        cache.update(Some("cached-home".into()));
        let pointer = cache.native_path().unwrap().as_ptr();

        for _ in 0..3 {
            assert_eq!(cache.path(), Some(PathBuf::from("cached-home")));
            assert_eq!(cache.native_path().unwrap().as_ptr(), pointer);
        }
    }

    /// Checks Unix home paths preserve non-UTF-8 bytes in both representations.
    #[cfg(unix)]
    #[test]
    fn cache_preserves_unix_path_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let bytes = b"/home/\xff".to_vec();
        let path = PathBuf::from(OsString::from_vec(bytes.clone()));
        let mut cache = HomeDirectoryCache::default();
        cache.update(Some(path.clone()));

        assert_eq!(cache.path(), Some(path));
        assert_eq!(cache.native_path().unwrap().to_bytes(), bytes);
    }

    /// Checks Windows keeps UTF-16 in Rust and provides a UTF-8 view to C.
    #[cfg(windows)]
    #[test]
    fn cache_preserves_windows_path_units() {
        use std::os::windows::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_wide(&[
            b'C' as u16,
            b':' as u16,
            b'/' as u16,
            0xd800,
        ]));
        let mut cache = HomeDirectoryCache::default();
        cache.update(Some(path.clone()));

        assert_eq!(cache.path(), Some(path.clone()));
        assert_eq!(
            cache.native_path().unwrap().to_bytes(),
            path.to_string_lossy().as_bytes()
        );
    }

    /// Checks nonempty HOME wins over system-home discovery.
    #[test]
    fn prefers_home_to_system_home() {
        let home = resolve_home_from(
            |name| (name == "HOME").then(|| "rustvim-missing-home".into()),
            || panic!("HOME must take precedence"),
        );
        assert_eq!(home, Some(PathBuf::from("rustvim-missing-home")));
    }

    /// Checks empty HOME is ignored in favor of the supplied system home.
    #[test]
    fn empty_home_uses_system_home() {
        let home = resolve_home_from(
            |name| (name == "HOME").then(OsString::new),
            || Some("rustvim-missing-system-home".into()),
        );
        assert_eq!(home, Some(PathBuf::from("rustvim-missing-system-home")));
    }

    /// Checks Unix relative home paths resolve against the current directory.
    #[cfg(unix)]
    #[test]
    fn canonicalizes_unix_home() {
        let home = resolve_home_from(
            |name| (name == "HOME").then(|| ".".into()),
            || panic!("HOME must take precedence"),
        );
        assert_eq!(
            home,
            Some(std::env::current_dir().unwrap().canonicalize().unwrap())
        );
    }

    /// Checks Unix discovery falls back to the current directory.
    #[cfg(unix)]
    #[test]
    fn missing_home_uses_current_directory() {
        assert_eq!(
            resolve_home_from(|_| None, || None),
            Some(std::env::current_dir().unwrap().canonicalize().unwrap())
        );
    }

    /// Checks Windows drive/path, profile references, and the final C:/ fallback.
    #[cfg(windows)]
    #[test]
    fn follows_windows_home_rules() {
        for (values, expected) in [
            (vec![("HOMEDRIVE", "D:"), ("HOMEPATH", r"\user")], "D:/user"),
            (vec![("HOMEDRIVE", "D:")], "D:/"),
            (
                vec![
                    ("HOME", r"%USERPROFILE%\child"),
                    ("USERPROFILE", r"C:\profile"),
                ],
                "C:/profile/child",
            ),
            (vec![("HOME", r"%MISSING%\child")], "C:/"),
            (vec![], "C:/"),
        ] {
            let home = resolve_home_from(
                |name| {
                    values
                        .iter()
                        .find(|(key, _)| *key == name)
                        .map(|(_, value)| OsString::from(value))
                },
                || None,
            );
            assert_eq!(home.unwrap().into_os_string(), OsString::from(expected));
        }
    }
}
