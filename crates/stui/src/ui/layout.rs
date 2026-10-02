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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Layout {
    Group(Group),
    Split {
        split: Side,
        children: Vec<Layout>,
        /// The first child's share of a two-way split, when a border was dragged; equal
        /// otherwise. Kept on this device; the graph's glass does not carry it yet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ratio: Option<f32>,
    },
}

/// A border between two splits, as drawn: which split node it divides (counted in order, the
/// whole layout first), its direction, and the area that split shares out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Divider {
    pub rect: Rect,
    pub side: Side,
    pub node: usize,
    pub area: Rect,
}

/// Layouts are equal by their structure: how a border was dragged here is this device's.
impl PartialEq for Layout {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Layout::Group(a), Layout::Group(b)) => a == b,
            (
                Layout::Split {
                    split: a,
                    children: c,
                    ..
                },
                Layout::Split {
                    split: b,
                    children: d,
                    ..
                },
            ) => a == b && c == d,
            _ => false,
        }
    }
}

impl Eq for Layout {}

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
                ratio: None,
            };
        }
        index + 1
    }

    /// Split group `index`, putting `group` before it: to its left (`Right` splits) or above it
    /// (`Below`). Returns the new group's index, which is `index`; the old group moves after it.
    pub fn split_first(&mut self, index: usize, side: Side, group: Group) -> usize {
        if let Some(node) = self.node_mut(index) {
            let old = std::mem::take(node);
            *node = Layout::Split {
                split: side,
                children: vec![Layout::Group(group), old],
                ratio: None,
            };
        }
        index
    }

    /// The layout without group `index`; `None` when that was its only group. A split left
    /// with one child becomes that child.
    pub fn remove(self, index: usize) -> Option<Layout> {
        match self {
            Layout::Group(_) => (index != 0).then_some(self),
            Layout::Split {
                split,
                children,
                ratio,
            } => {
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
                        ratio: ratio.filter(|_| kept.len() == 2),
                        children: kept,
                    }),
                }
            }
        }
    }

    /// Where each group goes in `area`, in group order, and the one-cell dividers between them.
    pub fn rects(&self, area: Rect) -> (Vec<Rect>, Vec<(Rect, Side)>) {
        let (groups, dividers) = self.layout_in(area);
        (
            groups,
            dividers
                .into_iter()
                .map(|divider| (divider.rect, divider.side))
                .collect(),
        )
    }

    /// Where each group goes in `area`, and every border with the split it divides.
    pub fn layout_in(&self, area: Rect) -> (Vec<Rect>, Vec<Divider>) {
        let mut groups = Vec::new();
        let mut dividers = Vec::new();
        let mut nodes = 0;
        self.place(area, &mut groups, &mut dividers, &mut nodes);
        (groups, dividers)
    }

    fn place(
        &self,
        area: Rect,
        groups: &mut Vec<Rect>,
        dividers: &mut Vec<Divider>,
        nodes: &mut usize,
    ) {
        match self {
            Layout::Group(_) => groups.push(area),
            Layout::Split {
                split,
                children,
                ratio,
            } => {
                let node = *nodes;
                *nodes += 1;
                let count = children.len().max(1) as u16;
                let (total, start) = match split {
                    Side::Right => (area.width, area.x),
                    Side::Below => (area.height, area.y),
                };
                let room = total.saturating_sub(count - 1);
                let each = room / count;
                // A dragged border sets the first of two children's share.
                let first = match ratio {
                    Some(ratio) if children.len() == 2 && room >= 2 => {
                        ((f32::from(room) * ratio).round() as u16).clamp(1, room - 1)
                    }
                    _ => each,
                };
                let mut at = start;
                for (index, child) in children.iter().enumerate() {
                    let last = index + 1 == children.len();
                    let size = if last {
                        (start + total).saturating_sub(at)
                    } else if index == 0 {
                        first
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
                    child.place(rect, groups, dividers, nodes);
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
                        dividers.push(Divider {
                            rect: divider,
                            side: *split,
                            node,
                            area,
                        });
                        at += 1;
                    }
                }
            }
        }
    }

    /// Set (or, with `None`, clear) the share of split node `node`, counted as `layout_in`
    /// counts them.
    pub fn set_ratio(&mut self, node: usize, value: Option<f32>) {
        fn walk(layout: &mut Layout, node: usize, counter: &mut usize, value: Option<f32>) -> bool {
            let Layout::Split {
                children, ratio, ..
            } = layout
            else {
                return false;
            };
            if *counter == node {
                *ratio = value.map(|value| value.clamp(0.1, 0.9));
                return true;
            }
            *counter += 1;
            children
                .iter_mut()
                .any(|child| walk(child, node, counter, value))
        }
        walk(self, node, &mut 0, value);
    }

    /// Carry the shares of `old`'s splits into this layout wherever its shape is the same, so a
    /// border dragged here survives a glass update that changed something else.
    pub fn carry_ratios(&mut self, old: &Layout) {
        if let (
            Layout::Split {
                split,
                children,
                ratio,
            },
            Layout::Split {
                split: old_split,
                children: old_children,
                ratio: old_ratio,
            },
        ) = (self, old)
            && split == old_split
            && children.len() == old_children.len()
        {
            if ratio.is_none() {
                *ratio = *old_ratio;
            }
            for (child, old) in children.iter_mut().zip(old_children) {
                child.carry_ratios(old);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dragged_share_sizes_the_split_and_survives_an_update_of_the_same_shape() {
        let mut layout = Layout::Group(Group::of(Tab::pane("agent:a")));
        layout.split(0, Side::Right, Group::of(Tab::pane("mission:m")));
        let area = Rect::new(0, 0, 101, 10);
        layout.set_ratio(0, Some(0.25));
        let (groups, dividers) = layout.layout_in(area);
        assert_eq!(groups[0].width, 25);
        assert_eq!(dividers[0].node, 0);
        assert_eq!(dividers[0].area, area);
        layout.set_ratio(0, Some(0.01));
        assert_eq!(
            layout.layout_in(area).0[0].width,
            10,
            "a share is kept within 10–90%"
        );
        // An update from st with the same shape keeps it; equality ignores it.
        let mut update = Layout::Group(Group::of(Tab::pane("agent:a")));
        update.split(0, Side::Right, Group::of(Tab::pane("mission:m")));
        assert_eq!(update, layout);
        update.carry_ratios(&layout);
        assert_eq!(update.layout_in(area).0[0].width, 10);
        // A different shape does not take it.
        let mut other = Layout::Group(Group::of(Tab::pane("agent:a")));
        other.split(0, Side::Below, Group::of(Tab::pane("mission:m")));
        other.carry_ratios(&layout);
        assert_eq!(
            other.layout_in(area).0[0].height,
            4,
            "evenly, past the border"
        );
    }

    #[test]
    fn a_split_can_put_the_new_group_first() {
        let mut layout = Layout::Group(Group::of(Tab::pane("agent:a")));
        assert_eq!(
            layout.split_first(0, Side::Right, Group::of(Tab::pane("mission:m"))),
            0
        );
        let keys = |layout: &Layout| {
            layout
                .groups()
                .iter()
                .map(|group| group.tabs[0].pane.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&layout), ["mission:m", "agent:a"]);
        layout.split_first(1, Side::Below, Group::of(Tab::pane("machine:h")));
        assert_eq!(keys(&layout), ["mission:m", "machine:h", "agent:a"]);
    }

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
            matches!(&layout, Layout::Split { split: Side::Right, children, .. } if children.len() == 2)
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
