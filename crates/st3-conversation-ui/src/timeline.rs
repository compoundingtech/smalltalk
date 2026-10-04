use st3_client::{TimelineBody, TimelineEntry};

fn is_projection_notice(entry: &TimelineEntry) -> bool {
    matches!(&entry.body, TimelineBody::Error(error)
        if matches!(error.code.as_str(),
            "timeline-query-limited" | "timeline-history-incomplete"))
}

/// Metadata and entries from a native conversation frame, preserved at the UI boundary.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub replace: bool,
    pub has_more: bool,
    pub items: Vec<TimelineEntry>,
    /// The session the entries belong to, when the stream says.
    pub session_id: Option<String>,
}

#[derive(Default)]
pub struct Timeline {
    pub items: Vec<TimelineEntry>,
    /// Whether the most recently received frame replaced the bounded window.
    pub replace: bool,
    pub has_more: bool,
    pub session_id: Option<String>,
    /// Earlier pages read from st's session timeline while the person scrolled back.
    pub older: Older,
}

/// Reading back past the live window, one page of st's session timeline at a time.
#[derive(Clone, Debug, Default)]
pub struct Older {
    /// At least one earlier page was read: these entries are not in the live window.
    pub paged: bool,
    /// st said there is nothing before the oldest entry held: the session's start.
    pub start: bool,
    /// st's cursor for the page before the oldest one read, and when it was read.
    pub cursor: Option<(String, std::time::Instant)>,
    pub loading: bool,
    /// Why the last page could not be read; scrolling up tries again.
    pub failed: Option<String>,
}

impl Timeline {
    pub fn apply(&mut self, frame: Frame) {
        self.replace = frame.replace;
        self.has_more = frame.has_more;
        let other_session = frame.session_id.is_some()
            && self.session_id.is_some()
            && frame.session_id != self.session_id;
        if frame.session_id.is_some() {
            self.session_id = frame.session_id;
        }
        if other_session {
            self.older = Older::default();
        }
        if frame.replace {
            // A new window keeps the pages read before it, unless it no longer meets them:
            // entries could be missing between the two, and a gap must never look whole.
            // Session-stable availability notices cannot establish history continuity.
            let meets = frame.items.iter().any(|item| {
                !is_projection_notice(item)
                    && self.items.iter().any(|held| {
                        !is_projection_notice(held) && held.id == item.id
                    })
            });
            let oldest = frame
                .items
                .iter()
                .find(|item| !is_projection_notice(item))
                .map(|first| (first.timestamp.as_str(), first.sequence));
            if self.older.paged && !other_session && (meets || !frame.has_more) {
                let mut items = std::mem::take(&mut self.items);
                items.retain(|held| {
                    !is_projection_notice(held)
                        && oldest.is_some_and(|(at, sequence)| {
                            (held.timestamp.as_str(), held.sequence) < (at, sequence)
                        })
                });
                items.extend(frame.items);
                self.items = items;
            } else {
                self.older = Older::default();
                self.items = frame.items;
            }
        } else {
            self.merge(frame.items, true);
        }
    }

    /// An earlier page of `session_id`'s timeline, newest entries already held win. A page for
    /// another session (the agent restarted meanwhile) is dropped.
    pub fn older_page(
        &mut self,
        session_id: &str,
        items: Vec<TimelineEntry>,
        has_more: bool,
        cursor: Option<String>,
    ) {
        self.older.loading = false;
        if self.session_id.as_deref() != Some(session_id) {
            return;
        }
        self.older.failed = None;
        self.older.paged = true;
        self.older.start = !has_more;
        self.older.cursor = cursor.map(|cursor| (cursor, std::time::Instant::now()));
        self.merge(items, false);
    }

    pub fn older_failed(&mut self, reason: String) {
        self.older.loading = false;
        self.older.failed = Some(reason);
    }

    /// Whether st holds entries before the oldest one shown.
    pub fn more_before(&self) -> bool {
        if self.older.paged {
            !self.older.start
        } else {
            self.has_more
        }
    }

    fn merge(&mut self, items: Vec<TimelineEntry>, revise: bool) {
        for item in items {
            match self.items.iter_mut().find(|entry| entry.id == item.id) {
                Some(entry) if revise => *entry = item,
                Some(_) => {}
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
