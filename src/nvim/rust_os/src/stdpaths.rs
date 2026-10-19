//! Safe state-directory paths shared by Rust and native callers.
#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

/// Returns the application state directory using Neovim's cached home.
///
/// The caller supplies the cached home so unsetting HOME does not change
/// established defaults. Environment values are read afresh, as in Neovim.
pub fn state_directory(home: Option<&Path>) -> PathBuf {
    state_directory_from(|name| std::env::var_os(name), home)
}

/// Appends a name to the application state directory.
///
/// Adds exactly trailing_separators forward slashes after the name. When
/// escape_commas is true, inserts a backslash before every comma in the path.
pub fn user_state_subpath(
    home: Option<&Path>,
    name: &Path,
    trailing_separators: usize,
    escape_commas: bool,
) -> PathBuf {
    subpath(
        state_directory(home),
        name,
        trailing_separators,
        escape_commas,
    )
}

/// Selects Unix XDG_STATE_HOME or the cached home's .local/state directory.
///
/// An explicitly empty XDG_STATE_HOME selects a relative path. App names are
/// read literally; empty or missing NVIM_APPNAME uses "nvim".
#[cfg(unix)]
fn state_directory_from(
    environment: impl Fn(&str) -> Option<OsString>,
    home: Option<&Path>,
) -> PathBuf {
    let mut path = environment("XDG_STATE_HOME").unwrap_or_else(|| {
        home.unwrap_or(Path::new("~"))
            .join(".local/state")
            .into_os_string()
    });
    let appname = environment("NVIM_APPNAME")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "nvim".into());
    append(&mut path, &appname);
    PathBuf::from(path)
}

/// Selects Windows XDG_STATE_HOME, LOCALAPPDATA, or cached home/AppData/Local.
///
/// Empty variables are ignored. App names receive "-data", and directory
/// separators are normalized without discarding native UTF-16 units.
#[cfg(windows)]
fn state_directory_from(
    environment: impl Fn(&str) -> Option<OsString>,
    home: Option<&Path>,
) -> PathBuf {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::ffi::OsStringExt;

    let mut path = environment("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| environment("LOCALAPPDATA").filter(|value| !value.is_empty()))
        .unwrap_or_else(|| {
            home.unwrap_or(Path::new("~"))
                .join("AppData/Local")
                .into_os_string()
        });
    let mut appname = environment("NVIM_APPNAME")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "nvim".into());
    appname.push("-data");
    append(&mut path, &appname);
    let units: Vec<u16> = path
        .encode_wide()
        .map(|unit| {
            if unit == u16::from(b'\\') {
                u16::from(b'/')
            } else {
                unit
            }
        })
        .collect();
    PathBuf::from(OsString::from_wide(&units))
}

/// Builds a subpath without re-reading environment or home values.
fn subpath(
    directory: PathBuf,
    name: &Path,
    trailing_separators: usize,
    escape_commas: bool,
) -> PathBuf {
    let mut path = directory.into_os_string();
    append(&mut path, name.as_os_str());
    if escape_commas {
        path = escaped_commas(path);
    }
    for _ in 0..trailing_separators {
        path.push("/");
    }
    PathBuf::from(path)
}

/// Appends Unix path text without replacing the base for leading separators.
#[cfg(unix)]
fn append(base: &mut OsString, name: &OsStr) {
    if !base.is_empty() && !base.as_encoded_bytes().ends_with(b"/") {
        base.push("/");
    }
    base.push(name);
}

/// Appends Windows path text, recognizing slash, backslash, and drive prefixes.
#[cfg(windows)]
fn append(base: &mut OsString, name: &OsStr) {
    let bytes = base.as_encoded_bytes();
    if !bytes.is_empty()
        && !bytes.ends_with(b"/")
        && !bytes.ends_with(b"\\")
        && !bytes.ends_with(b":")
    {
        base.push("/");
    }
    base.push(name);
}

/// Escapes commas while preserving arbitrary Unix filename bytes.
#[cfg(unix)]
fn escaped_commas(path: OsString) -> OsString {
    use std::os::unix::ffi::OsStringExt;

    let mut escaped = Vec::new();
    for byte in path.into_vec() {
        if byte == b',' {
            escaped.push(b'\\');
        }
        escaped.push(byte);
    }
    OsString::from_vec(escaped)
}

/// Escapes commas while preserving arbitrary Windows UTF-16 units.
#[cfg(windows)]
fn escaped_commas(path: OsString) -> OsString {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::ffi::OsStringExt;

    let mut escaped = Vec::new();
    for unit in path.encode_wide() {
        if unit == u16::from(b',') {
            escaped.push(u16::from(b'\\'));
        }
        escaped.push(unit);
    }
    OsString::from_wide(&escaped)
}

#[cfg(test)]
mod tests {
    //! Checks cached home, environment rules, encoding, and native subpath options.

    use super::*;

    /// Builds a directory from supplied environment values and a fixed cached home.
    fn directory(values: &[(&str, &str)]) -> PathBuf {
        state_directory_from(
            |name| {
                values
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            },
            Some(Path::new("/cached-home")),
        )
    }

    /// Checks configured state directories, nested app names, and separators.
    #[test]
    fn uses_state_and_app_name() {
        for (state, appname) in [
            ("/state", ""),
            ("relative", "editor/profile"),
            ("/state/", "editor"),
            ("/state//", "editor"),
            ("/état", "東京"),
        ] {
            let name = if appname.is_empty() { "nvim" } else { appname };
            let suffix = if cfg!(windows) { "-data" } else { "" };
            let separator = if state.ends_with('/') { "" } else { "/" };
            assert_eq!(
                directory(&[("XDG_STATE_HOME", state), ("NVIM_APPNAME", appname)]).into_os_string(),
                OsString::from(format!("{state}{separator}{name}{suffix}"))
            );
        }
    }

    /// Checks HOME changes do not replace the home supplied by the native cache.
    #[test]
    fn uses_cached_home() {
        let suffix = if cfg!(windows) {
            "AppData/Local/nvim-data"
        } else {
            ".local/state/nvim"
        };
        for values in [&[][..], &[("HOME", "/new-home")][..], &[("HOME", "")][..]] {
            assert_eq!(
                directory(values).into_os_string(),
                OsString::from(format!("/cached-home/{suffix}"))
            );
        }
    }

    /// Checks unset native home retains the literal tilde fallback.
    #[test]
    fn missing_cached_home_keeps_tilde() {
        let suffix = if cfg!(windows) {
            "AppData/Local/nvim-data"
        } else {
            ".local/state/nvim"
        };
        assert_eq!(
            state_directory_from(|_| None, None).into_os_string(),
            OsString::from(format!("~/{suffix}"))
        );
    }

    /// Checks state values containing variable references are used literally.
    #[test]
    fn does_not_expand_state_values() {
        let name = if cfg!(windows) { "nvim-data" } else { "nvim" };
        assert_eq!(
            directory(&[("XDG_STATE_HOME", "~/$STATE")]).into_os_string(),
            OsString::from(format!("~/$STATE/{name}"))
        );
    }

    /// Checks comma escaping and exact trailing separators used by native options.
    #[test]
    fn formats_subpath_options() {
        for (escape, expected) in [
            (false, "/state,one/editor/backup//"),
            (true, "/state\\,one/editor/backup//"),
        ] {
            assert_eq!(
                subpath("/state,one/editor".into(), Path::new("backup"), 2, escape)
                    .into_os_string(),
                OsString::from(expected)
            );
        }
        assert_eq!(
            subpath("/state/editor".into(), Path::new("logs/nvim.log"), 0, true).into_os_string(),
            OsString::from("/state/editor/logs/nvim.log")
        );
    }

    /// Checks an explicitly empty Unix state variable selects a relative path.
    #[cfg(unix)]
    #[test]
    fn empty_unix_state_is_relative() {
        assert_eq!(
            directory(&[("XDG_STATE_HOME", "")]).into_os_string(),
            OsString::from("nvim")
        );
    }

    /// Checks native Unix filename bytes survive path building and comma escaping.
    #[cfg(unix)]
    #[test]
    fn preserves_unix_path_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let directory = state_directory_from(
            |name| {
                (name == "XDG_STATE_HOME").then(|| OsString::from_vec(b"state-\xff,one".to_vec()))
            },
            None,
        );
        assert_eq!(
            subpath(directory, Path::new("log"), 0, true)
                .into_os_string()
                .into_vec(),
            b"state-\xff\\,one/nvim/log"
        );
    }

    /// Checks Windows uses LOCALAPPDATA when XDG_STATE_HOME is empty or absent.
    #[cfg(windows)]
    #[test]
    fn windows_uses_local_appdata() {
        for values in [
            vec![("LOCALAPPDATA", r"C:\state")],
            vec![("XDG_STATE_HOME", ""), ("LOCALAPPDATA", r"C:\state")],
        ] {
            assert_eq!(
                directory(&values).into_os_string(),
                OsString::from("C:/state/nvim-data")
            );
        }
    }

    /// Checks Windows path transformations preserve unpaired UTF-16 surrogates.
    #[cfg(windows)]
    #[test]
    fn preserves_windows_path_units() {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::ffi::OsStringExt;

        let directory = state_directory_from(
            |name| {
                (name == "XDG_STATE_HOME").then(|| OsString::from_wide(&[67, 58, 92, 0xd800, 44]))
            },
            None,
        );
        let path = subpath(directory, Path::new("log"), 0, true);
        let mut expected = vec![67, 58, 47, 0xd800, 92, 44];
        expected.extend("/nvim-data/log".encode_utf16());
        assert_eq!(path.as_os_str().encode_wide().collect::<Vec<_>>(), expected);
    }
}
