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
        has_more: Some(true),
        items: vec![item("a", 1, 1, "old")],
        ..Frame::default()
    });
    assert!(timeline.replace && timeline.has_more);
    timeline.apply(Frame {
        replace: false,
        has_more: Some(true),
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
        has_more: Some(false),
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
fn delta_preserves_older_history_until_an_older_page_reaches_the_start() {
    let mut timeline = Timeline::default();
    timeline.apply(Frame {
        replace: true,
        has_more: Some(true),
        items: vec![item("c", 3, 1, "c")],
        session_id: Some("session/a".into()),
        header: None,
    });
    timeline.apply(Frame {
        items: vec![item("d", 4, 1, "d")],
        session_id: Some("session/a".into()),
        ..Frame::default()
    });
    assert!(timeline.more_before(), "a delta must not hide older history");
    timeline.older_page("session/a", vec![item("b", 2, 1, "b")], true, Some("older".into()));
    assert_eq!(ids(&timeline), ["b", "c", "d"]);
    assert!(timeline.more_before());
    timeline.apply(Frame {
        items: vec![item("d", 4, 2, "revised")],
        ..Frame::default()
    });
    timeline.older_page("session/a", vec![item("a", 1, 1, "a")], false, None);
    assert_eq!(ids(&timeline), ["a", "b", "c", "d"]);
    assert_eq!(timeline.items[3].revision, 2);
    assert!(!timeline.more_before());
}

#[test]
fn explicit_availability_updates_and_new_windows_do_not_inherit_old_history() {
    let mut timeline = Timeline::default();
    timeline.apply(Frame {
        replace: true,
        has_more: Some(true),
        session_id: Some("session/a".into()),
        ..Frame::default()
    });
    timeline.apply(Frame {
        has_more: Some(false),
        ..Frame::default()
    });
    assert!(!timeline.more_before());
    timeline.apply(Frame {
        has_more: Some(true),
        ..Frame::default()
    });
    assert!(timeline.more_before());
    timeline.apply(Frame {
        replace: true,
        ..Frame::default()
    });
    assert!(!timeline.more_before(), "a replacement without metadata starts fresh");
    timeline.apply(Frame {
        has_more: Some(true),
        ..Frame::default()
    });
    timeline.apply(Frame {
        session_id: Some("session/b".into()),
        ..Frame::default()
    });
    assert!(!timeline.more_before(), "another session cannot inherit availability");
}

#[test]
fn earlier_pages_go_above_the_window_and_survive_a_new_window_that_meets_them() {
    let window = |items: Vec<st3_client::TimelineEntry>, has_more: bool| Frame {
        replace: true,
        has_more: Some(has_more),
        items,
        session_id: Some("session/a".into()),
        header: None,
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
        has_more: Some(true),
        items,
        session_id: Some("session/a".into()),
        header: None,
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
fn projection_notice_is_removed_from_preserved_older_prefix() {
    let notice = |code: &str, timestamp: &str| serde_json::from_value(serde_json::json!({
        "id": format!("timeline-entry/session/a/{code}"),
        "sequence": 0, "revision": 1,
        "timestamp": timestamp, "role": "system", "final": true,
        "type": "error", "body": {
            "code": code,
            "message": "Projection availability changed", "retryable": false,
            "details": {}
        }
    })).unwrap();
    let window = |items| Frame {
        replace: true, has_more: Some(true), items, session_id: Some("session/a".into()),
        header: None,
    };
    let mut timeline = Timeline::default();
    timeline.apply(window(vec![
        notice("timeline-history-incomplete", "2026-09-30T09:00:00Z"),
        item("c", 3, 1, "c"), item("d", 4, 1, "d"),
    ]));
    timeline.older_page(
        "session/a", vec![item("a", 1, 1, "a"), item("b", 2, 1, "b")], false, None,
    );
    timeline.apply(window(vec![item("d", 4, 1, "d"), item("e", 5, 1, "e")]));
    assert_eq!(ids(&timeline), ["a", "b", "c", "d", "e"]);
    assert!(timeline.older.paged);
    timeline.apply(window(vec![
        notice("timeline-query-limited", "2026-09-30T09:30:00Z"),
        item("e", 5, 1, "e"), item("f", 6, 1, "f"),
    ]));
    assert_eq!(ids(&timeline), [
        "a", "b", "c", "d", "timeline-entry/session/a/timeline-query-limited", "e", "f",
    ]);
}

#[test]
fn an_earlier_page_from_another_session_is_dropped_and_a_new_session_starts_over() {
    let mut timeline = Timeline::default();
    timeline.apply(Frame {
        replace: true,
        has_more: Some(true),
        items: vec![item("c", 3, 1, "c")],
        session_id: Some("session/a".into()),
        header: None,
    });
    timeline.older_page("session/b", vec![item("b", 2, 1, "b")], true, None);
    assert_eq!(ids(&timeline), ["c"]);
    assert!(!timeline.older.paged);
    timeline.older_page("session/a", vec![item("b", 2, 1, "b")], true, None);
    timeline.older_failed("st did not answer".into());
    assert_eq!(timeline.older.failed.as_deref(), Some("st did not answer"));
    timeline.apply(Frame {
        replace: true,
        has_more: Some(false),
        items: vec![item("n", 1, 1, "n")],
        session_id: Some("session/b".into()),
        header: None,
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
            signed: None,
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
            signed: None,
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
        (
            include_str!("../../../fixtures/clients/transcripts/native-claude-run.json"),
            include_str!("../../../fixtures/clients/transcripts/native-claude-run.expected.json"),
        ),
        (
            include_str!("../../../fixtures/clients/transcripts/native-omp-run.json"),
            include_str!("../../../fixtures/clients/transcripts/native-omp-run.expected.json"),
        ),
        (
            include_str!("../../../fixtures/clients/transcripts/omp-parity.json"),
            include_str!("../../../fixtures/clients/transcripts/omp-parity.expected.json"),
        ),
    ] {
        // A page also carries its conversation header; the rows are its items.
        let input = serde_json::from_str::<serde_json::Value>(input).unwrap();
        let items = match input {
            serde_json::Value::Object(page) => {
                serde_json::from_value::<Vec<st3_client::TimelineEntry>>(
                    page.get("items").cloned().unwrap_or(serde_json::Value::Null),
                )
            }
            entries => serde_json::from_value::<Vec<st3_client::TimelineEntry>>(entries),
        }
        .unwrap();
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
fn omp_bookkeeping_custom_entries_hide_and_other_custom_entries_stay() {
    let items = serde_json::from_str::<Vec<st3_client::TimelineEntry>>(include_str!(
        "../../../fixtures/clients/transcripts/native-omp-run.json"
    ))
    .unwrap();
    let text = |entries: &[crate::Entry]| serde_json::to_string(entries).unwrap();
    let shown = text(&adapt::conversation(&items, &Default::default()));
    assert!(!shown.contains("unrecognized omp entry"), "{shown}");
    // Shown unfiltered, the same records are there: nothing is dropped from the data.
    let everything = text(&adapt::conversation_with_filters(&items, &Default::default(), crate::SHOW_EVERYTHING));
    assert!(everything.contains("tool_execution_start") && everything.contains("session_exit"));
    // A custom entry of another kind is not bookkeeping and stays.
    let mut other = items.clone();
    for entry in &mut other {
        if let st3_client::TimelineBody::Content(content) = &mut entry.body {
            for block in &mut content.blocks {
                if block.source_type == "custom" {
                    block.payload["raw"]["customType"] = "future_custom".into();
                }
            }
        }
    }
    assert!(text(&adapt::conversation(&other, &Default::default())).contains("unrecognized omp entry"));
}

#[test]
fn omp_parity_header_renders_one_line_with_source_and_age() {
    let page: serde_json::Value = serde_json::from_str(include_str!(
        "../../../fixtures/clients/transcripts/omp-parity.json"
    ))
    .unwrap();
    let line = header::line(&page["header"], "2026-10-06T12:00:00Z");
    assert_eq!(
        line,
        "model synthetic/model · context 50 tokens · cost $0.02 · todo 1/5 · jobs 1 · agents 1 \
         · ask Continue? · working [register · 0s ago] · transcript · 0s ago"
    );
}

#[test]
#[cfg(feature = "ratatui")]
fn a_subagent_card_opens_its_child_conversation() {
    let entries = parity_entries();
    let card = entries
        .iter()
        .find(|entry| entry.id == "parity-18#child")
        .expect("one card per subagent");
    let Body::Tool { title, state, output } = &card.body else {
        panic!("{card:?}");
    };
    assert_eq!((title.as_str(), *state), ("reviewer · completed", ToolState::Ok));
    assert_eq!(
        output.as_slice(),
        [
            "Review synthetic code",
            "duration 1200ms",
            "tokens 80",
            "cost $0.02",
            "open session/child"
        ]
    );
    assert_eq!(header::open_session(output), Some("session/child"));
    let doc = Cache::default().render(
        std::slice::from_ref(card),
        40,
        &HashSet::new(),
        "*",
        &theme(),
    );
    assert!(
        doc.targets
            .iter()
            .any(|target| matches!(&target.hit, PaneIntent::Open(id) if id == "session/child")),
        "{:?}",
        doc.targets
    );
    assert!(
        doc.lines
            .iter()
            .any(|line| text::plain(line).contains("open session/child"))
    );
}

#[test]
#[cfg(feature = "ratatui")]
fn typed_calls_bundle_in_the_simplified_conversation() {
    let entries = parity_entries();
    let doc = Cache::default().render_as(
        &entries,
        60,
        &HashSet::new(),
        "*",
        &theme(),
        Density::Simple,
    );
    let shown = doc
        .lines
        .iter()
        .map(|line| text::plain(line))
        .collect::<Vec<_>>();
    assert!(
        // The thirteen typed calls and the subagent card form one run.
        shown.iter().any(|line| line.contains("14 tool calls")),
        "{shown:?}"
    );
}

/// The omp-parity page's entries, as the shared transcript test reads them.
fn parity_entries() -> Vec<Entry> {
    let page: serde_json::Value = serde_json::from_str(include_str!(
        "../../../fixtures/clients/transcripts/omp-parity.json"
    ))
    .unwrap();
    let items = serde_json::from_value::<Vec<st3_client::TimelineEntry>>(page["items"].clone())
        .unwrap();
    adapt::conversation(&items, &Default::default())
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

#[test]
fn claude_skill_expansion_belongs_to_its_call_and_keeps_the_actual_body() {
    let timeline: Vec<st3_client::TimelineEntry> = serde_json::from_str(include_str!(
        "../../../fixtures/clients/transcripts/claude-skill.json"
    ))
    .unwrap();
    let raw = match &timeline[2].body {
        st3_client::TimelineBody::Content(content) => content.text.as_deref().unwrap(),
        other => panic!("unexpected {other:?}"),
    };
    // A window starting after the call still preserves the entire skill as ordinary text.
    let orphan = adapt::from_harness(true, raw, &Default::default());
    assert!(
        matches!(&orphan[..], [Body::User(body)] if body == raw.trim()),
        "{orphan:?}"
    );
    let entries = adapt::conversation(&timeline, &Default::default());
    assert_eq!(entries.len(), 2, "{entries:?}");
    assert_eq!(entries[0].id, "skill-call");
    assert!(
        matches!(&entries[0].body, Body::Tool { title, state: ToolState::Ok, output }
        if title == "Skill st" && output.join("\n") == raw.trim()),
        "{entries:?}"
    );
}

#[test]
#[cfg(feature = "ratatui")]
fn claude_skill_renders_folded_and_opens_with_its_newlines_and_quoted_tag() {
    let timeline = serde_json::from_str::<Vec<st3_client::TimelineEntry>>(include_str!(
        "../../../fixtures/clients/transcripts/claude-skill.json"
    ))
    .unwrap();
    let entries = adapt::conversation(&timeline, &Default::default());
    let cache = Cache::default();
    let folded = cache.render(&entries, 100, &HashSet::new(), "", &theme());
    assert!(
        !folded
            .lines
            .iter()
            .any(|line| text::plain(line).contains("## Messages"))
    );
    let opened = cache.render(
        &entries,
        100,
        &HashSet::from(["skill-call".into()]),
        "",
        &theme(),
    );
    assert!(
        opened
            .lines
            .iter()
            .all(|line| !text::plain(line).contains('\n'))
    );
    let rendered = opened
        .lines
        .iter()
        .map(text::plain)
        .collect::<Vec<_>>()
        .join("\n");
    for kept in [
        "# st",
        "## Messages",
        "`<smalltalk-message>`, followed by a bounded preview.",
        "it prints nothing, st did not start the session and nothing here applies.",
    ] {
        assert!(rendered.contains(kept), "lost {kept:?}: {rendered}");
    }
}

#[test]
fn the_note_that_earlier_history_is_not_shown_is_at_the_start_not_the_bottom() {
    // Nathan, 2026-10-06: the note showed at the bottom of a conversation, by the message box.
    let entry = |index: u64, kind: &str, body: serde_json::Value, at: &str| -> st3_client::TimelineEntry {
        serde_json::from_value(serde_json::json!({
            "id": format!("entry/{index}"), "sequence": index, "revision": 1,
            "timestamp": at, "role": "assistant", "final": true, "type": kind, "body": body
        }))
        .unwrap()
    };
    let timeline = vec![
        entry(1, "content", serde_json::json!({"media_type":"text/plain","text":"first words"}), "2026-10-05T10:00:00Z"),
        entry(2, "content", serde_json::json!({"media_type":"text/plain","text":"last words"}), "2026-10-05T11:00:00Z"),
        entry(
            0,
            "truncation",
            serde_json::json!({"reason":"the native transcript prefix is outside the bounded read window","omitted_from_sequence":0,"omitted_to_sequence":0}),
            "2026-10-06T09:00:00Z",
        ),
    ];
    let rendered = adapt::conversation(&timeline, &Default::default());
    let shown = rendered
        .iter()
        .map(|entry| serde_json::to_string(&entry.body).unwrap())
        .collect::<Vec<_>>();
    assert!(shown[0].contains("Earlier history is not shown"), "{shown:?}");
    assert!(shown.last().unwrap().contains("last words"), "{shown:?}");
}

#[test]
fn exposed_timeline_variants_media_and_unknown_payloads_are_visible() {
    let bodies = [
        (
            "status",
            serde_json::json!({"status":"waiting","detail":"approval"}),
        ),
        (
            "usage",
            serde_json::json!({"semantics":"response","driver":"omp","input_tokens":12,"cost":0.25,"attribution":{"agent_id":"agent/a","mission_run_id":null,"generation_id":null,"step_id":null}}),
        ),
        (
            "redaction",
            serde_json::json!({"reason":"credential","withheld_bytes":42,"withheld_items":2}),
        ),
        (
            "truncation",
            serde_json::json!({"reason":"window","omitted_from_sequence":1,"omitted_to_sequence":9}),
        ),
        (
            "truncation",
            serde_json::json!({"reason":"the native transcript prefix is outside the bounded read window","omitted_from_sequence":0,"omitted_to_sequence":0}),
        ),
        ("future-secret", serde_json::json!({"secret":"must-not-render"})),
        (
            "content",
            serde_json::json!({"media_type":"image/png","attachment_id":"attachment/safe"}),
        ),
        (
            "error",
            serde_json::json!({"code":"transcript-not-bound","message":"transcript not bound: path unknown","retryable":true,"details":{"not_yet":true}}),
        ),
    ];
    let timeline = bodies
        .iter()
        .enumerate()
        .map(|(index, (kind, body))| {
            serde_json::from_value(serde_json::json!({
                "id":format!("entry/{index}"),"sequence":index,"revision":1,
                "timestamp":"2026-10-05T10:00:00Z","role":"assistant","final":true,
                "type":kind,"body":body
            }))
            .unwrap()
        })
        .collect::<Vec<st3_client::TimelineEntry>>();
    let rendered = adapt::conversation(&timeline, &Default::default());
    let display = serde_json::to_string(&rendered).unwrap();
    for visible in [
        "waiting",
        "approval",
        "credential",
        "42",
        "window",
        "1–9",
        "future-secret",
        "attachment/safe",
        "image/png",
        "transcript unavailable",
        "Earlier history is not shown: st reads only the newest part",
    ] {
        assert!(display.contains(visible), "missing {visible}: {display}");
    }
    assert!(display.contains("must-not-render"));
    assert!(!display.contains("bounded read window"), "{display}");
    assert!(!display.contains("nothing in the harness"));
    assert_eq!(rendered.len(), timeline.len());
}

#[test]
fn attachment_only_content_survives_for_each_native_role() {
    for role in ["user", "system", "assistant", "tool", "future-role"] {
        let entry = serde_json::from_value(serde_json::json!({
            "id":"image","sequence":1,"revision":1,"timestamp":"2026-10-05T10:00:00Z",
            "role":role,"final":true,"type":"content",
            "body":{"media_type":"image/png","attachment_id":"attachment/safe"}
        }))
        .unwrap();
        let rendered = adapt::conversation(&[entry], &Default::default());
        let display = serde_json::to_string(&rendered).unwrap();
        assert!(display.contains("attachment/safe"), "{role}: {display}");
    }
}

#[test]
fn only_structural_delivery_envelopes_split_harness_content() {
    let envelope = "<smalltalk-message id=\"a1\" from=\"person/example\" to=\"agent/example/quay\" subject=\"Keys &amp; locks\" sha256=\"00\" graph=\"message/a1\">\nRotate &lt;all&gt; keys.\n</smalltalk-message>";
    for raw in [
        "A quoted `<smalltalk-message>` stays here.".to_owned(),
        "<smalltalk-message>\nordinary text\n</smalltalk-message>".to_owned(),
        envelope.replace("graph=\"message/a1\"", "graph=\"message/other\""),
        envelope.replace(" sha256=\"00\"", ""),
        format!("An example: `{envelope}`"),
        format!("```xml\n{envelope}\n```"),
        format!("~~~xml\n{envelope}\n~~~"),
        envelope
            .lines()
            .map(|line| format!("    {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
        "Mention `<channel>` in prose.".to_owned(),
    ] {
        let bodies = adapt::from_harness(true, &raw, &Default::default());
        assert!(
            matches!(&bodies[..], [Body::User(text)] if text == raw.trim()),
            "{bodies:?}"
        );
    }
    // An invalid quoted head must not steal the attributes of a later real delivery.
    let raw = format!("Mention `<smalltalk-message>` here.\n{envelope}");
    let bodies = adapt::from_harness(true, &raw, &Default::default());
    assert!(
        matches!(&bodies[..], [Body::User(_), Body::Mail { from, subject, body, .. }]
        if from == "person/example" && subject == "Keys & locks" && body == "Rotate <all> keys."),
        "{bodies:?}"
    );
    let shown = std::collections::BTreeSet::from(["message/a1".into()]);
    assert!(adapt::from_harness(true, envelope, &shown).is_empty());
}

#[test]
fn skill_expansions_match_the_pending_name_in_this_turn() {
    let timeline: Vec<st3_client::TimelineEntry> = serde_json::from_str(include_str!(
        "../../../fixtures/clients/transcripts/claude-skill.json"
    ))
    .unwrap();
    let mut other = timeline.clone();
    if let st3_client::TimelineBody::ToolCall(call) = &mut other[0].body {
        call.arguments = serde_json::json!({"skill": "another"});
    }
    let entries = adapt::conversation(&other, &Default::default());
    assert!(
        entries
            .iter()
            .any(|entry| matches!(&entry.body, Body::User(_)))
    );
    // A later turn's content must not attach to a stale call.
    let mut later = timeline.clone();
    later.insert(2, timeline[3].clone());
    let entries = adapt::conversation(&later, &Default::default());
    assert!(
        entries
            .iter()
            .any(|entry| matches!(&entry.body, Body::User(_)))
    );
    // A later tool result must not overwrite the loaded body.
    let mut reordered = timeline.clone();
    reordered.swap(1, 2);
    let entries = adapt::conversation(&reordered, &Default::default());
    assert!(matches!(&entries[0].body, Body::Tool { output, .. }
        if output.iter().any(|line| line == "## Messages")));
    // Plugin names carry a namespace while the base directory uses the skill basename.
    let mut plugin = timeline.clone();
    if let st3_client::TimelineBody::ToolCall(call) = &mut plugin[0].body {
        call.arguments = serde_json::json!({"skill": "example:st"});
    }
    let entries = adapt::conversation(&plugin, &Default::default());
    assert_eq!(entries.len(), 2);
}

#[test]
#[cfg(feature = "ratatui")]
fn wrapping_multiline_runs_preserves_blank_rows_styles_and_copy_boundaries() {
    let rows = text::wrap(
        &[
            text::run(
                "When\nit prints nothing\n\n",
                ratatui::style::Style::default(),
            ),
            text::run(
                "## Messages\nlast",
                ratatui::style::Style::default().fg(Color::Blue),
            ),
        ],
        14,
        &[],
        &[],
        None,
    );
    assert!(rows.iter().all(|line| !text::plain(line).contains('\n')));
    let selection = Selection {
        pane: "example".into(),
        anchor: (0, 0),
        head: (rows.len() - 1, 100),
    };
    assert_eq!(
        selection.text(&rows),
        "When\nit prints nothing\n\n## Messages\nlast"
    );
    assert!(
        rows.iter().any(|line| text::plain(line) == "## Messages"
            && line.spans[0].style.fg == Some(Color::Blue))
    );
}

#[test]
fn attachment_only_mail_and_non_image_refs_remain_visible() {
    let message: st3_client::TimelineEntry = serde_json::from_value(serde_json::json!({
        "id":"mail","sequence":1,"revision":1,"timestamp":"2026-10-05T10:00:00Z",
        "role":"user","final":true,"type":"message",
        "body":{"message_id":"message/media","role":"user","from":"person/ada","to":"agent/a",
            "attachments":[{"blob":"blob/safe-hash","origin":"host/test","sha256":"safe-hash","media_type":"application/pdf","name":"document.pdf","size":12}]}
    }))
    .unwrap();
    let standalone = adapt::conversation(std::slice::from_ref(&message), &Default::default());
    let display = serde_json::to_string(&standalone).unwrap();
    assert!(display.contains("safe-hash") && display.contains("application/pdf"));
    let content = item("content", 2, 1, "");
    let paired = adapt::conversation(&[message, content], &Default::default());
    assert!(matches!(&paired[0].body, Body::Mail { body, .. } if body.contains("safe-hash")));
}

#[test]
fn pending_binding_preserves_authorized_activity_without_claiming_an_empty_harness() {
    let activity = item("activity", 1, 1, "Captured activity");
    let mail = serde_json::from_value(serde_json::json!({
        "id":"mail","sequence":2,"revision":1,"timestamp":"2026-10-05T10:00:00Z",
        "role":"user","final":true,"type":"message",
        "body":{"message_id":"message/status","from":"person/ada","to":"agent/a","title":"Status?"}
    }))
    .unwrap();
    let text = item("mail-content", 3, 1, "How is the audit going?");
    let notice = serde_json::from_value(serde_json::json!({
        "id":"notice","sequence":4,"revision":1,"timestamp":"2026-10-05T10:00:01Z",
        "role":"system","final":true,"type":"error",
        "body":{"code":"transcript-not-bound","message":"transcript not bound: path unknown",
            "retryable":true,"details":{"not_yet":true}}
    }))
    .unwrap();
    let timeline = [activity, mail, text, notice];
    assert!(adapt::unreadable_transcript(&timeline).is_none());
    let rendered = adapt::conversation(&timeline, &Default::default());
    assert!(matches!(&rendered[0].body, Body::Assistant(text) if text == "Captured activity"));
    assert!(matches!(&rendered[1].body, Body::Mail { body, .. } if body == "How is the audit going?"));
    let Body::Event(availability) = &rendered[2].body else {
        panic!("{rendered:?}");
    };
    assert!(availability.contains("unavailable"));
    assert!(!availability.contains("nothing"));
}

fn review_entry(kind: &str, role: &str, body: serde_json::Value) -> st3_client::TimelineEntry {
    serde_json::from_value(serde_json::json!({
        "id":kind,"sequence":1,"revision":1,"timestamp":"2026-10-05T10:00:00Z",
        "role":role,"final":true,"type":kind,"body":body
    }))
    .unwrap()
}

#[test]
fn review_usage_is_compact_and_preserves_supplied_tokens_cost_and_semantics() {
    for (body, expected) in [
        (serde_json::json!({"semantics":"response","driver":"omp","input_tokens":12,"output_tokens":5,"cached_tokens":3,"cache_write_tokens":2,"total_tokens":22,"cost":0.25,"currency":"USD"}),
            "usage: response · input 12 · output 5 · cached 3 · cache write 2 · total 22 · cost USD 0.25"),
        (serde_json::json!({"semantics":"context_occupancy","driver":"omp","context_used_tokens":0,"context_window_tokens":100}),
            "usage: context occupancy · context used 0 · context window 100"),
        (serde_json::json!({"semantics":"session_cumulative","driver":"omp","cost":0}),
            "usage: session cumulative · cost 0 (currency unknown)"),
        (serde_json::json!({"semantics":"future","driver":"omp"}),
            "usage: unknown"),
    ] {
        let mut body = body;
        body["attribution"] = serde_json::json!({"agent_id":"ATTRIBUTION_SENTINEL","mission_run_id":"MISSION_SENTINEL","generation_id":null,"step_id":null});
        let rendered = adapt::conversation(&[review_entry("usage", "assistant", body)], &Default::default());
        assert!(matches!(&rendered[..], [Entry { body: Body::Event(line), .. }] if line == expected), "{rendered:?}");
    }
}

#[test]
fn unknown_role_content_preserves_payload_and_skips_blank_events() {
    for text in [None, Some(""), Some(" \n\t")] {
        let entry = review_entry("content", "future-role", serde_json::json!({
            "media_type":"text/plain","text":text
        }));
        assert!(adapt::conversation(&[entry], &Default::default()).is_empty());
    }
    let entry = review_entry("content", "future-role", serde_json::json!({
        "media_type":"image/png","text":"UNRECOGNIZED_PAYLOAD","attachment_id":"attachment/safe"
    }));
    let rendered = adapt::conversation(&[entry], &Default::default());
    let display = serde_json::to_string(&rendered).unwrap();
    assert!(display.contains("attachment/safe"));
    assert!(display.contains("UNRECOGNIZED_PAYLOAD"));
    let entry = review_entry("content", "future-role", serde_json::json!({
        "media_type":"text/plain","text":"UNRECOGNIZED_PAYLOAD"
    }));
    let rendered = adapt::conversation(&[entry], &Default::default());
    let display = serde_json::to_string(&rendered).unwrap();
    assert!(display.contains("unknown role"));
    assert!(display.contains("UNRECOGNIZED_PAYLOAD"));
}

#[test]
fn review_delayed_mail_content_contains_each_media_ref_once() {
    let message = review_entry("message", "user", serde_json::json!({
        "message_id":"message/media","from":"person/ada","to":"agent/a",
        "attachments":[{"blob":"blob/hash","origin":"host/test","sha256":"hash","media_type":"application/pdf","size":12}]
    }));
    let status = review_entry("status", "system", serde_json::json!({"status":"running"}));
    for attachment_id in [None, Some("blob/hash"), Some("hash")] {
        let content = review_entry("content", "user", serde_json::json!({
            "media_type":"application/pdf","text":"Document attached","attachment_id":attachment_id
        }));
        let rendered = adapt::conversation(
            &[message.clone(), status.clone(), content],
            &Default::default(),
        );
        let mail = rendered
            .iter()
            .find_map(|entry| match &entry.body {
                Body::Mail { body, .. } => Some(body),
                _ => None,
            })
            .expect("delayed content stays associated with its mail envelope");
        assert!(mail.contains("Document attached") && mail.contains("application/pdf"));
        assert_eq!(
            serde_json::to_string(&rendered).unwrap().matches("hash").count(),
            1
        );
    }
}

#[test]
fn review_unknown_type_and_diagnostics_are_bounded_on_unicode_boundaries() {
    let unknown = review_entry(&"界".repeat(1000), "system", serde_json::json!({"secret":"SENTINEL"}));
    let rendered = adapt::conversation(&[unknown], &Default::default());
    assert!(matches!(&rendered[0].body, Body::Event(line)
        if line.contains('…') && line.matches('界').count() == 64));
    for (code, details) in [
        ("transcript-not-bound", serde_json::json!({"not_yet":true})),
        ("transcript-not-bound", serde_json::json!({})),
        ("other", serde_json::json!({"severity":"warning"})),
        ("other", serde_json::json!({})),
    ] {
        let entry = review_entry("error", "system", serde_json::json!({
            "code":code,"message":"界".repeat(1000),"retryable":true,"details":details
        }));
        let rendered = adapt::conversation(std::slice::from_ref(&entry), &Default::default());
        assert!(matches!(&rendered[0].body, Body::Event(line)
            if line.contains('…') && line.matches('界').count() == 256));
        if let Some(unavailable) = adapt::unreadable_transcript(&[entry]) {
            assert!(unavailable.contains('…') && unavailable.chars().count() < 300);
        }
    }
    for message in ["界".repeat(256), "line one\nline two".into()] {
        let entry = review_entry("error", "system", serde_json::json!({
            "code":"other","message":message,"retryable":true,"details":{"severity":"warning"}
        }));
        let rendered = adapt::conversation(&[entry], &Default::default());
        assert!(matches!(&rendered[0].body, Body::Event(line) if line == &message));
    }
}

#[test]
fn review_unpaired_mail_refs_do_not_leak_into_the_next_envelope() {
    let first = review_entry("message", "user", serde_json::json!({
        "message_id":"message/first","from":"person/ada","to":"agent/a",
        "attachments":[{"blob":"blob/old","origin":"host/test","sha256":"old","media_type":"application/pdf","size":12}]
    }));
    let second = review_entry("message", "user", serde_json::json!({
        "message_id":"message/second","from":"person/ada","to":"agent/a"
    }));
    let content = review_entry("content", "user", serde_json::json!({
        "media_type":"text/plain","text":"Second message"
    }));
    let rendered = adapt::conversation(&[first, second, content], &Default::default());
    assert!(matches!(&rendered[..], [
        Entry { id: first, body: Body::Event(refs), .. },
        Entry { id: second, body: Body::Mail { body, .. }, .. }
    ] if first == "message/first" && refs.contains("application/pdf") && second == "message/second" && body == "Second message"), "{rendered:?}");
}

#[test]
fn a_persons_signed_message_says_which_device_signed_it_and_whether_it_checks() {
    // Nathan, 2026-10-06: signatures existed but no screen showed them.
    let timeline = |provenance: serde_json::Value| -> Vec<st3_client::TimelineEntry> {
        vec![
            serde_json::from_value(serde_json::json!({
                "id": "entry/1", "sequence": 1, "revision": 1, "timestamp": "2026-10-06T21:00:00Z",
                "role": "user", "final": true, "type": "message",
                "body": {"message_id": "message/example", "from": "person/example", "to": "agent/example/atlas",
                         "provenance": provenance}
            }))
            .unwrap(),
            serde_json::from_value(serde_json::json!({
                "id": "entry/2", "sequence": 2, "revision": 1, "timestamp": "2026-10-06T21:00:01Z",
                "role": "assistant", "final": true, "type": "content",
                "body": {"media_type": "text/plain", "text": "hello"}
            }))
            .unwrap(),
        ]
    };
    let mark = |provenance: serde_json::Value| {
        adapt::conversation(&timeline(provenance), &Default::default())
            .into_iter()
            .find_map(|entry| match entry.body {
                crate::Body::Mail { signed, .. } => Some(signed),
                _ => None,
            })
            .expect("a mail entry")
    };
    assert_eq!(
        mark(serde_json::json!({"verdict": "verified", "signer": "person/example", "key": "p256:AAAA", "device": "example phone (secure enclave)"})).as_deref(),
        Some("✓ example phone (secure enclave)")
    );
    // A key with no label falls back to who signed.
    assert_eq!(
        mark(serde_json::json!({"verdict": "verified", "signer": "person/example", "key": "p256:AAAA"})).as_deref(),
        Some("✓ person/example")
    );
    assert!(mark(serde_json::json!({"verdict": "held", "reason": "delegation d1 has not arrived"})).unwrap().starts_with("⚠ signature held: delegation"));
    assert!(mark(serde_json::json!({"verdict": "invalid", "reason": "the signature does not match the claim"})).unwrap().starts_with("✕ signature invalid"));
    // An old message with no signature says nothing.
    assert_eq!(mark(serde_json::json!({"verdict": "unsigned"})), None);
}

#[test]
fn native_unknown_system_blocks_and_user_reasoning_tags_remain_visible() {
    let raw = "[unrecognized future]\n{\"raw\":{\"token\":\"invented-token\"}}";
    let entry: st3_client::TimelineEntry = serde_json::from_value(serde_json::json!({"id":"timeline-entry/raw","sequence":1,"revision":1,"timestamp":"2026-10-06T12:00:00Z","role":"system","type":"content","final":true,"body":{"media_type":"text/plain","text":raw,"blocks":[{"id":"raw","kind":"unknown","source_type":"future","payload":{"raw":{"token":"invented-token"}}}]}})).unwrap();
    let shown = crate::adapt::conversation(&[entry], &std::collections::BTreeMap::new());
    assert!(matches!(&shown[0].body, Body::Event(text) if text == raw));
    assert_eq!(
        crate::clean_message_text_with_filters(
            "<analysis>visible invented-token</analysis>",
            crate::SHOW_EVERYTHING
        ),
        "<analysis>visible invented-token</analysis>"
    );
}

#[test]
fn display_filters_hide_context_but_raw_mode_keeps_every_entry_and_byte() {
    let raw = "<system-reminder>invented-token</system-reminder>visible\u{1b}\n<thinking>private reasoning</thinking>";
    let content: st3_client::TimelineEntry = serde_json::from_value(serde_json::json!({"id":"timeline-entry/filters","sequence":1,"revision":1,"timestamp":"2026-10-06T12:00:00Z","role":"user","type":"content","final":true,"body":{"media_type":"text/plain","text":raw,"blocks":[{"id":"source","kind":"source_record","source_type":"claude","visibility":"internal","payload":{"raw":{"text":raw}}}]}})).unwrap();
    let original = serde_json::to_value(&content).unwrap();
    let shown = adapt::conversation_with_filters(
        std::slice::from_ref(&content),
        &Default::default(),
        crate::DEFAULT_FILTERS,
    );
    assert!(matches!(&shown[0].body, Body::User(text) if text == "visible"));
    let everything = adapt::conversation_with_filters(
        std::slice::from_ref(&content),
        &Default::default(),
        crate::SHOW_EVERYTHING,
    );
    let Body::User(text) = &everything[0].body else {
        panic!("raw view")
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(text).unwrap(),
        original
    );
    assert_eq!(serde_json::to_value(content).unwrap(), original);
}

#[test]
fn native_window_notice_explains_that_the_remainder_is_not_fetchable() {
    let entry = serde_json::from_value(serde_json::json!({
        "id":"cut","sequence":0,"revision":1,"timestamp":"2026-10-05T10:00:00Z",
        "role":"system","final":true,"type":"truncation",
        "body":{"reason":"the native transcript prefix is outside the bounded read window; not fetchable through this owner read","omitted_from_sequence":0,"omitted_to_sequence":0}
    })).unwrap();
    let rendered = crate::adapt::conversation(&[entry], &Default::default());
    assert!(rendered.iter().any(|entry|matches!(&entry.body,crate::Body::Event(text) if text.contains("not fetchable"))));
}

#[test]
fn native_window_notice_with_unfetchable_remainder_stays_at_start() {
    let entry = |index: u64, kind: &str, body: serde_json::Value, at: &str| -> st3_client::TimelineEntry {
        serde_json::from_value(serde_json::json!({
            "id": format!("entry/{index}"), "sequence": index, "revision": 1,
            "timestamp": at, "role": "assistant", "final": true, "type": kind, "body": body
        }))
        .unwrap()
    };
    let timeline = vec![
        entry(1, "content", serde_json::json!({"media_type":"text/plain","text":"first words"}), "2026-10-05T10:00:00Z"),
        entry(2, "content", serde_json::json!({"media_type":"text/plain","text":"last words"}), "2026-10-05T11:00:00Z"),
        entry(
            0,
            "truncation",
            serde_json::json!({"reason":"the native transcript prefix is outside the bounded read window; not fetchable through this owner read","omitted_from_sequence":0,"omitted_to_sequence":0}),
            "2026-10-06T09:00:00Z",
        ),
    ];
    let rendered = adapt::conversation(&timeline, &Default::default());
    let shown = rendered
        .iter()
        .map(|entry| serde_json::to_string(&entry.body).unwrap())
        .collect::<Vec<_>>();
    assert!(shown[0].contains("Earlier history is not shown"), "{shown:?}");
    assert!(shown[0].contains("not fetchable"), "{shown:?}");
    assert!(shown.last().unwrap().contains("last words"), "{shown:?}");
}

// A real native Claude run (names invented): every turn is a wrapper entry, an empty reasoning
// step, and the harness's own records between calls. None of that is conversation.
#[test]
fn native_run_shows_one_mail_and_collates_calls_without_bookkeeping() {
    let items = serde_json::from_str::<Vec<st3_client::TimelineEntry>>(include_str!(
        "../../../fixtures/clients/transcripts/native-claude-run.json"
    ))
    .unwrap();
    let shown = adapt::conversation(&items, &Default::default());
    let text = serde_json::to_string(&shown).unwrap();
    assert_eq!(
        shown
            .iter()
            .filter(|entry| matches!(entry.body, Body::Mail { .. }))
            .count(),
        1,
        "the delivery reads once"
    );
    assert!(!text.contains("total_tokens_reminder") && !text.contains("last-prompt"));
    assert!(
        !shown.iter().any(
            |entry| matches!(&entry.body, Body::Assistant(text) if text.trim() == "[reasoning]")
        )
    );
    // Reasoning the model shared stays, and a record st does not know stays visible.
    assert!(text.contains("Check the queue before answering."));
    assert!(text.contains("future-kind"));
    // The two calls that the empty reasoning and the reminder separated are one run.
    let tools = shown
        .windows(2)
        .filter(|pair| {
            matches!(pair[0].body, Body::Tool { .. }) && matches!(pair[1].body, Body::Tool { .. })
        })
        .count();
    assert_eq!(tools, 1);
    assert_eq!(display_rows(&shown), shown.len() - 1);
    // Showing everything keeps every entry, and the switch alone brings the records back.
    let without = crate::DEFAULT_FILTERS
        .iter()
        .copied()
        .filter(|filter| *filter != crate::DisplayFilter::Bookkeeping)
        .collect::<Vec<_>>();
    let all = adapt::conversation_with_filters(&items, &Default::default(), &without);
    assert!(
        serde_json::to_string(&all)
            .unwrap()
            .contains("total_tokens_reminder")
    );
    assert_eq!(
        adapt::conversation_with_filters(&items, &Default::default(), crate::SHOW_EVERYTHING).len(),
        items.len()
    );
}
