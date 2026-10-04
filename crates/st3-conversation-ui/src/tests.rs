use crate::*;
#[cfg(feature = "ratatui")]
use ratatui::style::Color;
#[cfg(feature = "ratatui")]
use std::collections::HashSet;

#[cfg(feature = "ratatui")]
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
        ..Frame::default()
    });
    assert!(timeline.replace && timeline.has_more);
    timeline.apply(Frame {
        replace: false,
        has_more: true,
        items: vec![item("b", 2, 1, "next"), item("a", 1, 2, "revised")],
        ..Frame::default()
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
        ..Frame::default()
    });
    assert!(timeline.replace && !timeline.has_more);
    assert!(timeline.items.is_empty());
}

fn ids(timeline: &Timeline) -> Vec<&str> {
    timeline
        .items
        .iter()
        .map(|entry| entry.id.as_str())
        .collect()
}

#[test]
fn earlier_pages_go_above_the_window_and_survive_a_new_window_that_meets_them() {
    let window = |items: Vec<st3_client::TimelineEntry>, has_more: bool| Frame {
        replace: true,
        has_more,
        items,
        session_id: Some("session/a".into()),
    };
    let mut timeline = Timeline::default();
    timeline.apply(window(
        vec![item("c", 3, 1, "c"), item("d", 4, 1, "d")],
        true,
    ));
    assert!(timeline.more_before());
    timeline.older.loading = true;
    // The page overlaps what is held; the held (newer) revision stays.
    timeline.older_page(
        "session/a",
        vec![item("b", 2, 1, "b"), item("c", 3, 0, "stale")],
        true,
        Some("cursor-1".into()),
    );
    assert_eq!(ids(&timeline), ["b", "c", "d"]);
    assert!(timeline.items[1].revision == 1 && !timeline.older.loading);
    assert!(timeline.more_before() && timeline.older.cursor.is_some());
    timeline.older_page("session/a", vec![item("a", 1, 1, "a")], false, None);
    assert!(timeline.older.start && !timeline.more_before());
    // A reconnect's window that meets what is held keeps the earlier pages.
    timeline.apply(window(
        vec![item("d", 4, 1, "d"), item("e", 5, 1, "e")],
        true,
    ));
    assert_eq!(ids(&timeline), ["a", "b", "c", "d", "e"]);
    assert!(timeline.older.start);
    // One that skipped past it would leave a hole, so the earlier pages go.
    timeline.apply(window(vec![item("x", 9, 1, "x")], true));
    assert_eq!(ids(&timeline), ["x"]);
    assert!(!timeline.older.paged && timeline.more_before());
}

#[test]
fn projection_notices_do_not_connect_disconnected_history_windows() {
    let notice = |sequence: u64| -> st3_client::TimelineEntry {
        serde_json::from_value(serde_json::json!({
            "id": "timeline-entry/session/a/timeline-query-limited",
            "sequence": sequence, "revision": 1,
            "timestamp": "2026-09-30T10:00:00Z", "role": "system", "final": true,
            "type": "error", "body": {
                "code": "timeline-query-limited",
                "message": "Older operations are outside this view",
                "retryable": false, "details": {"operation_limit": 4096}
            }
        }))
        .unwrap()
    };
    let window = |items| Frame {
        replace: true,
        has_more: true,
        items,
        session_id: Some("session/a".into()),
    };
    let mut timeline = Timeline::default();
    timeline.apply(window(vec![
        item("c", 3, 1, "c"),
        item("d", 4, 1, "d"),
        notice(6),
    ]));
    timeline.older_page(
        "session/a",
        vec![item("a", 1, 1, "a"), item("b", 2, 1, "b")],
        false,
        None,
    );
    timeline.apply(window(vec![
        item("d", 4, 1, "d"),
        item("e", 5, 1, "e"),
        notice(6),
    ]));
    assert_eq!(
        ids(&timeline),
        [
            "a",
            "b",
            "c",
            "d",
            "e",
            "timeline-entry/session/a/timeline-query-limited"
        ]
    );
    assert!(timeline.older.paged);
    timeline.apply(window(vec![item("x", 9, 1, "x"), notice(10)]));
    assert_eq!(
        ids(&timeline),
        ["x", "timeline-entry/session/a/timeline-query-limited"]
    );
    assert!(!timeline.older.paged && timeline.more_before());
}

#[test]
fn an_earlier_page_from_another_session_is_dropped_and_a_new_session_starts_over() {
    let mut timeline = Timeline::default();
    timeline.apply(Frame {
        replace: true,
        has_more: true,
        items: vec![item("c", 3, 1, "c")],
        session_id: Some("session/a".into()),
    });
    timeline.older_page("session/b", vec![item("b", 2, 1, "b")], true, None);
    assert_eq!(ids(&timeline), ["c"]);
    assert!(!timeline.older.paged);
    timeline.older_page("session/a", vec![item("b", 2, 1, "b")], true, None);
    timeline.older_failed("st did not answer".into());
    assert_eq!(timeline.older.failed.as_deref(), Some("st did not answer"));
    timeline.apply(Frame {
        replace: true,
        has_more: false,
        items: vec![item("n", 1, 1, "n")],
        session_id: Some("session/b".into()),
    });
    assert_eq!(ids(&timeline), ["n"]);
    assert!(!timeline.older.paged && timeline.older.failed.is_none());
}

#[test]
#[cfg(feature = "ratatui")]
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
    let lines = vec!["a界b", "done"];
    let mut selection = Selection {
        pane: "session/a".into(),
        anchor: (0, 1),
        head: (1, 1),
    };
    assert_eq!(selection.text(&lines), "界b\ndo");
    std::mem::swap(&mut selection.anchor, &mut selection.head);
    assert_eq!(selection.text(&lines), "界b\ndo");
    assert_eq!(selection.text::<&str>(&[]), "");
}

#[test]
fn selection_copies_text_without_the_message_edge_indent_or_fences() {
    // Nathan, 2026-10-03: a command copied out of a message came with the "▎" edge on every
    // line.
    let lines = vec![
        "▎ you · 10:02",
        "▎ Run this:",
        "▎ ```sh",
        "▎   mkdir -p ~/.config/demo",
        "▎     touch ~/.config/demo/keys.env",
        "▎ ```",
    ];
    let mut selection = Selection {
        pane: "session/a".into(),
        anchor: (2, 0),
        head: (5, 40),
    };
    assert_eq!(
        selection.text(&lines),
        "mkdir -p ~/.config/demo\n  touch ~/.config/demo/keys.env"
    );
    // Starting inside the edge copies from the text; starting past it copies from there.
    selection.anchor = (1, 1);
    selection.head = (1, 40);
    assert_eq!(selection.text(&lines), "Run this:");
    selection.anchor = (1, 6);
    assert_eq!(selection.text(&lines), "this:");
    // Text that only looks like an edge mid-line stays.
    let plain = vec!["a ▎ b"];
    selection.anchor = (0, 0);
    selection.head = (0, 9);
    assert_eq!(selection.text(&plain), "a ▎ b");
}

#[test]
#[cfg(feature = "ratatui")]
fn copying_wrapped_lines_gives_back_only_the_real_newlines() {
    // Nathan, 2026-10-03: a line the pane wrapped is one line on the clipboard.
    let body = "first paragraph is long enough to wrap across several pane lines\nsecond line\n\nhttps://example.com/a/really/long/path/that/cannot/break/at/a/space";
    let entries = vec![Entry {
        id: "m".into(),
        at: "10:00".into(),
        body: Body::Mail {
            from: "you".into(),
            to: "agent".into(),
            subject: String::new(),
            body: body.into(),
            delivered: false,
            dictated: false,
            images: Vec::new(),
        },
    }];
    let doc = Cache::default().render(&entries, 30, &HashSet::new(), "", &theme());
    assert!(doc.lines.len() > 8, "the body wraps at this width");
    // From the body's first line to the end.
    let first = doc
        .lines
        .iter()
        .position(|line| text::plain(line).contains("first paragraph"))
        .unwrap();
    let selection = Selection {
        pane: "session/a".into(),
        anchor: (first, 0),
        head: (doc.lines.len() - 1, 200),
    };
    assert_eq!(selection.text(&doc.lines).trim_end(), body);
}

#[test]
#[cfg(feature = "ratatui")]
fn mail_to_the_person_leads_with_a_bullet_and_their_own_keeps_the_bar() {
    // Nathan, 2026-10-05: mail to me and from me looked the same.
    let mail = |from: &str, to: &str| Entry {
        id: format!("message/{from}-{to}"),
        at: "10:00".into(),
        body: Body::Mail {
            from: from.into(),
            to: to.into(),
            subject: String::new(),
            body: "hello\nthere".into(),
            delivered: false,
            dictated: false,
            images: Vec::new(),
        },
    };
    let lines = |entry: Entry| {
        let doc = Cache::default().render(&[entry], 60, &HashSet::new(), "", &theme());
        doc.lines.iter().map(text::plain).collect::<Vec<_>>()
    };
    let to_you = lines(mail("agent", "you"));
    assert!(to_you[0].starts_with("● agent → you"), "{to_you:?}");
    assert!(to_you[1..].iter().all(|line| !line.starts_with('●')), "{to_you:?}");
    assert!(to_you[1].starts_with("▎ "), "the body keeps the bar: {to_you:?}");
    let from_you = lines(mail("you", "agent"));
    assert!(from_you[0].starts_with("▎ you → agent"), "{from_you:?}");
    // Copying from the bullet's row leaves the bullet out, as it does the bar.
    let selection = Selection {
        pane: "session/a".into(),
        anchor: (0, 0),
        head: (to_you.len() - 1, 200),
    };
    let doc = Cache::default().render(&[mail("agent", "you")], 60, &HashSet::new(), "", &theme());
    let copied = selection.text(&doc.lines);
    assert!(!copied.contains('●') && !copied.contains('▎'), "{copied}");
    assert!(copied.contains("hello"), "{copied}");
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

#[test]
fn selection_joins_wrapped_lines_from_another_renderer() {
    struct Line<'a>(&'a str, Option<&'a str>);
    impl SelectionLine for Line<'_> {
        fn plain_text(&self) -> std::borrow::Cow<'_, str> {
            std::borrow::Cow::Borrowed(self.0)
        }

        fn continuation(&self) -> Option<&str> {
            self.1
        }
    }
    let lines = [
        Line("▎ a long", None),
        Line("▎ paragraph", Some(" ")),
        Line("▎ https://example.com/", None),
        Line("▎ path", Some("")),
        Line("▎ ```sh", None),
        Line("▎   echo hello", None),
        Line("▎ ```", None),
    ];
    let mut selection = Selection {
        pane: "session/a".into(),
        anchor: (0, 0),
        head: (lines.len() - 1, 80),
    };
    assert_eq!(
        selection.text(&lines),
        "a long paragraph\nhttps://example.com/path\n  echo hello"
    );
    assert_eq!(selection.text(&[String::from("plain text")]), "plain text");
    // A selection that starts in a continuation must not join text outside the selection.
    selection.anchor = (1, 4);
    selection.head = (1, 8);
    assert_eq!(selection.text(&lines), "ragra");
}

#[test]
fn shared_transcripts_match_without_a_renderer() {
    for (input, expected) in [
        (
            include_str!("../../../fixtures/clients/transcripts/claude.json"),
            include_str!("../../../fixtures/clients/transcripts/claude.expected.json"),
        ),
        (
            include_str!("../../../fixtures/clients/transcripts/codex.json"),
            include_str!("../../../fixtures/clients/transcripts/codex.expected.json"),
        ),
        (
            include_str!("../../../fixtures/clients/transcripts/deliveries.json"),
            include_str!("../../../fixtures/clients/transcripts/deliveries.expected.json"),
        ),
    ] {
        let items = serde_json::from_str::<Vec<st3_client::TimelineEntry>>(input).unwrap();
        let mut entries =
            serde_json::to_value(adapt::conversation(&items, &Default::default())).unwrap();
        // The local display time depends on the host's time zone, not the conversation model.
        for entry in entries.as_array_mut().unwrap() {
            entry.as_object_mut().unwrap().remove("at");
        }
        assert_eq!(
            entries,
            serde_json::from_str::<serde_json::Value>(expected).unwrap()
        );
    }
}

#[test]
fn shared_style_rules_are_available_without_a_renderer() {
    assert_eq!(
        serde_json::to_value(style::RULES).unwrap(),
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../../../fixtures/clients/conversation-style.json"
        ))
        .unwrap()
    );
}
