//! A glass tab's panes: a tree of splits whose leaves are pane keys. This is the shape a glass
//! is stored in, on this device now and in the graph later, so it holds structure only: which
//! panes, side by side or one below the other. Sizes, focus and scroll stay with each window.

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Layout {
    Pane { pane: String },
    Split { split: Side, children: Vec<Layout> },
}

impl Layout {
    pub fn pane(key: impl Into<String>) -> Self {
        Layout::Pane { pane: key.into() }
    }

    /// Every pane key, left to right and top to bottom: a leaf's index is its place here.
    pub fn leaves(&self) -> Vec<&str> {
        match self {
            Layout::Pane { pane } => vec![pane.as_str()],
            Layout::Split { children, .. } => children.iter().flat_map(Layout::leaves).collect(),
        }
    }

    fn count(&self) -> usize {
        self.leaves().len()
    }

    /// The leaf at `index`, mutably.
    fn leaf_mut(&mut self, index: usize) -> Option<&mut Layout> {
        match self {
            Layout::Pane { .. } => (index == 0).then_some(self),
            Layout::Split { children, .. } => {
                let mut index = index;
                for child in children {
                    let count = child.count();
                    if index < count {
                        return child.leaf_mut(index);
                    }
                    index -= count;
                }
                None
            }
        }
    }

    /// Show `key` in leaf `index` instead of what it shows.
    pub fn replace(&mut self, index: usize, key: &str) {
        if let Some(leaf) = self.leaf_mut(index) {
            *leaf = Layout::pane(key);
        }
    }

    /// Split leaf `index`, putting `key` on `side` of it. Returns the new leaf's index.
    pub fn split(&mut self, index: usize, side: Side, key: &str) -> usize {
        if let Some(leaf) = self.leaf_mut(index) {
            let old = std::mem::replace(leaf, Layout::pane(""));
            *leaf = Layout::Split {
                split: side,
                children: vec![old, Layout::pane(key)],
            };
        }
        index + 1
    }

    /// The layout without leaf `index`; `None` when that was its only pane. A split left with
    /// one child becomes that child.
    pub fn remove(self, index: usize) -> Option<Layout> {
        match self {
            Layout::Pane { .. } => (index != 0).then_some(self),
            Layout::Split { split, children } => {
                let mut index = index;
                let mut kept = Vec::new();
                for child in children {
                    let count = child.count();
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

    /// Where each leaf goes in `area`, in leaf order, and the one-cell dividers between them.
    pub fn rects(&self, area: Rect) -> (Vec<Rect>, Vec<(Rect, Side)>) {
        let mut leaves = Vec::new();
        let mut dividers = Vec::new();
        self.place(area, &mut leaves, &mut dividers);
        (leaves, dividers)
    }

    fn place(&self, area: Rect, leaves: &mut Vec<Rect>, dividers: &mut Vec<(Rect, Side)>) {
        match self {
            Layout::Pane { .. } => leaves.push(area),
            Layout::Split { split, children } => {
                let count = children.len().max(1) as u16;
                let (total, start) = match split {
                    Side::Right => (area.width, area.x),
                    Side::Below => (area.height, area.y),
                };
                let gaps = count - 1;
                let each = total.saturating_sub(gaps) / count;
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
                    child.place(rect, leaves, dividers);
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

    #[test]
    fn splitting_and_closing_keep_leaf_order_and_collapse_lone_children() {
        let mut layout = Layout::pane("agent:a");
        assert_eq!(layout.split(0, Side::Right, "mission:m"), 1);
        assert_eq!(layout.split(1, Side::Below, "machine:h"), 2);
        assert_eq!(layout.leaves(), ["agent:a", "mission:m", "machine:h"]);
        layout.replace(2, "machine:g");
        assert_eq!(layout.leaves(), ["agent:a", "mission:m", "machine:g"]);

        let layout = layout.remove(1).unwrap();
        assert_eq!(layout.leaves(), ["agent:a", "machine:g"]);
        // The below-split lost a child, so it is gone: one split remains.
        assert_eq!(
            layout,
            Layout::Split {
                split: Side::Right,
                children: vec![Layout::pane("agent:a"), Layout::pane("machine:g")],
            }
        );
        let layout = layout.remove(0).unwrap();
        assert_eq!(layout, Layout::pane("machine:g"));
        assert_eq!(layout.remove(0), None);
    }

    #[test]
    fn rects_fill_the_area_with_one_cell_dividers() {
        let mut layout = Layout::pane("a");
        layout.split(0, Side::Right, "b");
        layout.split(1, Side::Below, "c");
        let area = Rect::new(0, 0, 81, 21);
        let (leaves, dividers) = layout.rects(area);
        assert_eq!(leaves[0], Rect::new(0, 0, 40, 21));
        assert_eq!(dividers[0], (Rect::new(40, 0, 1, 21), Side::Right));
        assert_eq!(leaves[1], Rect::new(41, 0, 40, 10));
        assert_eq!(dividers[1], (Rect::new(41, 10, 40, 1), Side::Below));
        assert_eq!(leaves[2], Rect::new(41, 11, 40, 10));
    }

    #[test]
    fn a_layout_is_stored_as_plain_json_structure() {
        let mut layout = Layout::pane("agent:agent/example/harbor");
        layout.split(0, Side::Below, "mission:mission/example/audit");
        let json = serde_json::to_value(&layout).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"split": "below", "children": [
                {"pane": "agent:agent/example/harbor"},
                {"pane": "mission:mission/example/audit"}
            ]})
        );
        assert_eq!(serde_json::from_value::<Layout>(json).unwrap(), layout);
    }
}
