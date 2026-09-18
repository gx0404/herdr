//! 客户端停靠布局：只计算几何和标签归属，不拥有终端运行时。

use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub(super) enum PanelId {
    Workspaces,
    Agents,
    Monitor,
    Accounts,
    Terminal(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum DockNode {
    Panel {
        panel: PanelId,
    },
    Split {
        axis: Axis,
        ratio: f32,
        first: Box<DockNode>,
        second: Box<DockNode>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TabGroup {
    pub id: u64,
    pub tabs: Vec<String>,
    pub active: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct DockLayout {
    pub version: u32,
    pub root: DockNode,
    pub groups: Vec<TabGroup>,
    pub focused: PanelId,
    pub maximized: Option<PanelId>,
    pub locked: bool,
    pub revision: u64,
}

#[derive(Clone, Debug)]
pub(super) struct Divider {
    pub path: Vec<bool>,
    pub axis: Axis,
    pub area: Rect,
    pub handle: Rect,
}

#[derive(Default)]
pub(super) struct Geometry {
    pub panels: Vec<(PanelId, Rect)>,
    pub dividers: Vec<Divider>,
    pub compact: bool,
}

impl DockNode {
    fn panel(panel: PanelId) -> Self {
        Self::Panel { panel }
    }

    fn split(axis: Axis, ratio: f32, first: Self, second: Self) -> Self {
        Self::Split {
            axis,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    pub fn contains(&self, target: &PanelId) -> bool {
        match self {
            Self::Panel { panel } => panel == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }

    fn without(self, target: &PanelId) -> Option<Self> {
        match self {
            Self::Panel { ref panel } if panel == target => None,
            Self::Panel { .. } => Some(self),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.without(target), second.without(target)) {
                (Some(a), Some(b)) => Some(Self::split(axis, ratio, a, b)),
                (Some(node), None) | (None, Some(node)) => Some(node),
                (None, None) => None,
            },
        }
    }

    fn insert(&mut self, panel: PanelId, target: &PanelId, edge: Edge) -> bool {
        match self {
            Self::Panel { panel: current } if current == target => {
                let axis = match edge {
                    Edge::Left | Edge::Right => Axis::Horizontal,
                    Edge::Top | Edge::Bottom => Axis::Vertical,
                };
                let original = self.clone();
                let added = Self::panel(panel);
                *self = match edge {
                    Edge::Left | Edge::Top => Self::split(axis, 0.5, added, original),
                    Edge::Right | Edge::Bottom => Self::split(axis, 0.5, original, added),
                };
                true
            }
            Self::Panel { .. } => false,
            Self::Split { first, second, .. } => {
                first.insert(panel.clone(), target, edge) || second.insert(panel, target, edge)
            }
        }
    }

    fn minimum(&self) -> (u16, u16) {
        match self {
            Self::Panel { panel } => match panel {
                PanelId::Workspaces | PanelId::Agents => (18, 5),
                PanelId::Monitor | PanelId::Accounts => (28, 8),
                PanelId::Terminal(_) => (18, 5),
            },
            Self::Split {
                axis,
                first,
                second,
                ..
            } => {
                let (aw, ah) = first.minimum();
                let (bw, bh) = second.minimum();
                match axis {
                    Axis::Horizontal => (aw.saturating_add(bw).saturating_add(1), ah.max(bh)),
                    Axis::Vertical => (aw.max(bw), ah.saturating_add(bh).saturating_add(1)),
                }
            }
        }
    }

    fn geometry(&self, area: Rect, path: &mut Vec<bool>, result: &mut Geometry) {
        match self {
            Self::Panel { panel } => result.panels.push((panel.clone(), area)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (first_min, second_min, available) = match axis {
                    Axis::Horizontal => (first.minimum().0, second.minimum().0, area.width),
                    Axis::Vertical => (first.minimum().1, second.minimum().1, area.height),
                };
                let usable = available.saturating_sub(1);
                let lower = first_min.min(usable);
                let upper = usable.saturating_sub(second_min).max(lower);
                let size = ((f32::from(usable) * ratio.clamp(0.05, 0.95)).round() as u16)
                    .clamp(lower, upper)
                    .min(usable);
                let remaining = usable.saturating_sub(size);
                let (a, handle, b) = match axis {
                    Axis::Horizontal => (
                        Rect::new(area.x, area.y, size, area.height),
                        Rect::new(
                            area.x.saturating_add(size),
                            area.y,
                            u16::from(available > 0),
                            area.height,
                        ),
                        Rect::new(
                            area.x.saturating_add(size).saturating_add(1),
                            area.y,
                            remaining,
                            area.height,
                        ),
                    ),
                    Axis::Vertical => (
                        Rect::new(area.x, area.y, area.width, size),
                        Rect::new(
                            area.x,
                            area.y.saturating_add(size),
                            area.width,
                            u16::from(available > 0),
                        ),
                        Rect::new(
                            area.x,
                            area.y.saturating_add(size).saturating_add(1),
                            area.width,
                            remaining,
                        ),
                    ),
                };
                result.dividers.push(Divider {
                    path: path.clone(),
                    axis: *axis,
                    area,
                    handle,
                });
                path.push(false);
                first.geometry(a, path, result);
                path.pop();
                path.push(true);
                second.geometry(b, path, result);
                path.pop();
            }
        }
    }

    fn validate(&self, depth: usize, panels: &mut Vec<PanelId>) -> bool {
        if depth > 16 || panels.len() > 64 {
            return false;
        }
        match self {
            Self::Panel { panel } => {
                if panels.contains(panel) {
                    return false;
                }
                panels.push(panel.clone());
                true
            }
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                ratio.is_finite()
                    && (0.05..=0.95).contains(ratio)
                    && first.validate(depth + 1, panels)
                    && second.validate(depth + 1, panels)
            }
        }
    }
}

impl Default for DockLayout {
    fn default() -> Self {
        Self::from_sidebar(0.22, 0.6)
    }
}

impl DockLayout {
    pub fn from_sidebar(width_ratio: f32, section_ratio: f32) -> Self {
        Self {
            version: 1,
            root: DockNode::split(
                Axis::Horizontal,
                width_ratio.clamp(0.05, 0.95),
                DockNode::split(
                    Axis::Vertical,
                    section_ratio.clamp(0.05, 0.95),
                    DockNode::panel(PanelId::Workspaces),
                    DockNode::panel(PanelId::Agents),
                ),
                DockNode::panel(PanelId::Terminal(1)),
            ),
            groups: vec![TabGroup {
                id: 1,
                tabs: Vec::new(),
                active: None,
            }],
            focused: PanelId::Terminal(1),
            maximized: None,
            locked: false,
            revision: 0,
        }
    }

    pub fn valid(&self) -> bool {
        let mut panels = Vec::new();
        if self.version != 1
            || !self.root.validate(0, &mut panels)
            || !panels.contains(&self.focused)
            || self.maximized.as_ref().is_some_and(|p| !panels.contains(p))
            || self.groups.is_empty()
            || self.groups.len() > 32
        {
            return false;
        }
        let mut ids = std::collections::HashSet::new();
        let mut tabs = std::collections::HashSet::new();
        self.groups.iter().all(|group| {
            ids.insert(group.id)
                && panels.contains(&PanelId::Terminal(group.id))
                && group.tabs.iter().all(|tab| tabs.insert(tab))
                && group
                    .active
                    .as_ref()
                    .is_none_or(|tab| group.tabs.contains(tab))
        }) && panels.iter().all(|panel| match panel {
            PanelId::Terminal(id) => ids.contains(id),
            _ => true,
        })
    }

    pub fn geometry(&self, area: Rect) -> Geometry {
        let mut geometry = Geometry::default();
        let minimum = self.root.minimum();
        let compact = area.width < minimum.0 || area.height < minimum.1;
        if let Some(panel) = self.maximized.as_ref().or(compact.then_some(&self.focused)) {
            geometry.panels.push((panel.clone(), area));
            geometry.compact = compact;
        } else {
            self.root.geometry(area, &mut Vec::new(), &mut geometry);
        }
        geometry
    }

    pub fn dock(&mut self, panel: PanelId, target: &PanelId, edge: Edge) -> bool {
        if self.locked || &panel == target || !self.root.contains(target) {
            return false;
        }
        let Some(mut root) = self.root.clone().without(&panel) else {
            return false;
        };
        if !root.insert(panel.clone(), target, edge) {
            return false;
        }
        if !root.validate(0, &mut Vec::new()) {
            return false;
        }
        self.root = root;
        self.focused = panel;
        self.maximized = None;
        self.revision = self.revision.saturating_add(1);
        true
    }

    pub fn resize(&mut self, path: &[bool], ratio: f32) -> bool {
        if self.locked || !ratio.is_finite() {
            return false;
        }
        let mut node = &mut self.root;
        for branch in path {
            let DockNode::Split { first, second, .. } = node else {
                return false;
            };
            node = if *branch { second } else { first };
        }
        let DockNode::Split { ratio: current, .. } = node else {
            return false;
        };
        *current = ratio.clamp(0.05, 0.95);
        true
    }

    pub fn close_panel(&mut self, panel: &PanelId) -> bool {
        if self.locked || matches!(panel, PanelId::Terminal(_)) {
            return false;
        }
        let Some(root) = self.root.clone().without(panel) else {
            return false;
        };
        self.root = root;
        self.maximized = None;
        if self.focused == *panel {
            self.focused = PanelId::Terminal(self.groups.first().map_or(1, |group| group.id));
        }
        true
    }

    /// One-way preference migration: the accounts page merged into the
    /// monitor panel, so legacy layouts drop their standalone accounts panel
    /// and keep only the geometry that still applies.
    pub fn discard_accounts_panel(&mut self) {
        while self.root.contains(&PanelId::Accounts) {
            let Some(root) = self.root.clone().without(&PanelId::Accounts) else {
                break;
            };
            self.root = root;
        }
        if self.maximized.as_ref() == Some(&PanelId::Accounts) {
            self.maximized = None;
        }
        if self.focused == PanelId::Accounts {
            self.focused = PanelId::Terminal(self.groups.first().map_or(1, |group| group.id));
        }
    }

    /// Reconcile tab membership against the server snapshot.
    ///
    /// The first group is the primary terminal strip and mirrors the focused
    /// workspace's tabs only, so each workspace owns an independent terminal
    /// page and switching workspaces swaps the strip wholesale. Tabs dragged
    /// into secondary groups keep their explicit membership (deduped against
    /// `existing_tabs`) and stay visible even when their workspace is not
    /// focused. `active` re-anchors the primary strip's selected tab.
    pub fn reconcile_workspace_tabs(
        &mut self,
        focused_tabs: &[String],
        existing_tabs: &[String],
        active: Option<&str>,
    ) -> bool {
        let mut changed = false;
        let mut seen = std::collections::HashSet::new();
        for group in self.groups.iter_mut().skip(1) {
            let previous_len = group.tabs.len();
            group
                .tabs
                .retain(|tab| existing_tabs.contains(tab) && seen.insert(tab.clone()));
            changed |= previous_len != group.tabs.len();
            if group
                .active
                .as_ref()
                .is_none_or(|tab| !group.tabs.contains(tab))
            {
                let next = group.tabs.first().cloned();
                changed |= group.active != next;
                group.active = next;
            }
        }
        if let Some(primary) = self.groups.first_mut() {
            let previous = std::mem::take(&mut primary.tabs);
            primary.tabs = focused_tabs
                .iter()
                .filter(|tab| !seen.contains(*tab))
                .cloned()
                .collect();
            changed |= previous != primary.tabs;
            let next = match active {
                Some(tab) if primary.tabs.iter().any(|id| id == tab) => Some(tab.to_owned()),
                // Without a server focus change, keep a local tab selection
                // that is still a member of the strip.
                _ => primary
                    .active
                    .as_ref()
                    .filter(|tab| primary.tabs.contains(*tab))
                    .cloned()
                    .or_else(|| primary.tabs.first().cloned()),
            };
            changed |= primary.active != next;
            primary.active = next;
        }
        let previous_groups = self.groups.len();
        self.remove_empty_groups();
        changed || previous_groups != self.groups.len()
    }

    fn remove_empty_groups(&mut self) {
        let empty = self
            .groups
            .iter()
            .filter(|g| g.tabs.is_empty())
            .map(|g| g.id)
            .collect::<Vec<_>>();
        for id in empty {
            if self.groups.len() <= 1 {
                break;
            }
            let panel = PanelId::Terminal(id);
            if let Some(root) = self.root.clone().without(&panel) {
                self.root = root;
                self.groups.retain(|g| g.id != id);
                if self.maximized.as_ref() == Some(&panel) {
                    self.maximized = None;
                }
                if self.focused == panel {
                    if let Some(group) = self.groups.first() {
                        self.focused = PanelId::Terminal(group.id);
                    }
                }
            }
        }
    }

    pub fn move_tab(&mut self, tab: &str, target: u64, index: usize, edge: Option<Edge>) -> bool {
        if self.locked
            || !self.groups.iter().any(|g| g.id == target)
            || !self
                .groups
                .iter()
                .any(|g| g.tabs.iter().any(|id| id == tab))
        {
            return false;
        }
        if edge.is_some()
            && self
                .groups
                .iter()
                .any(|g| g.id == target && g.tabs == [tab])
        {
            return false;
        }
        let next_id = self
            .groups
            .iter()
            .map(|g| g.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let destination = if edge.is_some() { next_id } else { target };
        if let Some(edge) = edge {
            if self.groups.len() >= 32
                || !self.dock(
                    PanelId::Terminal(destination),
                    &PanelId::Terminal(target),
                    edge,
                )
            {
                return false;
            }
            self.groups.push(TabGroup {
                id: destination,
                tabs: Vec::new(),
                active: None,
            });
        }
        for group in &mut self.groups {
            group.tabs.retain(|id| id != tab);
            if group.active.as_deref() == Some(tab) {
                group.active = group.tabs.first().cloned();
            }
        }
        if let Some(group) = self.groups.iter_mut().find(|g| g.id == destination) {
            group
                .tabs
                .insert(index.min(group.tabs.len()), tab.to_owned());
            group.active = Some(tab.to_owned());
        }
        self.focused = PanelId::Terminal(destination);
        self.remove_empty_groups();
        self.revision = self.revision.saturating_add(1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_panels_preserves_unique_identity_and_bounds() {
        let mut layout = DockLayout::default();
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            assert!(layout.dock(PanelId::Agents, &PanelId::Terminal(1), edge));
            assert!(layout.valid());
            let area = Rect::new(3, 2, 140, 50);
            let geometry = layout.geometry(area);
            assert_eq!(geometry.panels.len(), 3);
            for (_, rect) in &geometry.panels {
                assert!(rect.x >= area.x && rect.y >= area.y);
                assert!(rect.right() <= area.right() && rect.bottom() <= area.bottom());
            }
        }
    }

    #[test]
    fn narrow_screen_projection_does_not_destroy_saved_layout() {
        let layout = DockLayout::default();
        let saved = serde_json::to_string(&layout).unwrap();
        for area in [Rect::new(0, 0, 0, 0), Rect::new(0, 0, 20, 4)] {
            let geometry = layout.geometry(area);
            assert!(geometry.compact);
            assert_eq!(geometry.panels, vec![(PanelId::Terminal(1), area)]);
        }
        assert_eq!(serde_json::to_string(&layout).unwrap(), saved);
        assert_eq!(layout.geometry(Rect::new(0, 0, 140, 50)).panels.len(), 3);
    }

    #[test]
    fn splitting_and_merging_groups_retains_tabs_exactly_once() {
        let mut layout = DockLayout::default();
        let tabs = vec!["a".into(), "b".into(), "c".into()];
        layout.reconcile_workspace_tabs(&tabs, &tabs, Some("a"));
        assert!(layout.move_tab("b", 1, 0, Some(Edge::Right)));
        assert_eq!(layout.groups.len(), 2);
        assert!(layout.valid());
        assert!(layout.move_tab("b", 1, 1, None));
        assert_eq!(layout.groups.len(), 1);
        assert_eq!(layout.groups[0].tabs, tabs);
        assert!(layout.valid());
    }

    #[test]
    fn primary_strip_mirrors_the_focused_workspace_only() {
        let mut layout = DockLayout::default();
        let ws_one = vec!["t1".to_owned(), "t2".to_owned()];
        let ws_two = vec!["t3".to_owned()];
        let all = [ws_one.clone(), ws_two.clone()].concat();
        layout.reconcile_workspace_tabs(&ws_one, &all, Some("t2"));
        assert_eq!(layout.groups[0].tabs, ws_one);
        assert_eq!(layout.groups[0].active.as_deref(), Some("t2"));
        // A tab dragged into a secondary group stays there even while its
        // workspace is not focused, and is not duplicated in the primary.
        assert!(layout.move_tab("t2", 1, 0, Some(Edge::Right)));
        // Switching the focused workspace swaps the primary strip wholesale.
        layout.reconcile_workspace_tabs(&ws_two, &all, Some("t3"));
        assert_eq!(layout.groups[0].tabs, ws_two);
        assert_eq!(layout.groups[0].active.as_deref(), Some("t3"));
        assert_eq!(layout.groups[1].tabs, vec!["t2".to_owned()]);
        layout.reconcile_workspace_tabs(&ws_one, &all, Some("t1"));
        assert_eq!(layout.groups[0].tabs, vec!["t1".to_owned()]);
        assert_eq!(layout.groups[1].tabs, vec!["t2".to_owned()]);
        assert_eq!(layout.groups[1].active.as_deref(), Some("t2"));
        assert!(layout.valid());
    }

    #[test]
    fn invalid_drop_and_stale_split_path_are_transactional() {
        let mut layout = DockLayout::default();
        let before = layout.clone();
        assert!(!layout.dock(PanelId::Agents, &PanelId::Terminal(77), Edge::Left));
        assert!(!layout.resize(&[true, false], 0.4));
        assert!(!layout.resize(&[], f32::NAN));
        assert_eq!(layout, before);
    }

    #[test]
    fn corrupt_preferences_reject_duplicate_or_unowned_panels() {
        let mut layout = DockLayout::default();
        assert!(layout.valid());
        layout.groups.push(layout.groups[0].clone());
        assert!(!layout.valid());
        let layout = DockLayout {
            focused: PanelId::Terminal(99),
            ..Default::default()
        };
        assert!(!layout.valid());
    }
}
