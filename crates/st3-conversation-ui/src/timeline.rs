use st3_client::TimelineEntry;

/// Metadata and entries from a native conversation frame, preserved at the UI boundary.
#[derive(Clone, Debug)]
pub struct Frame {
    pub replace: bool,
    pub has_more: bool,
    pub items: Vec<TimelineEntry>,
}

#[derive(Default)]
pub struct Timeline {
    pub items: Vec<TimelineEntry>,
    /// Whether the most recently received frame replaced the bounded window.
    pub replace: bool,
    pub has_more: bool,
}

impl Timeline {
    pub fn apply(&mut self, frame: Frame) {
        self.replace = frame.replace;
        self.has_more = frame.has_more;
        if frame.replace {
            self.items = frame.items;
        } else {
            for item in frame.items {
                match self.items.iter_mut().find(|entry| entry.id == item.id) {
                    Some(entry) => *entry = item,
                    None => self.items.push(item),
                }
            }
            self.items.sort_by(|a, b| {
                a.timestamp
                    .cmp(&b.timestamp)
                    .then(a.sequence.cmp(&b.sequence))
            });
        }
    }
}
