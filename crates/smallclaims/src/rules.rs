//! Rules: what principals may write, each in one of three modes.
//!
//! A rule is a claim of kind [`RULE_SET`] on `rule/NAME`; the latest one for a name, in canonical
//! order, is the rule. It restricts a write when the writer matches its `actors` (and none of its
//! `except`), the claim's kind matches its `kinds`, and the subject matches its `subjects` and
//! none of its `unless_subjects`. A subject pattern may say `{actor}`, the writer's name without
//! its family, or `{namespace}`, the first two segments of that name: for
//! `agent/team/web/reviewer`, `doc/{namespace}/**` is everything under `doc/team/web/`.
//!
//! Only a person sets a rule. An agent's `rule.set` is refused whatever the rules say, so no
//! lockdown can be undone by the agents it restricts.
//!
//! A rule's mode says what a restricted write does:
//!
//! - `off`: nothing.
//! - `audit`: the write proceeds, and a [`RULE_AUDITED`] claim records who tried what, under which
//!   rule. Every rule ships in audit, so its log can be read before it rejects anything.
//! - `enforce`: the write is refused with the typed error `rule-denied`.
//!
//! Patterns: `*` matches within one path segment (no `/`), `**` matches anything.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A rule, written on `rule/NAME`.
pub const RULE_SET: &str = "rule.set";
/// A write a rule in audit mode would have refused, written on the rule's subject.
pub const RULE_AUDITED: &str = "rule.audited";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Off,
    #[default]
    Audit,
    Enforce,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Audit => "audit",
            Self::Enforce => "enforce",
        }
    }
}

/// The fields of a [`RULE_SET`] claim.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Rule {
    #[serde(default)]
    pub mode: Mode,
    /// What the rule is for, in words a person reads in an audit record.
    #[serde(default)]
    pub description: String,
    pub actors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub except: Vec<String>,
    pub kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subjects: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unless_subjects: Vec<String>,
}

impl Rule {
    pub fn from_fields(fields: &Value) -> Option<Self> {
        serde_json::from_value(fields.clone()).ok()
    }

    pub fn fields(&self) -> std::collections::BTreeMap<String, Value> {
        serde_json::from_value(serde_json::to_value(self).expect("a rule serializes"))
            .expect("a rule is an object")
    }

    /// Whether this rule restricts `actor` writing `kind` on `subject`, whatever its mode.
    pub fn restricts(&self, actor: &str, kind: &str, subject: &str) -> bool {
        let name = actor.split_once('/').map_or(actor, |(_, name)| name);
        let namespace = name.splitn(3, '/').take(2).collect::<Vec<_>>().join("/");
        let any = |patterns: &[String], value: &str| {
            patterns.iter().any(|pattern| {
                glob(
                    &pattern
                        .replace("{actor}", name)
                        .replace("{namespace}", &namespace),
                    value,
                )
            })
        };
        any(&self.actors, actor)
            && !any(&self.except, actor)
            && any(&self.kinds, kind)
            && (self.subjects.is_empty() || any(&self.subjects, subject))
            && !any(&self.unless_subjects, subject)
    }
}

/// A rule by name, as the graph holds it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NamedRule {
    pub name: String,
    pub claim_id: String,
    #[serde(flatten)]
    pub rule: Rule,
}

/// What the rules say about one write.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Decision {
    /// Rules in audit mode that would refuse it.
    pub audited: Vec<NamedRule>,
    /// The first rule in enforce mode that refuses it.
    pub denied: Option<NamedRule>,
}

/// Decide one write against `rules`.
pub fn decide(rules: &[NamedRule], actor: &str, kind: &str, subject: &str) -> Decision {
    let mut decision = Decision::default();
    for rule in rules {
        if rule.rule.mode == Mode::Off || !rule.rule.restricts(actor, kind, subject) {
            continue;
        }
        match rule.rule.mode {
            Mode::Enforce if decision.denied.is_none() => decision.denied = Some(rule.clone()),
            Mode::Audit => decision.audited.push(rule.clone()),
            _ => {}
        }
    }
    decision
}

/// `*` matches within one path segment, `**` matches anything; every other character matches
/// itself.
pub fn glob(pattern: &str, value: &str) -> bool {
    fn matches(pattern: &[u8], value: &[u8]) -> bool {
        match pattern {
            [] => value.is_empty(),
            [b'*', b'*', rest @ ..] => (0..=value.len()).any(|skip| matches(rest, &value[skip..])),
            [b'*', rest @ ..] => {
                let segment = value
                    .iter()
                    .position(|byte| *byte == b'/')
                    .unwrap_or(value.len());
                (0..=segment).any(|skip| matches(rest, &value[skip..]))
            }
            [first, rest @ ..] => value.first() == Some(first) && matches(rest, &value[1..]),
        }
    }
    matches(pattern.as_bytes(), value.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(mode: Mode) -> Rule {
        Rule {
            mode,
            description: "an agent publishes only under its own prefix".into(),
            actors: vec!["agent/**".into()],
            except: vec!["agent/example/librarian".into()],
            kinds: vec!["doc.bound".into()],
            subjects: vec!["doc/**".into()],
            unless_subjects: vec!["doc/{actor}/**".into()],
        }
    }

    #[test]
    fn globs_match_segments_and_paths() {
        assert!(glob("agent/*", "agent/reviewer"));
        assert!(!glob("agent/*", "agent/example/reviewer"));
        assert!(glob("agent/**", "agent/example/reviewer"));
        assert!(glob("mission.*", "mission.produced"));
        assert!(glob("mission*", "mission-run.created"));
        assert!(!glob("mission.*", "mission-run.created"));
        assert!(glob("**", ""));
    }

    #[test]
    fn a_rule_restricts_only_what_it_names() {
        let rule = rule(Mode::Audit);
        assert!(rule.restricts("agent/example/reviewer", "doc.bound", "doc/elsewhere/notes"));
        assert!(!rule.restricts(
            "agent/example/reviewer",
            "doc.bound",
            "doc/example/reviewer/notes"
        ));
        assert!(!rule.restricts(
            "agent/example/librarian",
            "doc.bound",
            "doc/elsewhere/notes"
        ));
        assert!(!rule.restricts("person/ada", "doc.bound", "doc/elsewhere/notes"));
        assert!(!rule.restricts(
            "agent/example/reviewer",
            "message.sent",
            "doc/elsewhere/notes"
        ));
        let namespace = Rule {
            unless_subjects: vec!["doc/{namespace}/**".into()],
            ..rule
        };
        assert!(!namespace.restricts(
            "agent/team/web/reviewer",
            "doc.bound",
            "doc/team/web/plan"
        ));
        assert!(namespace.restricts(
            "agent/team/web/reviewer",
            "doc.bound",
            "doc/team/api/plan"
        ));
    }

    #[test]
    fn modes_decide_what_a_restricted_write_does() {
        let named = |name: &str, mode| NamedRule {
            name: name.into(),
            claim_id: format!("claim/{name}"),
            rule: rule(mode),
        };
        let write = ("agent/example/reviewer", "doc.bound", "doc/elsewhere/notes");
        let decide = |rules: &[NamedRule]| decide(rules, write.0, write.1, write.2);
        assert_eq!(decide(&[named("off", Mode::Off)]), Decision::default());
        let audited = decide(&[named("audit", Mode::Audit)]);
        assert_eq!(audited.audited.len(), 1);
        assert!(audited.denied.is_none());
        let both = decide(&[named("audit", Mode::Audit), named("enforce", Mode::Enforce)]);
        assert_eq!(both.audited.len(), 1);
        assert_eq!(both.denied.unwrap().name, "enforce");
    }
}
