//! C boundary for Rust build diagnostics.

use std::ffi::c_char;

/// Returns NUL-terminated Rust build information, valid for the process lifetime.
#[unsafe(no_mangle)]
pub extern "C" fn rustvim_rust_build_info() -> *const c_char {
    concat!(
        "Rust compiler: ",
        env!("RUSTVIM_RUSTC_VERSION"),
        "\nCargo: ",
        env!("RUSTVIM_CARGO_VERSION"),
        "\nRust target: ",
        env!("RUSTVIM_TARGET"),
        "\nRust profile: ",
        env!("RUSTVIM_PROFILE"),
        "\nRust build: opt-level=",
        env!("RUSTVIM_OPT_LEVEL"),
        ", debuginfo=",
        env!("RUSTVIM_DEBUG"),
        "\nRust target features: ",
        env!("RUSTVIM_TARGET_FEATURES"),
        "\0",
    )
    .as_ptr()
    .cast()
}
