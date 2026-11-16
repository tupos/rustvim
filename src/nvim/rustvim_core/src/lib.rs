#![allow(dead_code)]
#![allow(static_mut_refs)]

// Unit tests link libnvim directly and need its logging and OS entry points.
// Linking rustvim_tracing also includes its rust_os dependency.
#[cfg(test)]
extern crate rustvim_tracing as _;

// Include Wasmtime's C API in the same Rust link as the editor and its tests.
#[cfg(feature = "wasmtime")]
extern crate wasmtime_c_api as _;

mod context;
pub mod defines;
pub mod eval;
pub mod generated;
pub mod globals;
pub mod grid;
pub mod mbyte;
pub mod option;
pub mod pos;
pub mod ui;
mod ui_compositor;
