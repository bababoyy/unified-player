//! Shared navigation-rail model and geometry.

use crate::state::{WorkspaceNavigationItem, WorkspaceRailItem, WorkspaceScopeKind};
use ratatui::layout::Rect;

/// The complete rail model used by both the renderer and its hit map.
///
/// Scope entries are deliberately included as separate roles rather than
/// being folded into the route list. Keyboard route movement still traverses
/// only `WorkspaceNavigationItem::ALL`, while rendering and hit testing use
/// this single visible-entry model.
pub(crate) const fn workspace_rail_items() -> [WorkspaceRailItem; 10] {
    [
        WorkspaceRailItem::Route(WorkspaceNavigationItem::Home),
        WorkspaceRailItem::Route(WorkspaceNavigationItem::Playlists),
        WorkspaceRailItem::Route(WorkspaceNavigationItem::LikedMusic),
        WorkspaceRailItem::Route(WorkspaceNavigationItem::Albums),
        WorkspaceRailItem::Route(WorkspaceNavigationItem::Artists),
        WorkspaceRailItem::Route(WorkspaceNavigationItem::Search),
        WorkspaceRailItem::Route(WorkspaceNavigationItem::Queue),
        WorkspaceRailItem::Scope(WorkspaceScopeKind::ALL[0]),
        WorkspaceRailItem::Scope(WorkspaceScopeKind::ALL[1]),
        WorkspaceRailItem::Scope(WorkspaceScopeKind::ALL[2]),
    ]
}

/// Rails shorter than this put routes on consecutive rows, so the scope
/// controls still fit below them instead of overlapping.
const SPACED_RAIL_MIN_HEIGHT: u16 = 22;

const fn is_compact(nav: Rect) -> bool {
    nav.height < SPACED_RAIL_MIN_HEIGHT
}

/// Return the route row's y-coordinate in the stable rail: Home above the
/// "Library" heading, collection routes under it, utility routes after the
/// divider.
pub(crate) const fn route_row_y(nav: Rect, item: WorkspaceNavigationItem) -> u16 {
    let compact = is_compact(nav);
    if matches!(item, WorkspaceNavigationItem::Home) {
        return nav.y.saturating_add(if compact { 0 } else { 1 });
    }
    let (first, pitch, section_gap) = if compact { (3, 1, 1) } else { (5, 2, 2) };
    let section_gap = if item.index() >= 5 { section_gap } else { 0 };
    nav.y
        .saturating_add(first)
        .saturating_add(((item.index() - 1) as u16).saturating_mul(pitch))
        .saturating_add(section_gap)
}

/// Return the "Library" heading above the collection routes.
pub(crate) fn library_heading_rect(nav: Rect) -> Rect {
    row_rect(
        nav,
        nav.y.saturating_add(if is_compact(nav) { 2 } else { 3 }),
    )
}

/// Return the full interactive row for one route.
pub(crate) fn route_row_rect(nav: Rect, item: WorkspaceNavigationItem) -> Rect {
    row_rect(nav, route_row_y(nav, item))
}

/// Return the divider between collection routes and utility routes.
pub(crate) fn route_divider_rect(nav: Rect) -> Rect {
    Rect::new(
        nav.x.saturating_add(2),
        nav.y.saturating_add(if is_compact(nav) { 7 } else { 13 }),
        nav.width.saturating_sub(4),
        1,
    )
}

/// Return the full interactive row for one provider/account scope control,
/// or an empty rect when the rail has no room for the scopes below the routes.
pub(crate) fn scope_row_rect(nav: Rect, kind: WorkspaceScopeKind) -> Rect {
    let last_route = route_row_y(nav, WorkspaceNavigationItem::Queue);
    let preferred = if is_compact(nav) { 11 } else { 30 };
    let scope_start = nav
        .y
        .saturating_add(preferred)
        .min(nav.bottom().saturating_sub(3));
    if scope_start <= last_route.saturating_add(1) {
        return Rect::default();
    }
    row_rect(nav, scope_start.saturating_add(kind.index() as u16))
}

/// Return the full interactive row for a visible navigation y-coordinate.
pub(crate) fn row_rect(nav: Rect, y: u16) -> Rect {
    Rect::new(nav.x.saturating_add(2), y, nav.width.saturating_sub(4), 1)
}

/// Return the label cell inside a navigation row.
pub(crate) fn label_rect(row: Rect) -> Rect {
    Rect::new(
        row.x.saturating_add(2),
        row.y,
        row.width.saturating_sub(2),
        row.height,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        label_rect, library_heading_rect, route_divider_rect, route_row_rect, row_rect,
        scope_row_rect, workspace_rail_items,
    };
    use crate::state::{WorkspaceHit, WorkspaceNavigationItem, WorkspaceScopeKind};
    use ratatui::layout::Rect;

    #[test]
    fn navigation_geometry_keeps_hit_row_and_label_inside_the_rail() {
        let row = row_rect(Rect::new(2, 1, 24, 20), 7);
        assert_eq!(row, Rect::new(4, 7, 20, 1));
        assert_eq!(label_rect(row), Rect::new(6, 7, 18, 1));
    }

    #[test]
    fn visible_rail_entries_have_one_geometry_model_and_distinct_roles() {
        let nav = Rect::new(2, 1, 24, 40);
        let routes = WorkspaceNavigationItem::ALL.map(|item| route_row_rect(nav, item));
        assert_eq!(routes[0], Rect::new(4, 2, 20, 1));
        assert_eq!(library_heading_rect(nav), Rect::new(4, 4, 20, 1));
        assert_eq!(routes[1], Rect::new(4, 6, 20, 1));
        assert_eq!(routes[4], Rect::new(4, 12, 20, 1));
        assert_eq!(routes[5], Rect::new(4, 16, 20, 1));
        assert_eq!(routes[6], Rect::new(4, 18, 20, 1));
        assert!(routes.windows(2).all(|pair| pair[0].bottom() <= pair[1].y));

        let divider = route_divider_rect(nav);
        assert_eq!(divider, Rect::new(4, 14, 20, 1));
        assert!(routes[4].bottom() <= divider.y && divider.bottom() <= routes[5].y);
        let scope = scope_row_rect(nav, WorkspaceScopeKind::Browsing);
        assert_eq!(scope, Rect::new(4, 31, 20, 1));

        assert_eq!(workspace_rail_items().len(), 10);
        assert_eq!(
            workspace_rail_items()[0].hit(),
            WorkspaceHit::Navigation(WorkspaceNavigationItem::Home)
        );
        assert_eq!(
            workspace_rail_items()[7].hit(),
            WorkspaceHit::Scope(WorkspaceScopeKind::Browsing)
        );
    }

    #[test]
    fn short_rails_stack_routes_and_never_overlap_the_scopes() {
        for height in 8..=40 {
            let nav = Rect::new(2, 1, 24, height);
            let mut rows: Vec<Rect> = WorkspaceNavigationItem::ALL
                .map(|item| route_row_rect(nav, item))
                .into_iter()
                .chain([library_heading_rect(nav), route_divider_rect(nav)])
                .chain(
                    WorkspaceScopeKind::ALL
                        .into_iter()
                        .map(|kind| scope_row_rect(nav, kind))
                        .filter(|row| !row.is_empty()),
                )
                .collect();
            rows.sort_by_key(|row| row.y);
            assert!(
                rows.windows(2).all(|pair| pair[0].y < pair[1].y),
                "rail rows overlap at height {height}"
            );
        }
        let short = Rect::new(2, 1, 24, 18);
        assert!(!scope_row_rect(short, WorkspaceScopeKind::Playback).is_empty());
        assert!(scope_row_rect(short, WorkspaceScopeKind::Playback).bottom() <= short.bottom());
    }
}
