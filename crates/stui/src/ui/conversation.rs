//! Translate the shared renderer's pane intents into stui's shell hit map.

use super::doc::{Doc, Hit, Target};
use super::theme;
use super::view::Entry;
use std::collections::HashSet;

#[derive(Default)]
pub struct Cache(st3_conversation_ui::Cache);

impl Cache {
    pub fn render(
        &self,
        entries: &[Entry],
        width: usize,
        expanded: &HashSet<String>,
        spinner: &str,
    ) -> Doc {
        let rendered = self
            .0
            .render(entries, width, expanded, spinner, &theme::conversation());
        Doc {
            lines: rendered.lines,
            messages: rendered.messages,
            targets: rendered
                .targets
                .into_iter()
                .map(|target| Target {
                    line: target.line,
                    column: target.column,
                    width: target.width,
                    hit: Hit::Pane(target.hit),
                })
                .collect(),
        }
    }
}
