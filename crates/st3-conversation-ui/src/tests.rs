use crate::*;
use ratatui::style::Color;
use ratatui::text::Line;
use std::collections::HashSet;

pub(crate) fn theme() -> Theme {
    Theme {
        user_bg: Color::Black,
        tool_bg: Color::Black,
        tool_ok_bg: Color::Black,
        tool_err_bg: Color::Black,
        overlay0: Color::DarkGray,
        overlay1: Color::Gray,
        text: Color::White,
        subtext0: Color::Gray,
        subtext1: Color::White,
        working: Color::Yellow,
        green: Color::Green,
        red: Color::Red,
        sapphire: Color::Cyan,
        surface1: Color::DarkGray,
        surface2: Color::Gray,
        teal: Color::Cyan,
        blue: Color::Blue,
        peach: Color::Yellow,
        lavender: Color::Magenta,
    }
}

fn item(id: &str, sequence: u64, revision: u64, text: &str) -> st3_client::TimelineEntry {
    serde_json::from_value(serde_json::json!({
        "id": id, "sequence": sequence, "revision": revision,
        "timestamp": "2026-09-30T10:00:00Z", "role": "assistant", "final": true,
        "type": "content", "body": {"media_type": "text/plain", "text": text}
    }))
    .unwrap()
}

#[test]
fn frame_updates_revise_by_id_and_replacements_remove_old_history() {
    let mut timeline = Timeline::default();
    timeline.apply(Frame {
        replace: true,
        has_more: true,
        items: vec![item("a", 1, 1, "old")],
    });
    assert!(timeline.replace && timeline.has_more);
    timeline.apply(Frame {
        replace: false,
        has_more: true,
        items: vec![item("b", 2, 1, "next"), item("a", 1, 2, "revised")],
    });
    assert!(!timeline.replace && timeline.has_more);
    let entries = adapt::conversation(&timeline.items, &Default::default());
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(matches!(&entries[0].body, Body::Assistant(text) if text == "revised"));
    timeline.apply(Frame {
        replace: true,
        has_more: false,
        items: vec![],
    });
    assert!(timeline.replace && !timeline.has_more);
    assert!(timeline.items.is_empty());
}

#[test]
fn theme_and_timestamp_changes_invalidate_cached_entry_styles_and_labels() {
    let cache = Cache::default();
    let mut entries = vec![Entry {
        id: "user".into(),
        at: "10:00".into(),
        body: Body::User("hi".into()),
    }];
    let palette = theme();
    let first = cache.render(&entries, 30, &HashSet::new(), "*", &palette);
    let changed = Theme {
        user_bg: Color::Blue,
        ..palette
    };
    let styled = cache.render(&entries, 30, &HashSet::new(), "*", &changed);
    assert_eq!(first.lines[0].spans[0].style.bg, Some(Color::Black));
    assert_eq!(styled.lines[0].spans[0].style.bg, Some(Color::Blue));
    entries[0].at = "11:00".into();
    let later = cache.render(&entries, 30, &HashSet::new(), "*", &palette);
    assert!(text::plain(&later.lines[0]).ends_with("11:00 "));
}

#[test]
fn scrolling_pauses_following_and_counts_new_lines_until_latest_is_requested() {
    let mut pane = PaneState {
        follow: true,
        ..Default::default()
    };
    pane.reconcile(20, 5);
    assert_eq!(pane.top, 15);
    pane.scroll(-3, 20, 5, true);
    pane.reconcile(24, 5);
    assert_eq!((pane.top, pane.unseen, pane.follow), (12, 4, false));
    pane.reconcile(24, 20);
    assert_eq!(pane.top, 4);
    pane.follow_latest();
    pane.reconcile(25, 5);
    assert_eq!((pane.top, pane.unseen, pane.follow), (20, 0, true));
}

#[test]
fn selection_uses_display_columns_in_both_drag_directions() {
    let lines = vec![Line::from("a界b"), Line::from("done")];
    let mut selection = Selection {
        pane: "session/a".into(),
        anchor: (0, 1),
        head: (1, 1),
    };
    assert_eq!(selection.text(&lines), "界b\ndo");
    std::mem::swap(&mut selection.anchor, &mut selection.head);
    assert_eq!(selection.text(&lines), "界b\ndo");
    assert_eq!(selection.text(&[]), "");
}

#[test]
fn pane_intents_keep_other_sessions_drafts_and_expansion_independent() {
    let mut state = State::default();
    state
        .drafts
        .insert("session/a".into(), "hello\nworld".into());
    state.drafts.insert("session/b".into(), "  ".into());
    assert_eq!(
        state.send("session/a"),
        Some(PaneIntent::Send("hello\nworld".into()))
    );
    assert_eq!(state.send("session/b"), None);
    assert_eq!(state.drafts["session/a"], "hello\nworld");
    assert_eq!(
        state.expand("tool/a".into()),
        PaneIntent::Expand("tool/a".into())
    );
    state.expand("tool/b".into());
    state.expand("tool/a".into());
    assert!(!state.expanded.contains("tool/a"));
    assert!(state.expanded.contains("tool/b"));
    assert_eq!(state.load_older(true), Some(PaneIntent::LoadOlder));
    assert_eq!(state.load_older(false), None);
}
