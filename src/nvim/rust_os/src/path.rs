//! Safe path-text operations shared by Rust callers.
#![forbid(unsafe_code)]

/// Borrows the text after the last platform path separator without normalizing it.
///
/// A trailing separator produces an empty tail; "." and ".." remain literal.
/// Windows accepts both slash forms and skips an initial drive prefix and
/// leading slash separators, matching native path_tail().
pub fn tail(path: &str) -> &str {
    #[cfg(windows)]
    let path = {
        let bytes = path.as_bytes();
        let path = if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            &path[2..]
        } else {
            path
        };
        path.trim_start_matches(['/', '\\'])
    };

    path.rsplit(std::path::is_separator)
        .next()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    //! Checks compatibility with native path-tail semantics.

    use super::tail;

    /// Checks final components, trailing separators, Unicode, and literal dot names.
    #[test]
    fn preserves_native_tail_rules() {
        for (path, expected) in [
            ("", ""),
            ("server", "server"),
            ("/tmp/nvim.sock", "nvim.sock"),
            ("dir///server", "server"),
            ("dir/", ""),
            ("/", ""),
            ("////", ""),
            ("dir/.", "."),
            ("dir/..", ".."),
            ("dir/héllo東京.sock", "héllo東京.sock"),
        ] {
            assert_eq!(tail(path), expected, "{path:?}");
        }
    }

    /// Checks Unix backslashes and colons remain ordinary filename characters.
    #[cfg(unix)]
    #[test]
    fn preserves_unix_backslashes_and_colons() {
        for path in [r"dir\server", "C:server", ":server", "server:stream"] {
            assert_eq!(tail(path), path);
        }
    }

    /// Checks Windows slash forms, drive prefixes, leading colons, and pipe paths.
    #[cfg(windows)]
    #[test]
    fn follows_windows_tail_rules() {
        for (path, expected) in [
            (r"C:\dir\server", "server"),
            (r"dir\", ""),
            ("C:server", "server"),
            ("C:", ""),
            (":server", ":server"),
            ("/:server", ":server"),
            ("C::server", ":server"),
            ("dir/server:stream", "server:stream"),
            (r"\\.\pipe\nvim", "nvim"),
        ] {
            assert_eq!(tail(path), expected, "{path:?}");
        }
    }
}
