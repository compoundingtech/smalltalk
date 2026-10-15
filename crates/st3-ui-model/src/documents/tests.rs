use super::*;
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../../../fixtures/clients/documents-v1.json")).unwrap()
}

fn content(reference: &str, text: &str) -> DocumentContent {
    let (name, version) = reference.rsplit_once('@').unwrap();
    DocumentContent {
        reference: reference.to_owned(),
        bytes: text.as_bytes().to_vec(),
        name: Some(name.to_owned()),
        version: Some(version.to_owned()),
        size: Some(text.len() as u64),
        changed_at_unix_ms: Some(1_700_000_000_000),
        latest: Some(true),
        owner: Some("agent/garden/writer".to_owned()),
    }
}

#[test]
fn finds_the_links_the_fixture_pins() {
    for case in fixture()["links"].as_array().unwrap() {
        let expected: Vec<DocumentLink> = serde_json::from_value(case["links"].clone()).unwrap();
        assert_eq!(
            document_links(case["text"].as_str().unwrap()),
            expected,
            "{}",
            case["name"]
        );
    }
}

#[test]
fn a_long_document_renders_unchanged() {
    let fixture = fixture();
    let text = fixture["links"][0]["text"].as_str().unwrap();
    let reference = format!("doc/garden/plan@{}", "c".repeat(64));
    let DocumentView::Ready(document) =
        DocumentView::from_content("doc/garden/plan", &content(&reference, text))
    else {
        panic!("a text document is ready");
    };
    assert_eq!(document.text, text);
    assert!(document.text.contains("| North | Ada |"));
    assert!(document.text.contains("```kdl\nstep \"plant\""));
    assert!(document.text.contains("     1. Water the beds"));
    assert_eq!(document.name, "doc/garden/plan");
    assert_eq!(document.short_version, "cccccccccccc");
    assert_eq!(document.changed_at_unix_ms, Some(1_700_000_000_000));
    assert_eq!(document.links.len(), 9);
}

#[test]
fn an_older_daemon_answer_still_names_the_document() {
    let reference = format!("doc/garden/plan@{}", "d".repeat(64));
    let old = DocumentContent {
        reference: reference.clone(),
        bytes: b"# Plan".to_vec(),
        name: None,
        version: None,
        size: None,
        changed_at_unix_ms: None,
        latest: None,
        owner: None,
    };
    let DocumentView::Ready(document) = DocumentView::from_content(&reference, &old) else {
        panic!("ready");
    };
    assert_eq!(document.name, "doc/garden/plan");
    assert_eq!(document.version, "d".repeat(64));
    assert_eq!(document.size, 6);
    assert_eq!(document.changed_at_unix_ms, None);
}

#[test]
fn bytes_that_are_not_text_say_so() {
    let reference = format!("doc/garden/photo@{}", "e".repeat(64));
    let mut photo = content(&reference, "");
    photo.bytes = vec![0xff, 0xfe, 0x00];
    let view = DocumentView::from_content("doc/garden/photo", &photo);
    let DocumentView::Unavailable { why, explanation, .. } = view else {
        panic!("not text");
    };
    assert_eq!(why, Unavailable::NotText);
    assert!(explanation.contains("doc/garden/photo"));
}

#[test]
fn errors_map_to_the_states_the_fixture_pins() {
    for case in fixture()["errors"].as_array().unwrap() {
        let status = case["status"].as_u64().map(|status| status as u16);
        let why = Unavailable::from_error(status, case["code"].as_str().unwrap());
        assert_eq!(serde_json::to_value(why).unwrap(), case["state"], "{case}");
        assert!(!why.explain("doc/x").is_empty());
    }
}

#[test]
fn following_a_link_cycle_returns_to_the_earlier_page() {
    let fixture = fixture();
    let mut trail = DocumentTrail::default();
    for reference in fixture["trail"]["open"].as_array().unwrap() {
        trail.open(reference.as_str().unwrap());
    }
    let expected: Vec<String> = serde_json::from_value(fixture["trail"]["expected"].clone()).unwrap();
    assert_eq!(trail.references, expected);
    assert_eq!(trail.back(), Some("doc/cycle/a"));
    assert_eq!(trail.back(), Some("doc/cycle/a"));
}

fn item(name: &str, version: char) -> DocumentItem {
    let version = version.to_string().repeat(64);
    DocumentItem {
        reference: format!("{name}@{version}"),
        name: name.to_owned(),
        version,
        size: 10,
        changed_at_unix_ms: 1_700_000_000_000,
        latest: true,
        owner: None,
    }
}

#[test]
fn the_list_pages_and_keeps_its_three_states_apart() {
    let mut list = DocumentList::new(Some("doc/garden/"));
    assert_eq!(list.placeholder().as_deref(), Some("Loading…"));
    list.fail(Unavailable::RemoteUnavailable);
    assert!(list.placeholder().unwrap().contains("did not answer"));
    list.push_page(&DocumentsPage {
        items: vec![item("doc/garden/a", 'a'), item("doc/garden/b", 'b')],
        has_more: true,
        limit: 2,
        history: false,
        next_cursor: Some("document/next".to_owned()),
    });
    assert_eq!(list.next_cursor.as_deref(), Some("document/next"));
    list.push_page(&DocumentsPage {
        items: vec![item("doc/garden/b", 'b'), item("doc/garden/c", 'c')],
        has_more: false,
        limit: 2,
        history: false,
        next_cursor: None,
    });
    let names: Vec<_> = list.rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(names, ["doc/garden/a", "doc/garden/b", "doc/garden/c"]);
    assert_eq!(list.next_cursor, None);
    assert_eq!(list.rows[0].short_version, "aaaaaaaaaaaa");

    let mut empty = DocumentList::new(Some("doc/none/"));
    empty.push_page(&DocumentsPage { items: vec![], has_more: false, limit: 50, history: false, next_cursor: None });
    assert_eq!(empty.placeholder().as_deref(), Some("No documents under doc/none/."));
}

#[test]
fn references_parse_only_document_names() {
    let hash = "f".repeat(64);
    assert_eq!(parse_reference(&format!("doc/a@{hash}")), Some(("doc/a", Some(hash.as_str()))));
    assert_eq!(parse_reference("doc/a"), Some(("doc/a", None)));
    for invalid in ["doc/", "doc/a/", "doc/../a", "doc/a@short", "message/a", "docs/a"] {
        assert_eq!(parse_reference(invalid), None, "{invalid}");
    }
}
