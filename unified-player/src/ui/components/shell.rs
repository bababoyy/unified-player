//! Canonical application-shell layout contracts.
//!
//! The geometry implementation still lives in `ui::layout` while the
//! migration is in progress. Re-exporting it here gives page components one
//! stable import boundary before the legacy shell is removed.

pub(crate) use super::super::layout::{
    LayoutMode, LayoutPolicy, WorkspaceFrame, WorkspaceLayout, WorkspaceLayoutKind,
};
