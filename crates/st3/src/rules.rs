//! The rules smalltalk ships, in terms of its own claim kinds. `st rules lockdown` sets them all,
//! in audit mode: each logs what it would refuse until a person turns it to enforce.

use smallclaims::rules::{Mode, Rule};

/// The lockdown preset. `starters` are the agents allowed to start other agents.
pub fn lockdown(starters: &[String]) -> Vec<(&'static str, Rule)> {
    vec![
        (
            "agents-create-no-missions",
            Rule {
                mode: Mode::Audit,
                description: "agents may not create or publish missions".into(),
                actors: vec!["agent/**".into()],
                kinds: vec!["mission-run.created".into(), "mission.published".into()],
                ..Rule::default()
            },
        ),
        (
            "named-agents-start-agents",
            Rule {
                mode: Mode::Audit,
                description: "only the named agents may start, declare or stop agents".into(),
                actors: vec!["agent/**".into()],
                except: starters.to_vec(),
                kinds: vec!["intent.desired".into()],
                subjects: vec!["agent/**".into()],
                ..Rule::default()
            },
        ),
        (
            "agents-publish-in-namespace",
            Rule {
                mode: Mode::Audit,
                description: "an agent publishes documents only under its own namespace".into(),
                actors: vec!["agent/**".into()],
                kinds: vec!["doc.bound".into()],
                subjects: vec!["doc/**".into()],
                unless_subjects: vec!["doc/{namespace}/**".into()],
                ..Rule::default()
            },
        ),
    ]
}
