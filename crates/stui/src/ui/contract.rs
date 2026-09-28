//! The shared client contract in `fixtures/clients`: what every Small Talk client shows.
//!
//! stui writes these files and the iOS app tests against them, so both apps clean the same
//! transcripts the same way, use the same words and colours, and share one demo world.
//! Run `STUI_UPDATE_CONTRACT=1 cargo test -p stui contract` after an intended change.

use super::adapt;
use super::demo;
use super::theme;
use super::view::*;
use ratatui::style::Color;
use serde_json::{Value, json};
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/clients")
}

fn hex(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        other => format!("{other:?}"),
    }
}

pub fn theme_tokens() -> Value {
    let tokens = [
        ("base", theme::BASE),
        ("mantle", theme::MANTLE),
        ("crust", theme::CRUST),
        ("surface0", theme::SURFACE0),
        ("surface1", theme::SURFACE1),
        ("surface2", theme::SURFACE2),
        ("overlay0", theme::OVERLAY0),
        ("overlay1", theme::OVERLAY1),
        ("subtext0", theme::SUBTEXT0),
        ("subtext1", theme::SUBTEXT1),
        ("text", theme::TEXT),
        ("lavender", theme::LAVENDER),
        ("blue", theme::BLUE),
        ("sapphire", theme::SAPPHIRE),
        ("teal", theme::TEAL),
        ("green", theme::GREEN),
        ("yellow", theme::YELLOW),
        ("peach", theme::PEACH),
        ("red", theme::RED),
        ("mauve", theme::MAUVE),
        ("accent", theme::ACCENT),
        ("person", theme::PERSON),
        ("working", theme::WORKING),
        ("idle", theme::IDLE),
        ("done", theme::DONE),
        ("fault", theme::FAULT),
        ("waiting", theme::WAITING),
        ("quiet", theme::QUIET),
        ("row_selected", theme::ROW_SELECTED),
        ("user_bg", theme::USER_BG),
        ("tool_bg", theme::TOOL_BG),
        ("tool_ok_bg", theme::TOOL_OK_BG),
        ("tool_err_bg", theme::TOOL_ERR_BG),
        ("selection_bg", theme::SELECTION_BG),
    ];
    json!({
        "name": "Catppuccin Mocha",
        "rule": "person (mauve) means a person is needed and is used for nothing else",
        "colors": tokens.iter().map(|(name, color)| (name.to_string(), json!(hex(*color)))).collect::<serde_json::Map<_, _>>(),
    })
}

pub fn words() -> Value {
    let mission = [
        Word::Decision,
        Word::Stalled,
        Word::Unstaffed,
        Word::Unclaimed,
        Word::Queued,
        Word::Working,
        Word::Watching,
        Word::Held,
        Word::Idle,
        Word::Done,
        Word::Failed,
    ]
    .iter()
    .map(|word| {
        json!({
            "id": serde_json::to_value(word).unwrap(),
            "name": word.name(),
            "explain": word.explain(),
            "glyph": super::screens::word_style(*word, "⠋").0,
        })
    })
    .collect::<Vec<_>>();
    let agent = [
        AgentState::NeedsYou,
        AgentState::Fault,
        AgentState::Working,
        AgentState::Idle,
        AgentState::Starting,
        AgentState::Stopped,
        AgentState::Unknown,
    ]
    .iter()
    .map(|state| {
        json!({
            "id": serde_json::to_value(state).unwrap(),
            "name": super::screens::agent_word(*state),
            "glyph": super::screens::agent_glyph(*state, "⠋").0,
        })
    })
    .collect::<Vec<_>>();
    let tier = [Tier::Stopped, Tier::Alert, Tier::Today, Tier::Later]
        .iter()
        .map(|tier| json!({ "id": serde_json::to_value(tier).unwrap(), "title": tier.title() }))
        .collect::<Vec<_>>();
    json!({ "mission": mission, "agent": agent, "tier": tier, "tabs": super::screens::TABS })
}

/// The cleaned conversation. `at` is a local display time, so it is left out: it depends on
/// the machine's time zone, not on the cleaning rules.
fn conversation_of(fixture: &str) -> Value {
    let timeline: Vec<st3_client::TimelineEntry> = serde_json::from_str(fixture).unwrap();
    let mut value =
        serde_json::to_value(adapt::conversation(&timeline, &[], &Default::default())).unwrap();
    for entry in value.as_array_mut().unwrap() {
        entry.as_object_mut().unwrap().remove("at");
    }
    value
}

fn files() -> Vec<(&'static str, Value)> {
    vec![
        (
            "demo-world.json",
            serde_json::to_value(demo::world()).unwrap(),
        ),
        ("theme.json", theme_tokens()),
        ("words.json", words()),
        (
            "transcripts/claude.expected.json",
            conversation_of(include_str!(
                "../../../../fixtures/clients/transcripts/claude.json"
            )),
        ),
        (
            "transcripts/codex.expected.json",
            conversation_of(include_str!(
                "../../../../fixtures/clients/transcripts/codex.json"
            )),
        ),
    ]
}

#[test]
fn the_shared_client_contract_matches_stui() {
    let update = std::env::var_os("STUI_UPDATE_CONTRACT").is_some();
    for (name, value) in files() {
        let path = root().join(name);
        let text = serde_json::to_string_pretty(&value).unwrap() + "\n";
        if update {
            std::fs::write(&path, &text).unwrap();
            continue;
        }
        let current = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("{name} is missing; run with STUI_UPDATE_CONTRACT=1"));
        assert!(
            current == text,
            "{name} no longer matches stui; if the change is intended, run STUI_UPDATE_CONTRACT=1 cargo test -p stui contract and update the iOS app"
        );
    }
}
