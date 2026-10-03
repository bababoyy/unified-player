use std::fmt::Debug;

use super::SelectionChange;

/// Provider-neutral contract for visible multi-selection behavior.
///
/// Page-specific adapters retain their own scope and identity rules, while
/// event handling can rely on one small set of shared operations. Contexts may
/// still expose different action capabilities without forking selection
/// mechanics.
pub trait MultiSelectModel {
    type Error: Debug;

    fn select_all_visible(&mut self) -> Result<SelectionChange, Self::Error>;

    fn invert_visible(&mut self) -> Result<SelectionChange, Self::Error>;

    /// Extend the inclusive range from the current cursor to `target` in the
    /// adapter's visible projection. Page handlers use this shared contract
    /// for Shift-based navigation so every list preserves the same gesture.
    fn extend_visible_range(
        &mut self,
        cursor: usize,
        target: usize,
    ) -> Result<SelectionChange, Self::Error>;

    fn clear_selection(&mut self) -> SelectionChange;

    fn selected_visible_indices(&self) -> Vec<usize>;
}
