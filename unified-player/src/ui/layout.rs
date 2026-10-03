use ratatui::layout::{Constraint, Layout, Rect};

use super::Orientation;

const WORKSPACE_NAVIGATION_RATIO_PERCENT: u32 = 15;
const WORKSPACE_NAVIGATION_MIN_WIDTH: u16 = 18;
const WORKSPACE_NAVIGATION_MAX_WIDTH: u16 = 26;

/// Coarse terminal-size bands used by page renderers.
///
/// The bands deliberately use stable thresholds instead of continuously
/// changing proportions. That keeps a resize from making a focused pane jump
/// between several subtly different layouts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LayoutMode {
    Compact,
    #[default]
    Normal,
    Wide,
}

impl LayoutMode {
    pub fn from_size(columns: u16, rows: u16) -> Self {
        if columns < 80 || rows <= 24 {
            Self::Compact
        } else if columns >= 120 && rows >= 32 {
            Self::Wide
        } else {
            Self::Normal
        }
    }

    pub const fn is_compact(self) -> bool {
        matches!(self, Self::Compact)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutPolicy {
    pub mode: LayoutMode,
    pub orientation: Orientation,
}

/// The design-v1 vertical chrome regions. The body is the only region whose
/// page renderer owns its internal composition; the other regions are shared
/// application chrome and therefore use stable integer cell geometry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceFrame {
    pub header: Rect,
    pub body: Rect,
    pub transport: Rect,
    pub footer: Rect,
    pub body_transport_separator: Rect,
    pub transport_footer_separator: Rect,
}

/// The body regions used by the Unified Player workspace.
///
/// The navigation, content, and optional queue/action rectangles are
/// calculated once per frame and reused by rendering and mouse hit-testing.
/// Opened collections use the right pane for the queue/action sidebar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceLayout {
    pub navigation: Rect,
    pub content: Rect,
    pub right: Rect,
    pub show_navigation: bool,
    pub show_right: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceLayoutKind {
    Library,
    Collection,
    Search,
    Settings,
    /// First-run setup: a step rail and a help inspector, both kept at
    /// shorter bodies than Settings because the transport band stays visible.
    Setup,
}

impl LayoutPolicy {
    /// Allocate the design-v1 header/body/transport/footer bands.
    pub fn workspace_frame(self, rect: Rect) -> WorkspaceFrame {
        let (header_height, transport_height, footer_height, separator_height) =
            if rect.height >= 32 {
                (3_u16, 7_u16, 2_u16, 1_u16)
            } else if rect.height >= 20 {
                (1, 3, 1, 1)
            } else if rect.height >= 10 {
                (1, 1, 1, 0)
            } else if rect.height >= 8 {
                // Content outranks chrome: the app header goes first...
                (0, 1, 1, 0)
            } else if rect.height >= 6 {
                // ...then the key hints, keeping the one-row transport.
                (0, 1, 0, 0)
            } else {
                return WorkspaceFrame {
                    body: rect,
                    ..WorkspaceFrame::default()
                };
            };
        let body_height = rect
            .height
            .saturating_sub(header_height)
            .saturating_sub(transport_height)
            .saturating_sub(footer_height)
            .saturating_sub(separator_height.saturating_mul(2));
        let header = Rect::new(rect.x, rect.y, rect.width, header_height);
        let body = Rect::new(
            rect.x,
            rect.y.saturating_add(header_height),
            rect.width,
            body_height,
        );
        let body_transport_separator =
            Rect::new(rect.x, body.bottom(), rect.width, separator_height);
        let transport = Rect::new(
            rect.x,
            body_transport_separator.bottom(),
            rect.width,
            transport_height,
        );
        let transport_footer_separator =
            Rect::new(rect.x, transport.bottom(), rect.width, separator_height);
        let footer = Rect::new(
            rect.x,
            transport_footer_separator.bottom(),
            rect.width,
            footer_height,
        );
        WorkspaceFrame {
            header,
            body,
            transport,
            footer,
            body_transport_separator,
            transport_footer_separator,
        }
    }

    /// Allocate the persistent Library/Context workspace with explicit cell
    /// geometry. Collection pages receive the queue/action pane only when the
    /// design-v1 wide threshold leaves a usable primary context region.
    pub fn workspace(self, rect: Rect, kind: WorkspaceLayoutKind) -> WorkspaceLayout {
        if rect.width == 0 || rect.height == 0 || rect.width < 20 || rect.height < 6 {
            return WorkspaceLayout {
                content: rect,
                ..WorkspaceLayout::default()
            };
        }

        let preferred = (u32::from(rect.width) * WORKSPACE_NAVIGATION_RATIO_PERCENT / 100) as u16;
        let navigation_width = preferred.clamp(
            WORKSPACE_NAVIGATION_MIN_WIDTH,
            WORKSPACE_NAVIGATION_MAX_WIDTH,
        );
        let show_navigation = if kind == WorkspaceLayoutKind::Settings {
            rect.width >= 80 && rect.height >= 20
        } else if kind == WorkspaceLayoutKind::Setup {
            rect.width >= 80 && rect.height >= 14
        } else {
            let primary_width = rect
                .width
                .saturating_sub(navigation_width)
                .saturating_sub(1);
            rect.width > navigation_width.saturating_add(2) && primary_width >= 32
        };
        if !show_navigation {
            return WorkspaceLayout {
                content: rect,
                ..WorkspaceLayout::default()
            };
        }

        let show_right = match kind {
            WorkspaceLayoutKind::Collection | WorkspaceLayoutKind::Settings => {
                rect.width >= 140 && rect.height >= 28
            }
            WorkspaceLayoutKind::Setup => rect.width >= 120 && rect.height >= 14,
            WorkspaceLayoutKind::Library | WorkspaceLayoutKind::Search => false,
        };
        let right_width = if show_right {
            if matches!(
                kind,
                WorkspaceLayoutKind::Settings | WorkspaceLayoutKind::Setup
            ) {
                ((u32::from(rect.width) * 24 / 100) as u16).clamp(32, 42)
            } else {
                ((u32::from(rect.width) * 20 / 100) as u16).clamp(28, 34)
            }
        } else {
            0
        };
        let content_x = rect.x.saturating_add(navigation_width).saturating_add(1);
        let content_width = rect
            .width
            .saturating_sub(navigation_width)
            .saturating_sub(1)
            .saturating_sub(if show_right { right_width + 1 } else { 0 });
        let right_x = content_x.saturating_add(content_width).saturating_add(1);
        WorkspaceLayout {
            navigation: Rect::new(rect.x, rect.y, navigation_width, rect.height),
            content: Rect::new(content_x, rect.y, content_width, rect.height),
            right: Rect::new(right_x, rect.y, right_width, rect.height),
            show_navigation,
            show_right,
        }
    }

    pub fn from_size(columns: u16, rows: u16) -> Self {
        let mode = LayoutMode::from_size(columns, rows);
        let orientation = match mode {
            LayoutMode::Compact => Orientation::Vertical,
            LayoutMode::Wide => Orientation::Horizontal,
            LayoutMode::Normal => Orientation::from_size(columns, rows),
        };
        Self { mode, orientation }
    }

    pub const fn new(mode: LayoutMode, orientation: Orientation) -> Self {
        Self { mode, orientation }
    }

    /// Give Diagnostics enough width for its inspector details on compact
    /// terminals. Wider layouts keep the overview and inspector side by side;
    /// compact layouts stack them so long routes and safe failure copy can wrap
    /// instead of becoming unreadable narrow columns.
    pub fn diagnostics_panes(self, rect: Rect) -> Vec<Rect> {
        if self.mode.is_compact() {
            Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(rect)
                .to_vec()
        } else {
            Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)])
                .split(rect)
                .to_vec()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_frame_matches_the_design_v1_canonical_bands() {
        let frame = LayoutPolicy::from_size(180, 49).workspace_frame(Rect::new(0, 0, 180, 49));

        assert_eq!(frame.header, Rect::new(0, 0, 180, 3));
        assert_eq!(frame.body, Rect::new(0, 3, 180, 35));
        assert_eq!(frame.body_transport_separator, Rect::new(0, 38, 180, 1));
        assert_eq!(frame.transport, Rect::new(0, 39, 180, 7));
        assert_eq!(frame.transport_footer_separator, Rect::new(0, 46, 180, 1));
        assert_eq!(frame.footer, Rect::new(0, 47, 180, 2));
    }

    #[test]
    fn workspace_frame_compresses_chrome_in_height_bands() {
        let medium = LayoutPolicy::from_size(80, 30).workspace_frame(Rect::new(0, 0, 80, 30));
        assert_eq!(medium.header.height, 1);
        assert_eq!(medium.body.height, 23);
        assert_eq!(medium.transport.height, 3);
        assert_eq!(medium.footer.height, 1);
        assert_eq!(medium.body_transport_separator.height, 1);

        let compact = LayoutPolicy::from_size(60, 16).workspace_frame(Rect::new(0, 0, 60, 16));
        assert_eq!(compact.header, Rect::new(0, 0, 60, 1));
        assert_eq!(compact.body, Rect::new(0, 1, 60, 13));
        assert_eq!(compact.transport, Rect::new(0, 14, 60, 1));
        assert_eq!(compact.footer, Rect::new(0, 15, 60, 1));
        assert!(compact.body_transport_separator.is_empty());
    }

    #[test]
    fn size_bands_match_supported_terminal_fixtures() {
        assert_eq!(LayoutMode::from_size(60, 20), LayoutMode::Compact);
        assert_eq!(LayoutMode::from_size(80, 23), LayoutMode::Compact);
        assert_eq!(LayoutMode::from_size(80, 24), LayoutMode::Compact);
        assert_eq!(LayoutMode::from_size(80, 30), LayoutMode::Normal);
        assert_eq!(LayoutMode::from_size(120, 40), LayoutMode::Wide);
    }

    #[test]
    fn workspace_uses_design_v1_wide_library_geometry() {
        let layout = LayoutPolicy::from_size(180, 49)
            .workspace(Rect::new(0, 3, 180, 35), WorkspaceLayoutKind::Library);

        assert_eq!(layout.navigation, Rect::new(0, 3, 26, 35));
        assert_eq!(layout.content, Rect::new(27, 3, 153, 35));
        assert!(layout.show_navigation);
        assert!(!layout.show_right);
    }

    #[test]
    fn workspace_uses_design_v1_collection_queue_sidebar_geometry() {
        let layout = LayoutPolicy::from_size(180, 49)
            .workspace(Rect::new(0, 3, 180, 35), WorkspaceLayoutKind::Collection);

        assert_eq!(layout.navigation, Rect::new(0, 3, 26, 35));
        assert_eq!(layout.content, Rect::new(27, 3, 118, 35));
        assert_eq!(layout.right, Rect::new(146, 3, 34, 35));
        assert_eq!(layout.navigation.right(), 26);
        assert_eq!(layout.content.right(), 145);
        assert_eq!(layout.right.x, 146);
        assert!(layout.show_navigation);
        assert!(layout.show_right);
    }

    #[test]
    fn workspace_navigation_width_is_shared_across_page_recipes() {
        let wide_rect = Rect::new(0, 3, 180, 35);
        for kind in [
            WorkspaceLayoutKind::Library,
            WorkspaceLayoutKind::Collection,
            WorkspaceLayoutKind::Search,
            WorkspaceLayoutKind::Settings,
        ] {
            assert_eq!(
                LayoutPolicy::from_size(180, 49)
                    .workspace(wide_rect, kind)
                    .navigation
                    .width,
                WORKSPACE_NAVIGATION_MAX_WIDTH
            );
        }

        let normal_rect = Rect::new(0, 0, 120, 30);
        for kind in [
            WorkspaceLayoutKind::Library,
            WorkspaceLayoutKind::Collection,
            WorkspaceLayoutKind::Search,
            WorkspaceLayoutKind::Settings,
        ] {
            assert_eq!(
                LayoutPolicy::from_size(120, 30)
                    .workspace(normal_rect, kind)
                    .navigation
                    .width,
                WORKSPACE_NAVIGATION_MIN_WIDTH
            );
        }
    }

    #[test]
    fn workspace_uses_design_v1_settings_inspector_geometry() {
        let layout = LayoutPolicy::from_size(180, 49)
            .workspace(Rect::new(0, 3, 180, 35), WorkspaceLayoutKind::Settings);

        assert_eq!(layout.navigation, Rect::new(0, 3, 26, 35));
        assert_eq!(layout.content, Rect::new(27, 3, 110, 35));
        assert_eq!(layout.right, Rect::new(138, 3, 42, 35));
        assert!(layout.show_navigation);
        assert!(layout.show_right);
    }

    #[test]
    fn setup_keeps_rail_and_inspector_on_short_bodies() {
        let wide = LayoutPolicy::from_size(120, 40)
            .workspace(Rect::new(0, 3, 120, 26), WorkspaceLayoutKind::Setup);
        assert!(wide.show_navigation);
        assert!(wide.show_right);
        assert_eq!(wide.right.width, 32);

        let normal = LayoutPolicy::from_size(80, 24)
            .workspace(Rect::new(0, 1, 80, 17), WorkspaceLayoutKind::Setup);
        assert!(normal.show_navigation);
        assert!(!normal.show_right);

        let compact = LayoutPolicy::from_size(60, 20)
            .workspace(Rect::new(0, 1, 60, 13), WorkspaceLayoutKind::Setup);
        assert!(!compact.show_navigation);
        assert_eq!(compact.content, Rect::new(0, 1, 60, 13));
    }

    #[test]
    fn settings_removes_inspector_then_category_rail_at_responsive_breakpoints() {
        let normal = LayoutPolicy::from_size(100, 30)
            .workspace(Rect::new(0, 0, 100, 30), WorkspaceLayoutKind::Settings);
        assert!(normal.show_navigation);
        assert!(!normal.show_right);
        assert_eq!(normal.content, Rect::new(19, 0, 81, 30));

        let compact = LayoutPolicy::from_size(60, 20)
            .workspace(Rect::new(0, 0, 60, 20), WorkspaceLayoutKind::Settings);
        assert!(!compact.show_navigation);
        assert!(!compact.show_right);
        assert_eq!(compact.content, Rect::new(0, 0, 60, 20));
    }

    #[test]
    fn workspace_hides_queue_sidebar_before_the_context_gets_too_narrow() {
        let layout = LayoutPolicy::from_size(120, 30)
            .workspace(Rect::new(0, 0, 120, 30), WorkspaceLayoutKind::Collection);

        assert!(layout.show_navigation);
        assert!(!layout.show_right);
        assert_eq!(layout.content, Rect::new(19, 0, 101, 30));
        assert!(layout.right.is_empty());
    }

    #[test]
    fn workspace_collapses_before_it_makes_the_primary_pane_unusable() {
        let layout = LayoutPolicy::from_size(60, 20)
            .workspace(Rect::new(0, 0, 60, 20), WorkspaceLayoutKind::Collection);
        assert!(layout.show_navigation);
        assert_eq!(layout.navigation.width, 18);
        assert_eq!(layout.content.x, 19);
        assert_eq!(layout.content.width, 41);

        let emergency = LayoutPolicy::from_size(19, 6)
            .workspace(Rect::new(0, 0, 19, 6), WorkspaceLayoutKind::Library);
        assert!(!emergency.show_navigation);
        assert_eq!(emergency.content, Rect::new(0, 0, 19, 6));
    }

    #[test]
    fn compact_diagnostics_stacks_panels_for_readable_width() {
        let rect = Rect::new(2, 3, 56, 16);
        let panes = LayoutPolicy::from_size(60, 20).diagnostics_panes(rect);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].width, rect.width);
        assert_eq!(panes[1].width, rect.width);
        assert_eq!(panes[0].x, panes[1].x);
        assert_eq!(panes[0].bottom(), panes[1].y);
    }

    #[test]
    fn wide_diagnostics_keeps_side_by_side_panels() {
        let rect = Rect::new(2, 3, 116, 30);
        let panes = LayoutPolicy::from_size(120, 40).diagnostics_panes(rect);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].height, rect.height);
        assert_eq!(panes[1].height, rect.height);
        assert_eq!(panes[0].right(), panes[1].x);
    }
}
