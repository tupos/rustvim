//! Safe home-directory resolution shared with Neovim's native initialization.
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::path::PathBuf;

/// Resolves the home directory without creating another process-wide cache.
///
/// Neovim retains the result in its existing native cache until HOME is set
/// again. Unsetting HOME therefore does not discard the previously cached home.
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
    //! Checks home precedence without changing process environment variables.

    use super::*;

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
