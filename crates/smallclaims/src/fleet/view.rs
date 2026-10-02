//! The membership view the replication worker reads from the main daemon, and the rules for
//! who may exchange with this node.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::membership::{MemberState, Membership};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct FleetView {
    pub anchor: Option<String>,
    pub members: Vec<MemberView>,
    /// Config peers that were never members, removed at a high water.
    #[serde(default)]
    pub legacy_removed: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemberView {
    pub name: String,
    pub member_key: String,
    /// `current`, `ended`, or `conflicted`.
    pub state: String,
    pub mode: String,
    #[serde(default)]
    pub endpoints: Vec<Value>,
    pub start: u64,
    #[serde(default)]
    pub end: Option<u64>,
    /// `removed` or `left` once ended.
    #[serde(default)]
    pub ended: Option<String>,
    /// Who removed this member, and why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removal_reason: Option<String>,
}

impl FleetView {
    pub fn from_membership(membership: &Membership) -> Self {
        let mut members = Vec::new();
        let mut legacy_removed = Vec::new();
        let names = membership
            .incarnations()
            .map(|incarnation| incarnation.name.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for name in &names {
            let state = membership.state(name);
            for incarnation in membership
                .incarnations()
                .filter(|incarnation| incarnation.name == *name)
            {
                let label = match &state {
                    MemberState::Conflicted(current)
                        if current
                            .iter()
                            .any(|i| i.member_key == incarnation.member_key) =>
                    {
                        "conflicted"
                    }
                    _ if incarnation.end.is_none() => "current",
                    _ => "ended",
                };
                members.push(MemberView {
                    name: name.clone(),
                    member_key: incarnation.member_key.clone(),
                    state: label.into(),
                    mode: incarnation.mode.clone(),
                    endpoints: incarnation.endpoints.clone(),
                    start: incarnation.start,
                    end: incarnation.end,
                    ended: incarnation.ended.clone(),
                    removed_by: incarnation.removed_by.clone(),
                    removal_reason: incarnation.removal_reason.clone(),
                });
            }
        }
        for name in membership.legacy_removed_names() {
            if !names.contains(&name) {
                legacy_removed.push(name);
            }
        }
        Self {
            anchor: membership.anchor().map(str::to_owned),
            members,
            legacy_removed,
        }
    }

    pub fn current(&self, name: &str) -> Vec<&MemberView> {
        self.members
            .iter()
            .filter(|member| member.name == name && member.state != "ended")
            .collect()
    }

    fn ended_keys(&self, name: &str) -> Vec<&MemberView> {
        self.members
            .iter()
            .filter(|member| member.name == name && member.state == "ended")
            .collect()
    }
}

/// What a node knows about the sender of one signed message.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Sender {
    pub name: String,
    /// The member key the message presented, if any.
    pub member_key: Option<String>,
    /// Whether the member signature over the message verified against that key.
    pub member_signature_valid: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Acceptance {
    /// A current member proved its key.
    Member,
    /// A config peer accepted on the fleet secret alone.
    Legacy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Refusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    /// The ended key this refusal is about, for `member-removed` and `member-left`.
    pub member_key: Option<String>,
}

/// The acceptance table from `docs/fleet-join.md`. `legacy` is true on a node without
/// `fleet.toml`, or with `legacy_peers = true` during a migration.
pub fn accept(
    view: &FleetView,
    sender: &Sender,
    config_peer: bool,
    legacy: bool,
) -> Result<Acceptance, Refusal> {
    let current = view.current(&sender.name);
    if current.len() > 1 {
        return Err(Refusal {
            status: 409,
            code: "member-conflicted",
            message: format!("`{}` has more than one current member key", sender.name),
            member_key: None,
        });
    }
    if let Some(member) = current.first() {
        if sender.member_signature_valid
            && sender.member_key.as_deref() == Some(member.member_key.as_str())
        {
            return Ok(Acceptance::Member);
        }
        if legacy && config_peer {
            return Ok(Acceptance::Legacy);
        }
        return Err(Refusal {
            status: 401,
            code: "member-signature-required",
            message: format!("`{}` must sign with its member key", sender.name),
            member_key: None,
        });
    }
    let ended = view.ended_keys(&sender.name);
    let removed_legacy = view.legacy_removed.contains(&sender.name);
    let ended_match = match sender.member_key.as_deref() {
        Some(key) => ended
            .iter()
            .find(|member| member.member_key == key)
            .copied(),
        // A message without a key is from the legacy incarnation, or from an old build of
        // the ended one; either way the name has ended.
        None => ended.last().copied(),
    };
    if let Some(member) = ended_match {
        let left = member.ended.as_deref() == Some("left");
        return Err(Refusal {
            status: 403,
            code: if left {
                "member-left"
            } else {
                "member-removed"
            },
            // The removed node keeps this message to say who removed it and why.
            message: match (&member.removed_by, &member.removal_reason) {
                (Some(by), Some(reason)) if !left => format!(
                    "`{}` was removed from this fleet by {by}: {reason}",
                    sender.name
                ),
                _ => format!("`{}` is no longer a member of this fleet", sender.name),
            },
            member_key: Some(member.member_key.clone()),
        });
    }
    if removed_legacy && sender.member_key.is_none() {
        return Err(Refusal {
            status: 403,
            code: "member-removed",
            message: format!("`{}` was removed from this fleet", sender.name),
            member_key: None,
        });
    }
    if config_peer && legacy && ended.is_empty() && !removed_legacy {
        return Ok(Acceptance::Legacy);
    }
    Err(Refusal {
        status: 403,
        code: "not-a-member",
        message: format!("`{}` is not a member of this fleet", sender.name),
        member_key: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str, key: &str, state: &str, ended: Option<&str>) -> MemberView {
        MemberView {
            name: name.into(),
            member_key: key.into(),
            state: state.into(),
            mode: "listening".into(),
            endpoints: Vec::new(),
            start: 1,
            end: ended.map(|_| 10),
            ended: ended.map(str::to_owned),
            removed_by: None,
            removal_reason: None,
        }
    }

    fn sender(name: &str, key: Option<&str>, valid: bool) -> Sender {
        Sender {
            name: name.into(),
            member_key: key.map(str::to_owned),
            member_signature_valid: valid,
        }
    }

    fn view() -> FleetView {
        FleetView {
            anchor: Some("ka".into()),
            members: vec![
                member("a", "ka", "current", None),
                member("gone", "kg", "ended", Some("removed")),
                member("away", "kw", "ended", Some("left")),
                member("twin", "kt1", "conflicted", None),
                member("twin", "kt2", "conflicted", None),
            ],
            legacy_removed: vec!["old".into()],
        }
    }

    #[test]
    fn a_member_request_needs_a_valid_member_signature() {
        let view = view();
        assert_eq!(
            accept(&view, &sender("a", Some("ka"), true), false, false),
            Ok(Acceptance::Member)
        );
        for bad in [
            sender("a", Some("ka"), false),
            sender("a", Some("kg"), true),
            sender("a", None, false),
        ] {
            assert_eq!(
                accept(&view, &bad, false, false).unwrap_err().code,
                "member-signature-required"
            );
        }
    }

    #[test]
    fn a_legacy_config_peer_is_accepted_with_hmac_alone_only_while_legacy_is_on() {
        let view = view();
        let unsigned = sender("peer", None, false);
        assert_eq!(accept(&view, &unsigned, true, true), Ok(Acceptance::Legacy));
        assert_eq!(
            accept(&view, &unsigned, true, false).unwrap_err().code,
            "not-a-member"
        );
        // A migrated member that is still a config peer may fall back until finish.
        let unsigned_member = sender("a", None, false);
        assert_eq!(
            accept(&view, &unsigned_member, true, true),
            Ok(Acceptance::Legacy)
        );
        assert_eq!(
            accept(&view, &unsigned_member, true, false)
                .unwrap_err()
                .code,
            "member-signature-required"
        );
        // A node without membership accepts config peers exactly as before.
        let empty = FleetView::default();
        assert_eq!(
            accept(&empty, &unsigned, true, true),
            Ok(Acceptance::Legacy)
        );
        assert_eq!(
            accept(&empty, &unsigned, false, true).unwrap_err().code,
            "not-a-member"
        );
    }

    #[test]
    fn ended_names_are_refused_with_the_ended_key() {
        let view = view();
        let refusal = accept(&view, &sender("gone", Some("kg"), true), true, true).unwrap_err();
        assert_eq!(refusal.code, "member-removed");
        assert_eq!(refusal.member_key.as_deref(), Some("kg"));
        assert_eq!(
            accept(&view, &sender("gone", None, false), true, true)
                .unwrap_err()
                .code,
            "member-removed"
        );
        assert_eq!(
            accept(&view, &sender("away", Some("kw"), true), false, false)
                .unwrap_err()
                .code,
            "member-left"
        );
        assert_eq!(
            accept(&view, &sender("old", None, false), true, true)
                .unwrap_err()
                .code,
            "member-removed"
        );
        // A key this node has not seen admitted is not told it was removed.
        let rejoined = accept(&view, &sender("gone", Some("new"), true), true, true).unwrap_err();
        assert_eq!(rejoined.code, "not-a-member");
        assert_eq!(rejoined.member_key, None);
    }

    #[test]
    fn a_conflicted_name_is_refused() {
        assert_eq!(
            accept(&view(), &sender("twin", Some("kt1"), true), true, true)
                .unwrap_err()
                .code,
            "member-conflicted"
        );
    }
}
