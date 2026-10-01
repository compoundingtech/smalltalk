//! The membership fold. It turns admitted `fleet.*` claims into incarnations and writer windows.
//!
//! A membership claim counts only when the envelope that carries it is signed by a member whose
//! window holds that envelope's sequence, or, for the anchor's own admission, by the pinned
//! anchor key. Which members exist depends on which claims count, so the fold iterates to a
//! fixed point from the anchor. Claims that contradict each other (two members removing each
//! other concurrently) can make the iteration alternate between two sets; the fold then keeps
//! only the claims both sets agree on. The answer depends only on the set of claims and the
//! anchor, never on receipt order or clocks.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

pub const MEMBER_ADMITTED: &str = "fleet.member-admitted";
pub const MEMBER_ENDPOINTS: &str = "fleet.member-endpoints";
pub const MEMBER_LEFT: &str = "fleet.member-left";
pub const MEMBER_REMOVED: &str = "fleet.member-removed";

/// One admitted `fleet.*` claim and what is known about the envelope that carries it.
#[derive(Clone, Debug, PartialEq)]
pub struct FleetClaim {
    pub id: String,
    pub kind: String,
    pub subject: String,
    pub fields: BTreeMap<String, Value>,
    /// The writer (origin) of the envelope that carries the claim.
    pub writer: String,
    /// That envelope's writer sequence.
    pub sequence: u64,
    /// Member keys with a verified signature on that envelope.
    pub signers: BTreeSet<String>,
}

impl FleetClaim {
    fn name(&self) -> Option<&str> {
        self.subject.strip_prefix("host/")
    }

    fn text(&self, field: &str) -> Option<&str> {
        self.fields.get(field).and_then(Value::as_str)
    }

    fn number(&self, field: &str) -> Option<u64> {
        self.fields.get(field).and_then(Value::as_u64)
    }
}

/// One member key under one node name, and the window of that writer's sequences it signs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Incarnation {
    pub name: String,
    pub member_key: String,
    pub via: String,
    pub mode: String,
    pub sponsor: Option<String>,
    pub admitted_claim: String,
    /// The first sequence in the window: one past the admission's writer floor.
    pub start: u64,
    /// The last sequence in the window, set by a removal or a leave.
    pub end: Option<u64>,
    /// `removed` or `left` once the incarnation has ended.
    pub ended: Option<String>,
    pub endpoints: Vec<Value>,
    endpoints_sequence: Option<u64>,
}

impl Incarnation {
    fn holds(&self, sequence: u64) -> bool {
        sequence >= self.start && self.end.is_none_or(|end| sequence <= end)
    }
}

/// Which signature, if any, admits an envelope at one writer sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Window {
    /// The writer has no key for this sequence: admit it unsigned, as before membership.
    Legacy,
    /// Admit it only with a signature by one of these keys.
    Keyed(BTreeSet<String>),
    /// No incarnation of the writer holds this sequence.
    Fenced,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberState<'a> {
    Current(&'a Incarnation),
    Conflicted(Vec<&'a Incarnation>),
    /// Every incarnation has ended; this is the latest.
    Ended(&'a Incarnation),
    /// A config peer that was never a member, removed at this high water.
    LegacyRemoved(u64),
    NotMember,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Membership {
    anchor: Option<String>,
    incarnations: BTreeMap<String, Vec<Incarnation>>,
    legacy_ends: BTreeMap<String, u64>,
    valid: BTreeSet<String>,
}

impl Membership {
    /// Fold admitted `fleet.*` claims from a pinned anchor key. Without an anchor, nothing
    /// counts, every writer is legacy, and the node replicates as it did before membership.
    pub fn fold(anchor: Option<&str>, claims: &[FleetClaim]) -> Self {
        let mut claims = claims
            .iter()
            .filter(|claim| claim.kind.starts_with("fleet."))
            .collect::<Vec<_>>();
        claims.sort_by(|left, right| left.id.cmp(&right.id));
        let Some(anchor) = anchor else {
            return Self::default();
        };
        let limit = claims.len().saturating_mul(2).saturating_add(4);
        let mut previous: Option<BTreeSet<String>> = None;
        let mut current = BTreeSet::new();
        for _ in 0..limit {
            let next = valid_claims(anchor, &claims, &Self::build(anchor, &claims, &current));
            if next == current {
                return Self::build(anchor, &claims, &current);
            }
            if previous.as_ref() == Some(&next) {
                break;
            }
            previous = Some(std::mem::replace(&mut current, next));
        }
        // The claims alternate between two sets. Keep what both agree on.
        let agreed = match previous {
            Some(previous) => current.intersection(&previous).cloned().collect(),
            None => current,
        };
        Self::build(anchor, &claims, &agreed)
    }

    fn build(anchor: &str, claims: &[&FleetClaim], valid: &BTreeSet<String>) -> Self {
        let mut membership = Self {
            anchor: Some(anchor.to_owned()),
            valid: valid.clone(),
            ..Self::default()
        };
        let counted = claims
            .iter()
            .filter(|claim| valid.contains(&claim.id))
            .collect::<Vec<_>>();
        for claim in counted.iter().filter(|claim| claim.kind == MEMBER_ADMITTED) {
            let (Some(name), Some(key)) = (claim.name(), claim.text("member_key")) else {
                continue;
            };
            let incarnations = membership.incarnations.entry(name.to_owned()).or_default();
            if incarnations.iter().any(|known| known.member_key == key) {
                continue;
            }
            incarnations.push(Incarnation {
                name: name.to_owned(),
                member_key: key.to_owned(),
                via: claim.text("via").unwrap_or_default().to_owned(),
                mode: claim.text("mode").unwrap_or("listening").to_owned(),
                sponsor: claim.text("sponsor").map(str::to_owned),
                admitted_claim: claim.id.clone(),
                start: claim.number("writer_floor").unwrap_or(0).saturating_add(1),
                end: None,
                ended: None,
                endpoints: Vec::new(),
                endpoints_sequence: None,
            });
        }
        for claim in &counted {
            let Some(name) = claim.name() else {
                continue;
            };
            match claim.kind.as_str() {
                MEMBER_REMOVED | MEMBER_LEFT => {
                    let Some(high_water) = claim.number("high_water") else {
                        continue;
                    };
                    let ended = if claim.kind == MEMBER_LEFT {
                        "left"
                    } else {
                        "removed"
                    };
                    match claim.text("member_key") {
                        Some(key) => {
                            if let Some(incarnation) = membership
                                .incarnations
                                .get_mut(name)
                                .and_then(|all| all.iter_mut().find(|i| i.member_key == key))
                            {
                                incarnation.end = Some(
                                    incarnation
                                        .end
                                        .map_or(high_water, |end| end.max(high_water)),
                                );
                                if incarnation.ended.as_deref() != Some("removed") {
                                    incarnation.ended = Some(ended.to_owned());
                                }
                            }
                        }
                        None => {
                            let end = membership.legacy_ends.entry(name.to_owned()).or_default();
                            *end = (*end).max(high_water);
                        }
                    }
                }
                MEMBER_ENDPOINTS => {
                    let Some(key) = claim.text("member_key") else {
                        continue;
                    };
                    if let Some(incarnation) = membership
                        .incarnations
                        .get_mut(name)
                        .and_then(|all| all.iter_mut().find(|i| i.member_key == key))
                        && incarnation
                            .endpoints_sequence
                            .is_none_or(|sequence| claim.sequence > sequence)
                    {
                        incarnation.endpoints_sequence = Some(claim.sequence);
                        incarnation.endpoints = claim
                            .fields
                            .get("endpoints")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        if let Some(mode) = claim.text("mode") {
                            incarnation.mode = mode.to_owned();
                        }
                    }
                }
                _ => {}
            }
        }
        membership
    }

    pub fn anchor(&self) -> Option<&str> {
        self.anchor.as_deref()
    }

    /// Whether a `fleet.*` claim counts.
    pub fn counts(&self, claim_id: &str) -> bool {
        self.valid.contains(claim_id)
    }

    pub fn incarnations(&self) -> impl Iterator<Item = &Incarnation> {
        self.incarnations.values().flatten()
    }

    /// The keys whose incarnation windows hold this writer sequence.
    pub fn keys_for(&self, writer: &str, sequence: u64) -> BTreeSet<String> {
        self.incarnations
            .get(writer)
            .into_iter()
            .flatten()
            .filter(|incarnation| incarnation.holds(sequence))
            .map(|incarnation| incarnation.member_key.clone())
            .collect()
    }

    /// Config peers that were never members and were removed.
    pub fn legacy_removed_names(&self) -> impl Iterator<Item = String> + '_ {
        self.legacy_ends.keys().cloned()
    }

    /// Whether this writer has ever had a key.
    pub fn is_keyed_writer(&self, writer: &str) -> bool {
        self.incarnations
            .get(writer)
            .is_some_and(|all| !all.is_empty())
    }

    pub fn window(&self, writer: &str, sequence: u64) -> Window {
        let keys = self.keys_for(writer, sequence);
        if !keys.is_empty() {
            return Window::Keyed(keys);
        }
        let incarnations = self.incarnations.get(writer);
        if incarnations.is_some_and(|all| all.iter().any(|incarnation| incarnation.start <= 1)) {
            return Window::Fenced;
        }
        // The legacy window runs from 1 to the first keyed incarnation's floor, or to the
        // high water of a removal without a key, whichever comes first.
        let legacy_end = [
            self.legacy_ends.get(writer).copied(),
            incarnations
                .and_then(|all| all.iter().map(|incarnation| incarnation.start).min())
                .map(|start| start.saturating_sub(1)),
        ]
        .into_iter()
        .flatten()
        .min();
        match legacy_end {
            Some(end) if sequence > end => Window::Fenced,
            _ => Window::Legacy,
        }
    }

    pub fn state(&self, name: &str) -> MemberState<'_> {
        let incarnations = self
            .incarnations
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let current = incarnations
            .iter()
            .filter(|incarnation| incarnation.end.is_none())
            .collect::<Vec<_>>();
        match current.len() {
            1 => MemberState::Current(current[0]),
            0 => match incarnations
                .iter()
                .max_by_key(|incarnation| incarnation.start)
            {
                Some(latest) => MemberState::Ended(latest),
                None => match self.legacy_ends.get(name) {
                    Some(high_water) => MemberState::LegacyRemoved(*high_water),
                    None => MemberState::NotMember,
                },
            },
            _ => MemberState::Conflicted(current),
        }
    }
}

/// The claims that count when `membership` is the current view.
fn valid_claims(anchor: &str, claims: &[&FleetClaim], membership: &Membership) -> BTreeSet<String> {
    claims
        .iter()
        .filter(|claim| claim_counts(anchor, claim, membership))
        .map(|claim| claim.id.clone())
        .collect()
}

fn claim_counts(anchor: &str, claim: &FleetClaim, membership: &Membership) -> bool {
    let name = claim.name();
    if claim.kind == MEMBER_ADMITTED && claim.text("via") == Some("anchor") {
        // The anchor's own admission is the root: its key must be the pinned anchor key, it
        // must admit its own writer, and its envelope must be signed by that key inside the
        // window it defines.
        let start = claim.number("writer_floor").unwrap_or(0).saturating_add(1);
        let end = membership
            .incarnations
            .get(claim.writer.as_str())
            .and_then(|all| all.iter().find(|i| i.member_key == anchor))
            .and_then(|incarnation| incarnation.end);
        return claim.text("member_key") == Some(anchor)
            && name == Some(claim.writer.as_str())
            && claim.signers.contains(anchor)
            && claim.sequence >= start
            && end.is_none_or(|end| claim.sequence <= end);
    }
    let keys = membership.keys_for(&claim.writer, claim.sequence);
    let signed = claim.signers.iter().any(|signer| keys.contains(signer));
    if !signed {
        return false;
    }
    match claim.kind.as_str() {
        // Only a member may say where it listens or that it leaves, with its own key.
        MEMBER_ENDPOINTS | MEMBER_LEFT => claim.text("member_key").is_some_and(|key| {
            name == Some(claim.writer.as_str()) && keys.contains(key) && claim.signers.contains(key)
        }),
        // A member is admitted by another member, never by itself (except the anchor).
        MEMBER_ADMITTED => name.is_some_and(|name| name != claim.writer),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claim(
        id: &str,
        kind: &str,
        subject: &str,
        fields: Value,
        writer: &str,
        sequence: u64,
        signers: &[&str],
    ) -> FleetClaim {
        FleetClaim {
            id: id.into(),
            kind: kind.into(),
            subject: subject.into(),
            fields: serde_json::from_value(fields).unwrap(),
            writer: writer.into(),
            sequence,
            signers: signers.iter().map(|signer| (*signer).into()).collect(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn admitted(
        id: &str,
        name: &str,
        key: &str,
        via: &str,
        writer: &str,
        sequence: u64,
        signer: &str,
        floor: Option<u64>,
    ) -> FleetClaim {
        let mut fields = json!({
            "fleet_id": "fleet",
            "member_key": key,
            "via": via,
            "mode": "listening",
        });
        if let Some(floor) = floor {
            fields["writer_floor"] = json!(floor);
        }
        claim(
            id,
            MEMBER_ADMITTED,
            &format!("host/{name}"),
            fields,
            writer,
            sequence,
            &[signer],
        )
    }

    fn removed(
        id: &str,
        name: &str,
        key: Option<&str>,
        high_water: u64,
        writer: &str,
        sequence: u64,
        signer: &str,
    ) -> FleetClaim {
        let mut fields = json!({"high_water": high_water, "reason": "test"});
        if let Some(key) = key {
            fields["member_key"] = json!(key);
        }
        claim(
            id,
            MEMBER_REMOVED,
            &format!("host/{name}"),
            fields,
            writer,
            sequence,
            &[signer],
        )
    }

    /// The anchor `a` admits `b` and `r`; the base of most tests.
    fn base() -> Vec<FleetClaim> {
        vec![
            admitted("1-anchor", "a", "ka", "anchor", "a", 3, "ka", None),
            admitted("2-b", "b", "kb", "invite", "a", 4, "ka", None),
            admitted("3-r", "r", "kr", "invite", "a", 5, "ka", None),
        ]
    }

    #[test]
    fn without_an_anchor_nothing_counts_and_every_writer_is_legacy() {
        let membership = Membership::fold(None, &base());
        assert!(!membership.counts("1-anchor"));
        assert_eq!(membership.window("a", 10), Window::Legacy);
        assert_eq!(membership.state("b"), MemberState::NotMember);
    }

    #[test]
    fn membership_grows_from_the_anchor() {
        let membership = Membership::fold(Some("ka"), &base());
        for id in ["1-anchor", "2-b", "3-r"] {
            assert!(membership.counts(id), "{id}");
        }
        assert!(matches!(membership.state("b"), MemberState::Current(i) if i.member_key == "kb"));
        assert_eq!(
            membership.window("b", 1),
            Window::Keyed(BTreeSet::from(["kb".into()]))
        );
        assert_eq!(membership.window("legacy-peer", 1), Window::Legacy);
    }

    #[test]
    fn only_the_pinned_anchor_key_can_self_admit() {
        let mut claims = base();
        claims.push(admitted(
            "4-ghost", "ghost", "kg", "anchor", "ghost", 1, "kg", None,
        ));
        claims.push(admitted("5-self", "c", "kc", "invite", "c", 1, "kc", None));
        let membership = Membership::fold(Some("ka"), &claims);
        assert!(!membership.counts("4-ghost"));
        assert!(!membership.counts("5-self"));
        assert_eq!(membership.state("ghost"), MemberState::NotMember);
        // Another anchor pin sees no fleet at all from these claims.
        let other = Membership::fold(Some("kg"), &claims);
        assert!(!other.counts("1-anchor"));
        assert!(other.counts("4-ghost"));
    }

    #[test]
    fn membership_claims_count_only_when_signed_by_a_current_incarnation_in_its_window() {
        let mut claims = base();
        // r is removed at high water 20; afterwards it admits a ghost and removes b.
        claims.push(removed("6-remove-r", "r", Some("kr"), 20, "a", 9, "ka"));
        claims.push(admitted(
            "7-ghost", "ghost", "kg", "invite", "r", 21, "kr", None,
        ));
        claims.push(removed("8-r-removes-b", "b", Some("kb"), 1, "r", 22, "kr"));
        // An unsigned admission and one signed with another member's key do not count.
        claims.push(admitted(
            "9-unsigned",
            "x",
            "kx",
            "invite",
            "a",
            10,
            "nobody",
            None,
        ));
        claims.push(admitted(
            "10-forged",
            "y",
            "ky",
            "invite",
            "a",
            11,
            "kr",
            None,
        ));
        let membership = Membership::fold(Some("ka"), &claims);
        assert!(membership.counts("6-remove-r"));
        for id in ["7-ghost", "8-r-removes-b", "9-unsigned", "10-forged"] {
            assert!(!membership.counts(id), "{id}");
        }
        assert!(matches!(membership.state("r"), MemberState::Ended(i) if i.end == Some(20)));
        assert!(matches!(membership.state("b"), MemberState::Current(_)));
        assert_eq!(membership.state("ghost"), MemberState::NotMember);
        assert_eq!(
            membership.window("r", 20),
            Window::Keyed(BTreeSet::from(["kr".into()]))
        );
        assert_eq!(membership.window("r", 21), Window::Fenced);
    }

    #[test]
    fn a_stale_admission_cannot_undo_a_removal_of_the_same_key() {
        let mut claims = base();
        claims.push(removed("6-remove-r", "r", Some("kr"), 20, "a", 9, "ka"));
        claims.push(admitted("7-again", "r", "kr", "invite", "b", 3, "kb", None));
        let membership = Membership::fold(Some("ka"), &claims);
        assert!(matches!(membership.state("r"), MemberState::Ended(_)));
    }

    #[test]
    fn joining_again_with_a_new_key_is_a_new_incarnation_above_the_floor() {
        let mut claims = base();
        claims.push(removed("6-remove-r", "r", Some("kr"), 20, "a", 9, "ka"));
        claims.push(admitted(
            "7-rejoin",
            "r",
            "kr2",
            "invite",
            "a",
            10,
            "ka",
            Some(25),
        ));
        let membership = Membership::fold(Some("ka"), &claims);
        assert!(matches!(membership.state("r"), MemberState::Current(i) if i.member_key == "kr2"));
        assert_eq!(
            membership.window("r", 20),
            Window::Keyed(BTreeSet::from(["kr".into()]))
        );
        assert_eq!(membership.window("r", 21), Window::Fenced);
        assert_eq!(membership.window("r", 25), Window::Fenced);
        assert_eq!(
            membership.window("r", 26),
            Window::Keyed(BTreeSet::from(["kr2".into()]))
        );
    }

    #[test]
    fn endpoints_and_leave_count_only_from_the_members_own_writer() {
        let mut claims = base();
        let endpoints = |id: &str, writer: &str, signer: &str, sequence: u64, mode: &str| {
            claim(
                id,
                MEMBER_ENDPOINTS,
                "host/b",
                json!({"member_key": "kb", "mode": mode, "endpoints": [{"transport": "loopback"}]}),
                writer,
                sequence,
                &[signer],
            )
        };
        claims.push(endpoints("6-own", "b", "kb", 2, "listening"));
        claims.push(endpoints("7-newer", "b", "kb", 3, "dial-out"));
        claims.push(endpoints("8-other", "a", "ka", 20, "listening"));
        claims.push(claim(
            "9-left-by-other",
            MEMBER_LEFT,
            "host/b",
            json!({"member_key": "kb", "high_water": 4}),
            "a",
            21,
            &["ka"],
        ));
        let membership = Membership::fold(Some("ka"), &claims);
        assert!(membership.counts("6-own"));
        assert!(membership.counts("7-newer"));
        assert!(!membership.counts("8-other"));
        assert!(!membership.counts("9-left-by-other"));
        let MemberState::Current(b) = membership.state("b") else {
            panic!("b is current");
        };
        assert_eq!(b.mode, "dial-out");
        assert_eq!(b.endpoints.len(), 1);

        claims.push(claim(
            "10-left",
            MEMBER_LEFT,
            "host/b",
            json!({"member_key": "kb", "high_water": 4}),
            "b",
            4,
            &["kb"],
        ));
        let membership = Membership::fold(Some("ka"), &claims);
        assert!(
            matches!(membership.state("b"), MemberState::Ended(i) if i.ended.as_deref() == Some("left"))
        );
    }

    #[test]
    fn two_current_keys_for_one_name_are_conflicted() {
        let mut claims = base();
        claims.push(admitted(
            "6-second", "b", "kb2", "invite", "a", 12, "ka", None,
        ));
        let membership = Membership::fold(Some("ka"), &claims);
        assert!(matches!(membership.state("b"), MemberState::Conflicted(all) if all.len() == 2));
    }

    #[test]
    fn a_legacy_writer_is_legacy_until_a_keyless_removal_ends_its_window() {
        let mut claims = base();
        assert_eq!(
            Membership::fold(Some("ka"), &claims).window("old", 500),
            Window::Legacy
        );
        claims.push(removed("6-remove-old", "old", None, 40, "a", 9, "ka"));
        let membership = Membership::fold(Some("ka"), &claims);
        assert_eq!(membership.window("old", 40), Window::Legacy);
        assert_eq!(membership.window("old", 41), Window::Fenced);
        assert_eq!(membership.state("old"), MemberState::LegacyRemoved(40));
        // A legacy writer joining again keeps its legacy history and gets a keyed window.
        claims.push(admitted(
            "7-old-joins",
            "old",
            "ko",
            "invite",
            "a",
            10,
            "ka",
            Some(45),
        ));
        let membership = Membership::fold(Some("ka"), &claims);
        assert_eq!(membership.window("old", 40), Window::Legacy);
        assert_eq!(membership.window("old", 43), Window::Fenced);
        assert_eq!(
            membership.window("old", 46),
            Window::Keyed(BTreeSet::from(["ko".into()]))
        );
    }

    #[test]
    fn a_migrated_writer_signs_its_whole_history() {
        let mut claims = base();
        claims.push(admitted(
            "6-migrated",
            "m",
            "km",
            "migration",
            "a",
            12,
            "ka",
            None,
        ));
        let membership = Membership::fold(Some("ka"), &claims);
        assert_eq!(
            membership.window("m", 1),
            Window::Keyed(BTreeSet::from(["km".into()]))
        );
    }

    #[test]
    fn a_removal_the_remover_saw_before_wins_and_concurrent_mutual_removals_cancel() {
        // b saw r remove it (r's claim at 6 is within b's view of r, 6) before b removed r:
        // b's removal comes after b's own end, so only r's removal counts.
        let mut ordered = base();
        ordered.push(removed("6-r-removes-b", "b", Some("kb"), 2, "r", 6, "kr"));
        ordered.push(removed("7-b-removes-r", "r", Some("kr"), 6, "b", 3, "kb"));
        let membership = Membership::fold(Some("ka"), &ordered);
        assert!(membership.counts("6-r-removes-b"));
        assert!(!membership.counts("7-b-removes-r"));
        assert!(matches!(membership.state("b"), MemberState::Ended(_)));
        assert!(matches!(membership.state("r"), MemberState::Current(_)));

        // Neither saw the other's removal: neither counts.
        let mut concurrent = base();
        concurrent.push(removed("6-r-removes-b", "b", Some("kb"), 2, "r", 6, "kr"));
        concurrent.push(removed("7-b-removes-r", "r", Some("kr"), 5, "b", 3, "kb"));
        let membership = Membership::fold(Some("ka"), &concurrent);
        assert!(!membership.counts("6-r-removes-b"));
        assert!(!membership.counts("7-b-removes-r"));
        assert!(matches!(membership.state("b"), MemberState::Current(_)));
        assert!(matches!(membership.state("r"), MemberState::Current(_)));
    }

    #[test]
    fn the_fold_is_the_same_in_every_receipt_order() {
        let mut claims = base();
        claims.push(removed("6-remove-r", "r", Some("kr"), 20, "a", 9, "ka"));
        claims.push(admitted(
            "7-ghost", "ghost", "kg", "invite", "r", 21, "kr", None,
        ));
        claims.push(admitted("8-c", "c", "kc", "invite", "b", 2, "kb", Some(3)));
        claims.push(removed("9-r-removes-b", "b", Some("kb"), 1, "r", 22, "kr"));
        let expected = Membership::fold(Some("ka"), &claims);
        // Rotations and reversals cover every claim in every position.
        for rotation in 0..claims.len() {
            let mut order = claims.clone();
            order.rotate_left(rotation);
            assert_eq!(Membership::fold(Some("ka"), &order), expected);
            order.reverse();
            assert_eq!(Membership::fold(Some("ka"), &order), expected);
        }
    }
}
