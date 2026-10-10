//! The retention policy: how long st keeps each claim kind, and which claims a checkpoint may
//! drop. `retention.toml` beside this crate is the policy; `docs/st3/retention.md` explains it.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::Retention;

/// The policy file, compiled into every build so every member reads the same one.
pub const POLICY_TOML: &str = include_str!("../retention.toml");

/// How long a kind's claims matter.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    /// Only the latest state matters; a checkpoint keeps the newest claim per slot.
    Current,
    /// A status change or delivery, kept for its window.
    Transition,
    /// Kept forever.
    Durable,
    /// An observation series: its history goes to OpenTelemetry and the graph keeps the latest.
    Otel,
}

/// Where a kind's writes go.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Log {
    /// Every write is a replicated claim.
    #[default]
    Claims,
    /// Every write goes to the local observation log; a claim replicates when the state changes.
    OnChange,
    /// Only the local observation log of the node that made it.
    Local,
    /// Local when the system writes it without an actor, replicated otherwise.
    SystemLocal,
}

impl Log {
    /// The registry's storage for this log.
    pub fn retention(self) -> Retention {
        match self {
            Log::Claims => Retention::Durable,
            Log::OnChange => Retention::Latest,
            Log::Local => Retention::Local,
            Log::SystemLocal => Retention::SystemLocal,
        }
    }
}

/// One kind's entry in the policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct KindPolicy {
    pub class: Class,
    #[serde(default)]
    pub log: Log,
    /// How long a transition stays in the replicated log, such as `7d`.
    #[serde(default)]
    pub window: Option<String>,
    /// Why a kind whose class allows dropping has no checkpoint rule yet.
    #[serde(default)]
    pub pending: Option<String>,
}

/// One checkpoint rule. It renders as one line of the rules description.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RulePolicy {
    pub kind: String,
    /// Conditions on the claim, all of which must hold: `actor=null`, `FIELD=set` or
    /// `FIELD=VALUE`.
    #[serde(default)]
    pub when: Vec<String>,
    /// The fields besides subject and kind that make up a slot. `claim.origin` is the writer.
    /// Without a slot, the rule keeps every claim.
    #[serde(default)]
    pub slot: Option<Vec<String>>,
    /// A claim missing a slot field takes no part in the rule.
    #[serde(default)]
    pub require_slot: bool,
    /// Description terms written after the slot.
    #[serde(default)]
    pub after_slot: Vec<String>,
    /// What the rule keeps; it names the planner function.
    pub keep: String,
    /// How long before the cut a claim must be accepted before the rule drops it, such as `5d`.
    #[serde(default)]
    pub min_age: Option<String>,
    /// The rule is left out of the rules description. Only for a rule that predates its line;
    /// adding one changes the rules digest.
    #[serde(default)]
    pub undescribed: bool,
}

/// The whole policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub kinds: BTreeMap<String, KindPolicy>,
    #[serde(default)]
    pub rules: Vec<RulePolicy>,
}

const DAY_MS: u128 = 24 * 60 * 60 * 1000;

/// Milliseconds in a whole number of days written as `Nd`.
pub fn days_ms(value: &str) -> Result<u128, String> {
    value
        .strip_suffix('d')
        .and_then(|days| days.parse::<u128>().ok())
        .map(|days| days * DAY_MS)
        .ok_or_else(|| format!("`{value}` is not a whole number of days such as `7d`"))
}

impl Policy {
    /// Parse and check a policy.
    pub fn parse(text: &str) -> Result<Self, String> {
        let policy: Policy = toml::from_str(text).map_err(|error| error.to_string())?;
        for (kind, entry) in &policy.kinds {
            if let Some(window) = &entry.window {
                days_ms(window).map_err(|error| format!("{kind}: {error}"))?;
            }
            if entry.class == Class::Transition && entry.window.is_none() {
                return Err(format!("{kind}: a transition needs a window"));
            }
            if entry.class == Class::Durable && entry.pending.is_some() {
                return Err(format!("{kind}: a durable kind is never dropped, so nothing is pending"));
            }
        }
        for rule in &policy.rules {
            if let Some(min_age) = &rule.min_age {
                days_ms(min_age).map_err(|error| format!("{} rule: {error}", rule.kind))?;
            }
            for condition in &rule.when {
                if !condition.contains('=') {
                    return Err(format!("{} rule: `{condition}` is not NAME=VALUE", rule.kind));
                }
            }
        }
        Ok(policy)
    }

    /// The kind's entry. A custom kind is durable and replicated.
    pub fn kind(&self, kind: &str) -> KindPolicy {
        self.kinds.get(kind).cloned().unwrap_or(KindPolicy {
            class: Class::Durable,
            log: Log::Claims,
            window: None,
            pending: None,
        })
    }

    /// The rules for one kind, in policy order.
    pub fn rules_for<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a RulePolicy> + 'a {
        self.rules.iter().filter(move |rule| rule.kind == kind)
    }

    /// One line per described rule, in order: the kind, its conditions, its slot, the terms after
    /// the slot, what it keeps and its minimum age.
    pub fn rules_description(&self) -> String {
        let mut lines = Vec::new();
        for rule in self.rules.iter().filter(|rule| !rule.undescribed) {
            let mut terms = vec![rule.kind.clone()];
            terms.extend(rule.when.iter().cloned());
            if let Some(slot) = &rule.slot {
                let mut names = vec!["subject".to_owned()];
                names.extend(
                    slot.iter()
                        .map(|name| name.strip_prefix("claim.").unwrap_or(name).to_owned()),
                );
                terms.push(format!("slot={}", names.join(",")));
            }
            terms.extend(rule.after_slot.iter().cloned());
            terms.push(format!("keep={}", rule.keep));
            if let Some(min_age) = &rule.min_age {
                terms.push(format!("min-age-before-cut={min_age}"));
            }
            lines.push(terms.join(" "));
        }
        lines.join("\n")
    }
}

/// The policy this build runs.
pub fn policy() -> &'static Policy {
    static POLICY: OnceLock<Policy> = OnceLock::new();
    POLICY.get_or_init(|| {
        Policy::parse(POLICY_TOML).unwrap_or_else(|error| panic!("retention.toml: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_kind_is_named_once_and_nothing_else_is() {
        let registry = crate::registry();
        let named = policy().kinds.keys().collect::<Vec<_>>();
        let registered = registry.claims.keys().collect::<Vec<_>>();
        assert_eq!(named, registered, "retention.toml must name exactly the registered kinds");
        for rule in &policy().rules {
            assert!(
                rule.kind == "seat.status-history" || registry.claims.contains_key(&rule.kind),
                "a rule names an unregistered kind {}",
                rule.kind
            );
        }
    }

    #[test]
    fn a_kind_is_dropped_only_where_its_class_allows() {
        for rule in &policy().rules {
            if rule.kind == "seat.status-history" || rule.slot.is_none() {
                continue;
            }
            let entry = policy().kind(&rule.kind);
            assert_ne!(entry.class, Class::Durable, "{} is durable but has a rule", rule.kind);
            assert!(entry.pending.is_none(), "{} has a rule, so nothing is pending", rule.kind);
        }
        for (kind, entry) in &policy().kinds {
            let dropping = policy().rules_for(kind).any(|rule| rule.slot.is_some());
            if entry.class != Class::Durable && entry.log == Log::Claims {
                assert!(
                    dropping || entry.pending.is_some(),
                    "{kind} is {:?} but no rule drops it and nothing says why",
                    entry.class
                );
            }
        }
    }

    /// The storage each kind had before the policy file named it.
    #[test]
    fn the_policy_keeps_every_kinds_storage() {
        for (kind, spec) in &crate::registry().claims {
            let before = match kind.as_str() {
                "harness.timeline" | "harness.telemetry" | "render.applied"
                | "runtime.readiness-deadline-reached" | "sekret.called" | "sekret.exited"
                | "sekret.refused" | "sekret.changed" => Retention::Local,
                "runtime.action.requested" | "runtime.action.succeeded" | "runtime.action.failed"
                | "runtime.action.deadline-reached" => Retention::SystemLocal,
                "harness.observed" | "harness.usage" | "harness.todo.observed"
                | "workspace.observed" => Retention::Latest,
                _ => Retention::Durable,
            };
            assert_eq!(spec.retention, before, "{kind}");
        }
    }

    #[test]
    fn durations_are_whole_days() {
        assert_eq!(days_ms("7d").unwrap(), 7 * DAY_MS);
        assert!(days_ms("7h").is_err());
        assert!(Policy::parse("[kinds]\n\"a.b\" = { class = \"transition\" }\n").is_err());
    }
}
