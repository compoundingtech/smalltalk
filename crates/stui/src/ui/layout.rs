//! A glass's arrangement: a tree of splits whose leaves are groups of tabs, each tab one pane
//! key. This is the shape a glass is stored in, on this device and in the graph, so it holds
//! structure only. Which tab each group shows, focus and sizes stay with each window.

use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// Side by side: the second child to the right.
    Right,
    /// Stacked: the second child below.
    Below,
}

/// One tab: a pane key, and a name the person gave it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub pane: String,
}

impl Tab {
    pub fn pane(key: impl Into<String>) -> Self {
        Self {
            title: None,
            pane: key.into(),
        }
    }
}

/// A split's tabs. Which one shows is this window's, never stored.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Group {
    pub tabs: Vec<Tab>,
    #[serde(skip)]
    pub current: usize,
}

/// Groups are equal by their tabs; which tab a window shows is not structure.
impl PartialEq for Group {
    fn eq(&self, other: &Self) -> bool {
        self.tabs == other.tabs
    }
}

impl Eq for Group {}

impl Group {
    pub fn of(tab: Tab) -> Self {
        Self {
            tabs: vec![tab],
            current: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Layout {
    Group(Group),
    Split { split: Side, children: Vec<Layout> },
}

impl Default for Layout {
    fn default() -> Self {
        Layout::Group(Group::default())
    }
}

impl Layout {
    /// Every group, left to right and top to bottom: a group's index is its place here.
    pub fn groups(&self) -> Vec<&Group> {
        match self {
            Layout::Group(group) => vec![group],
            Layout::Split { children, .. } => children.iter().flat_map(Layout::groups).collect(),
        }
    }

    pub fn group_mut(&mut self, index: usize) -> Option<&mut Group> {
        match self {
            Layout::Group(group) => (index == 0).then_some(group),
            Layout::Split { children, .. } => {
                let mut index = index;
                for child in children {
                    let count = child.groups().len();
                    if index < count {
                        return child.group_mut(index);
                    }
                    index -= count;
                }
                None
            }
        }
    }

    /// Where a pane key already shows: (group, tab).
    pub fn find(&self, key: &str) -> Option<(usize, usize)> {
        self.groups().iter().enumerate().find_map(|(index, group)| {
            group
                .tabs
                .iter()
                .position(|tab| tab.pane == key)
                .map(|tab| (index, tab))
        })
    }

    /// The node holding group `index`, mutably, to replace it.
    fn node_mut(&mut self, index: usize) -> Option<&mut Layout> {
        match self {
            Layout::Group(_) => (index == 0).then_some(self),
            Layout::Split { children, .. } => {
                let mut index = index;
                for child in children {
                    let count = child.groups().len();
                    if index < count {
                        return child.node_mut(index);
                    }
                    index -= count;
                }
                None
            }
        }
    }

    /// Split group `index`, putting `group` on `side` of it. Returns the new group's index.
    pub fn split(&mut self, index: usize, side: Side, group: Group) -> usize {
        if let Some(node) = self.node_mut(index) {
            let old = std::mem::take(node);
            *node = Layout::Split {
                split: side,
                children: vec![old, Layout::Group(group)],
            };
        }
        index + 1
    }

    /// The layout without group `index`; `None` when that was its only group. A split left
    /// with one child becomes that child.
    pub fn remove(self, index: usize) -> Option<Layout> {
        match self {
            Layout::Group(_) => (index != 0).then_some(self),
            Layout::Split { split, children } => {
                let mut index = index;
                let mut kept = Vec::new();
                for child in children {
                    let count = child.groups().len();
                    if index < count {
                        kept.extend(child.remove(index));
                        index = usize::MAX;
                    } else {
                        index = index.saturating_sub(count);
                        kept.push(child);
                    }
                }
                match kept.len() {
                    0 => None,
                    1 => kept.pop(),
                    _ => Some(Layout::Split {
                        split,
                        children: kept,
                    }),
                }
            }
        }
    }

    /// Where each group goes in `area`, in group order, and the one-cell dividers between them.
    pub fn rects(&self, area: Rect) -> (Vec<Rect>, Vec<(Rect, Side)>) {
        let mut groups = Vec::new();
        let mut dividers = Vec::new();
        self.place(area, &mut groups, &mut dividers);
        (groups, dividers)
    }

    fn place(&self, area: Rect, groups: &mut Vec<Rect>, dividers: &mut Vec<(Rect, Side)>) {
        match self {
            Layout::Group(_) => groups.push(area),
            Layout::Split { split, children } => {
                let count = children.len().max(1) as u16;
                let (total, start) = match split {
                    Side::Right => (area.width, area.x),
                    Side::Below => (area.height, area.y),
                };
                let each = total.saturating_sub(count - 1) / count;
                let mut at = start;
                for (index, child) in children.iter().enumerate() {
                    let last = index + 1 == children.len();
                    let size = if last {
                        (start + total).saturating_sub(at)
                    } else {
                        each
                    };
                    let rect = match split {
                        Side::Right => Rect {
                            x: at,
                            width: size,
                            ..area
                        },
                        Side::Below => Rect {
                            y: at,
                            height: size,
                            ..area
                        },
                    };
                    child.place(rect, groups, dividers);
                    at += size;
                    if !last {
                        let divider = match split {
                            Side::Right => Rect {
                                x: at,
                                width: 1,
                                ..area
                            },
                            Side::Below => Rect {
                                y: at,
                                height: 1,
                                ..area
                            },
                        };
                        dividers.push((divider, *split));
                        at += 1;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tabs(layout: &Layout) -> Vec<Vec<&str>> {
        layout
            .groups()
            .iter()
            .map(|group| group.tabs.iter().map(|tab| tab.pane.as_str()).collect())
            .collect()
    }

    #[test]
    fn splitting_and_closing_keep_group_order_and_collapse_lone_children() {
        let mut layout = Layout::Group(Group::of(Tab::pane("agent:a")));
        assert_eq!(
            layout.split(0, Side::Right, Group::of(Tab::pane("mission:m"))),
            1
        );
        assert_eq!(
            layout.split(1, Side::Below, Group::of(Tab::pane("machine:h"))),
            2
        );
        layout.group_mut(1).unwrap().tabs.push(Tab::pane("agent:b"));
        assert_eq!(
            tabs(&layout),
            [
                vec!["agent:a"],
                vec!["mission:m", "agent:b"],
                vec!["machine:h"]
            ]
        );
        assert_eq!(layout.find("agent:b"), Some((1, 1)));

        let layout = layout.remove(1).unwrap();
        assert_eq!(tabs(&layout), [vec!["agent:a"], vec!["machine:h"]]);
        assert!(
            matches!(&layout, Layout::Split { split: Side::Right, children } if children.len() == 2)
        );
        let layout = layout.remove(0).unwrap();
        assert_eq!(tabs(&layout), [vec!["machine:h"]]);
        assert_eq!(layout.remove(0), None);
    }

    #[test]
    fn rects_fill_the_area_with_one_cell_dividers() {
        let mut layout = Layout::default();
        layout.split(0, Side::Right, Group::default());
        layout.split(1, Side::Below, Group::default());
        let (groups, dividers) = layout.rects(Rect::new(0, 0, 81, 21));
        assert_eq!(groups[0], Rect::new(0, 0, 40, 21));
        assert_eq!(dividers[0], (Rect::new(40, 0, 1, 21), Side::Right));
        assert_eq!(groups[1], Rect::new(41, 0, 40, 10));
        assert_eq!(dividers[1], (Rect::new(41, 10, 40, 1), Side::Below));
        assert_eq!(groups[2], Rect::new(41, 11, 40, 10));
    }

    #[test]
    fn a_layout_is_stored_as_the_wire_shape_without_which_tab_shows() {
        let mut layout = Layout::Group(Group::of(Tab::pane("agent:agent/example/harbor")));
        layout.split(
            0,
            Side::Below,
            Group {
                tabs: vec![Tab {
                    title: Some("audit".into()),
                    pane: "mission:mission/example/audit".into(),
                }],
                current: 0,
            },
        );
        layout.group_mut(0).unwrap().current = 3;
        let json = serde_json::to_value(&layout).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"split": "below", "children": [
                {"tabs": [{"pane": "agent:agent/example/harbor"}]},
                {"tabs": [{"title": "audit", "pane": "mission:mission/example/audit"}]}
            ]})
        );
        assert_eq!(serde_json::from_value::<Layout>(json).unwrap(), layout);
        assert_eq!(
            serde_json::from_value::<Layout>(serde_json::json!({"tabs": []})).unwrap(),
            Layout::default()
        );
    }
}
