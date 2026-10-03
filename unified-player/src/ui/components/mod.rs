//! Shared UI component primitives.
//!
//! Page modules remain responsible for composing provider/page-specific
//! content. These modules own geometry and surface contracts that must stay
//! identical when the same component appears in a workspace or legacy page.

pub(crate) mod collection;
pub(crate) mod footer;
pub(crate) mod help;
pub(crate) mod history;
pub(crate) mod list;
pub(crate) mod navigation;
pub(crate) mod popup_surface;
pub(crate) mod queue;
pub(crate) mod search;
pub(crate) mod shell;
