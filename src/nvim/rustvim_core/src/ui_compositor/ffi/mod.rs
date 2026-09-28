//! C boundary adapters for the Rust compositor.

mod entrypoints;
pub(in crate::ui_compositor) mod grid_store;
mod nvim_output;
mod runtime;

#[cfg(test)]
pub(in crate::ui_compositor) use entrypoints::{
    apply_cursor_update_to_registered_grids, cursor_is_covered_by_registered_layers,
    place_grid_in_registered_layers, remove_registered_layer, ui_comp_find_grid_at_coord,
    ui_comp_get_cell_at_coord, update_registered_layer_policy,
};
pub(in crate::ui_compositor) use nvim_output::NvimOutput;
pub(in crate::ui_compositor) use runtime::{
    compositor_runtime_snapshot, initialize_debug_highlights,
};
