//! Mouse hit testing.
//!
//! ratatui draws into a buffer and keeps no scene graph, so there is nothing
//! to hit-test against after the fact. Instead each render registers the
//! rectangles it drew something clickable into, paired with the action that
//! click means. The registry is rebuilt every frame, which keeps it honest:
//! a region that stopped being drawn stops being clickable in the same frame.
//!
//! Later registrations win, so a control drawn on top of a panel takes the
//! click — matching what the user sees.

use ratatui::layout::{Position, Rect};

/// What clicking a region does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Switch to a page in the sidebar.
    Page(super::app::Page),
    /// Select a time range chip.
    Range(super::app::Range),
    /// Toggle a tool on or off.
    ToggleSource(crate::model::Source),
    /// Select row `usize` of the focused list.
    Row(usize),
    /// 打开项目并显示它的 sessions。
    ProjectRow(usize),
    /// 打开 session replay。
    SessionRow(usize),
    /// 返回当前项目的 Sessions 页。
    BackToSessions,
    /// 播放或暂停 replay。
    ReplayToggle,
    /// 设置 replay 倍速。
    ReplaySpeed(u8),
    /// 跳到 replay 中的具体事件。
    ReplaySeek(usize),
    /// Clear any drill-down filter.
    ClearFilter,
    /// Re-run the scan.
    Refresh,
    /// Start or stop rescanning on a timer.
    ToggleWatch,
    /// Leave the dashboard.
    Quit,
}

#[derive(Debug, Clone)]
struct Region {
    rect: Rect,
    action: Action,
    /// Highlight the row/tile under the pointer.
    hover_id: Option<u64>,
}

#[derive(Debug, Default)]
pub struct Registry {
    regions: Vec<Region>,
}

impl Registry {
    pub fn clear(&mut self) {
        self.regions.clear();
    }

    pub fn add(&mut self, rect: Rect, action: Action) {
        self.push(rect, action, None);
    }

    /// Register a region that also highlights on hover.
    pub fn add_hoverable(&mut self, rect: Rect, action: Action, hover_id: u64) {
        self.push(rect, action, Some(hover_id));
    }

    fn push(&mut self, rect: Rect, action: Action, hover_id: Option<u64>) {
        // A zero-area rect can never be clicked; keeping it would only make
        // hit-testing walk more entries.
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.regions.push(Region { rect, action, hover_id });
    }

    /// The action at a point, topmost first.
    pub fn hit(&self, x: u16, y: u16) -> Option<&Action> {
        self.region_at(x, y).map(|r| &r.action)
    }

    /// The hover id at a point, if the topmost region under it has one.
    pub fn hover_at(&self, x: u16, y: u16) -> Option<u64> {
        self.region_at(x, y).and_then(|r| r.hover_id)
    }

    fn region_at(&self, x: u16, y: u16) -> Option<&Region> {
        let p = Position::new(x, y);
        self.regions.iter().rev().find(|r| r.rect.contains(p))
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.regions.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::Page;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, width: w, height: h }
    }

    #[test]
    fn a_point_inside_a_region_resolves_to_its_action() {
        let mut r = Registry::default();
        r.add(rect(2, 3, 10, 2), Action::Page(Page::Models));
        assert_eq!(r.hit(2, 3), Some(&Action::Page(Page::Models)));
        assert_eq!(r.hit(11, 4), Some(&Action::Page(Page::Models)));
        assert_eq!(r.hit(12, 4), None, "just past the right edge");
        assert_eq!(r.hit(2, 5), None, "just past the bottom edge");
    }

    #[test]
    fn the_topmost_region_wins() {
        let mut r = Registry::default();
        r.add(rect(0, 0, 20, 10), Action::Refresh);
        r.add(rect(5, 5, 3, 1), Action::Quit);
        assert_eq!(r.hit(6, 5), Some(&Action::Quit));
        assert_eq!(r.hit(1, 1), Some(&Action::Refresh));
    }

    #[test]
    fn degenerate_rects_are_not_registered() {
        let mut r = Registry::default();
        r.add(rect(0, 0, 0, 5), Action::Quit);
        r.add(rect(0, 0, 5, 0), Action::Quit);
        assert!(r.is_empty());
    }

    #[test]
    fn clearing_removes_stale_regions() {
        let mut r = Registry::default();
        r.add(rect(0, 0, 5, 5), Action::Quit);
        r.clear();
        assert_eq!(r.hit(1, 1), None, "a region no longer drawn is no longer clickable");
    }

    #[test]
    fn hover_ids_come_from_the_topmost_region_only() {
        let mut r = Registry::default();
        r.add_hoverable(rect(0, 0, 10, 3), Action::Row(0), 7);
        r.add(rect(0, 0, 10, 1), Action::Refresh);
        assert_eq!(r.hover_at(1, 1), Some(7));
        assert_eq!(r.hover_at(1, 0), None, "the plain region on top has no hover id");
    }
}
