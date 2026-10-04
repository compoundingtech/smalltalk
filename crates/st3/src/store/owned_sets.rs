//! Owned membership is graph authority. These reads and projections have no inventory store.
use super::*;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Source {
    pub repository: String,
    pub r#ref: String,
    pub sha: String,
    pub sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Member {
    pub kind: String,
    pub claim: String,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Revision {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout: Option<crate::rollout::Policy>,
    pub previous: Option<String>,
    pub source: Source,
    pub bundle_digest: String,
    pub members: BTreeMap<String, Member>,
    pub retired: BTreeMap<String, Member>,
    pub adoptions: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Options {
    pub set: String,
    pub source: Source,
    /// `absent` creates a set; otherwise an exact content revision is required.
    pub expected_set: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout: Option<crate::rollout::Policy>,
    #[serde(default)]
    pub adopt: BTreeSet<String>,
    #[serde(default)]
    pub allow_empty: bool,
    #[serde(default)]
    pub confirm_retire: Option<String>,
    #[serde(default)]
    pub expected_subjects: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Request {
    pub intent: IntentInput,
    pub options: Options,
    pub actor: String,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Preview {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout: Option<crate::rollout::Policy>,
    pub set: String,
    pub previous: Option<String>,
    pub source: Source,
    pub changes: BTreeMap<String, String>,
    pub effects: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rollouts: BTreeMap<String, Value>,
    pub expected_subjects: BTreeMap<String, Vec<String>>,
    pub digest: String,
    pub mass_retirement: bool,
    pub empty: bool,
    pub blockers: Vec<String>,
    pub noop: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct View {
    pub kind: String,
    pub id: String,
    pub revision: String,
    pub claim: String,
    pub updated_at: String,
    pub receipt: Revision,
    pub blockers: Vec<String>,
}

pub(super) struct Plan {
    pub intent: NormalizedIntent,
    pub preview: Preview,
    pub retired: BTreeMap<String, Member>,
    pub adoptions: BTreeMap<String, Vec<String>>,
    bundle_digest: String,
    pub materialize: BTreeSet<String>,
}

pub fn subject(name: &str) -> Result<String, St3Error> {
    let name = name.strip_prefix("owned-set/").unwrap_or(name);
    if name.is_empty()
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || name.chars().any(|c| c.is_whitespace() || c == '@')
    {
        return Err(St3Error::new(
            "invalid-owned-set",
            "set needs a nonempty graph name",
        ));
    }
    Ok(format!("owned-set/{name}"))
}

fn rows(connection: &Connection, at: Option<u64>) -> Result<Vec<View>, St3Error> {
    smallclaims::touched::note_read(|| "kind:owned-set.revised".into());
    let mut statement = connection.prepare(
        "SELECT id,subject,body,accepted_at_unix_ms FROM claims WHERE kind='owned-set.revised' AND store_index<=?1
         AND NOT EXISTS (SELECT 1 FROM replica_records WHERE replica_records.claim_id=claims.id AND replica_records.state='repaired')"
    ).map_err(internal)?;
    let records = statement
        .query_map([at.unwrap_or(u64::MAX).min(i64::MAX as u64)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(internal)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(internal)?;
    let mut views = Vec::new();
    for (claim, id, body, accepted) in records {
        let body: Value = serde_json::from_str(&body).map_err(internal)?;
        let Ok(receipt) = serde_json::from_value::<Revision>(body["fields"]["body"].clone()) else {
            continue;
        };
        let revision = canonical_hash(&receipt).map_err(internal)?;
        if body["fields"]["revision"].as_str() != Some(&revision) {
            continue;
        }
        views.push(View {
            kind: "owned-set".into(),
            id,
            revision,
            claim,
            updated_at: chrono::DateTime::from_timestamp_millis(
                accepted.parse().map_err(internal)?,
            )
            .ok_or_else(|| St3Error::new("invalid-set-time", "invalid receipt time"))?
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            receipt,
            blockers: Vec::new(),
        });
    }
    Ok(views)
}

fn known_members(
    connection: &Connection,
    set: &str,
    at: Option<u64>,
) -> Result<BTreeMap<String, Member>, St3Error> {
    let mut history = rows(connection, at)?
        .into_iter()
        .filter(|v| v.id == set)
        .collect::<Vec<_>>();
    history.sort_by(|a, b| {
        (a.receipt.source.sequence, &a.revision, &a.claim).cmp(&(
            b.receipt.source.sequence,
            &b.revision,
            &b.claim,
        ))
    });
    let mut members = BTreeMap::new();
    for view in history {
        members.extend(view.receipt.members);
        members.extend(view.receipt.retired);
    }
    Ok(members)
}

fn stop_declaration(subject: &str) -> Result<DesiredSubject, St3Error> {
    let source = if let Some(name) = subject.strip_prefix("schedule/") {
        format!(
            "version 2\nschedule {} {{ stop }}\n",
            serde_json::to_string(name).map_err(internal)?
        )
    } else {
        format!(
            "version 2\nstop {}\n",
            serde_json::to_string(subject).map_err(internal)?
        )
    };
    crate::graph::parse_internal_intent(&source, "unused")?
        .subjects
        .remove(subject)
        .ok_or_else(|| St3Error::new("invalid-set-member", "invalid retirement subject"))
}

/// A complete winning membership also retires subjects learned only from losing branches.
/// Such retirement is derived from the winning set claim until the next apply authors a stop.
pub(super) fn effective_members(
    connection: &Connection,
    view: &View,
    at: Option<u64>,
) -> Result<BTreeMap<String, (Member, bool, bool)>, St3Error> {
    let mut members = BTreeMap::new();
    for (subject, member) in known_members(connection, &view.id, at)? {
        if view.receipt.members.contains_key(&subject)
            || view.receipt.retired.contains_key(&subject)
        {
            continue;
        }
        let member = if member.kind == "mission" {
            member
        } else {
            Member {
                revision: desired_revision(&stop_declaration(&subject)?),
                claim: view.claim.clone(),
                kind: member.kind,
            }
        };
        members.insert(subject, (member, true, true));
    }
    members.extend(
        view.receipt
            .members
            .iter()
            .map(|(s, m)| (s.clone(), (m.clone(), false, false))),
    );
    members.extend(
        view.receipt
            .retired
            .iter()
            .map(|(s, m)| (s.clone(), (m.clone(), true, false))),
    );
    Ok(members)
}

fn reference(view: &View) -> String {
    format!("{}@{}", view.id, view.revision)
}

fn claim(
    connection: &Connection,
    id: &str,
    at: Option<u64>,
) -> Result<Option<ClaimRecord>, St3Error> {
    connection.query_row(
        "SELECT id,store_index,batch_id,subject,kind,origin,actor,body,predecessors,accepted_at_unix_ms
         FROM claims WHERE id=?1 AND store_index<=?2 AND NOT EXISTS
         (SELECT 1 FROM replica_records WHERE replica_records.claim_id=claims.id AND replica_records.state='repaired')",
        params![id,at.unwrap_or(u64::MAX).min(i64::MAX as u64)],claim_from_row
    ).optional().map_err(internal)
}

pub(super) fn selected(connection: &Connection, at: Option<u64>) -> Result<Vec<View>, St3Error> {
    let rows = rows(connection, at)?;
    let references: BTreeMap<_, _> = rows.iter().map(|v| (reference(v), v)).collect();
    let mut winners: BTreeMap<String, View> = BTreeMap::new();
    // Fold all admitted revisions, not just leaves. A late low sequence never shadows a high one.
    for view in &rows {
        let key = (view.receipt.source.sequence, &view.revision, &view.claim);
        if winners
            .get(&view.id)
            .is_none_or(|old| key > (old.receipt.source.sequence, &old.revision, &old.claim))
        {
            winners.insert(view.id.clone(), view.clone());
        }
    }
    let ownership: BTreeMap<String, BTreeSet<String>> =
        rows.iter().fold(BTreeMap::new(), |mut owners, v| {
            for s in v.receipt.members.keys().chain(v.receipt.retired.keys()) {
                owners.entry(s.clone()).or_default().insert(v.id.clone());
            }
            owners
        });
    for view in winners.values_mut() {
        let mut cursor = view.clone();
        let mut seen = BTreeSet::new();
        while let Some(previous) = &cursor.receipt.previous {
            if !seen.insert(previous.clone()) {
                view.blockers.push("cyclic previous revision".into());
                break;
            }
            let Some(parent) = references.get(previous) else {
                view.blockers
                    .push(format!("missing previous revision {previous}"));
                break;
            };
            if parent.id != view.id
                || parent.receipt.source.repository != cursor.receipt.source.repository
                || parent.receipt.source.r#ref != cursor.receipt.source.r#ref
                || parent.receipt.source.sequence >= cursor.receipt.source.sequence
            {
                view.blockers.push("invalid source lineage".into());
                break;
            }
            cursor = (*parent).clone();
        }
        for other in &rows {
            if other.id == view.id
                && other.receipt.source.sequence == view.receipt.source.sequence
                && other.revision != view.revision
            {
                view.blockers.push(format!(
                    "conflicting source sequence {}",
                    view.receipt.source.sequence
                ));
            }
            if other.id == view.id
                && (other.receipt.source.repository != view.receipt.source.repository
                    || other.receipt.source.r#ref != view.receipt.source.r#ref)
            {
                view.blockers.push("conflicting source binding".into());
            }
        }
        for (s, m) in view.receipt.members.iter().chain(&view.receipt.retired) {
            if ownership.get(s).is_some_and(|owners| owners.len() > 1) {
                view.blockers.push(format!("ownership conflict: {s}"));
            }
            let Some(c) = claim(connection, &m.claim, at)? else {
                view.blockers
                    .push(format!("missing member reference {}", m.claim));
                continue;
            };
            let retired = view.receipt.retired.contains_key(s);
            let valid = if m.kind == "mission" {
                c.kind == "mission.published"
                    && c.body["revision"].as_str() == Some(&m.revision)
                    && (!retired || c.body["state"] == "retired")
            } else {
                c.kind == "intent.desired"
                    && serde_json::from_value::<DesiredSubject>(c.body.clone()).is_ok_and(
                        |desired| {
                            desired_revision(&desired) == m.revision
                                && desired.subject == *s
                                && desired.owner_run.is_none()
                                && desired.owner_step.is_none()
                                && if retired && m.kind == "schedule" {
                                    desired.kind == "schedule"
                                        && crate::graph::schedule_spec(&desired.desired, "unused")
                                            .is_some_and(|spec| spec.stopped)
                                } else {
                                    desired.kind == if retired { "stop" } else { &m.kind }
                                }
                        },
                    )
            };
            if c.subject != *s || !valid {
                view.blockers.push(format!("invalid member reference {s}"));
            }
        }
        view.blockers.sort();
        view.blockers.dedup();
    }
    Ok(winners.into_values().collect())
}

pub(super) fn owner(
    connection: &Connection,
    member: &str,
    at: Option<u64>,
) -> Result<Option<String>, St3Error> {
    let owners: BTreeSet<_> = rows(connection, at)?
        .into_iter()
        .filter(|v| {
            v.receipt.members.contains_key(member) || v.receipt.retired.contains_key(member)
        })
        .map(|v| v.id)
        .collect();
    if owners.len() > 1 {
        return Err(St3Error::new(
            "owned-set-conflict",
            format!("{member} has conflicting set owners"),
        ));
    }
    Ok(owners.into_iter().next())
}

pub(super) fn refuse_unmanaged(connection: &Connection, member: &str) -> Result<(), St3Error> {
    if let Some(set) = owner(connection, member, None)? {
        return Err(St3Error::new(
            "set-managed-subject",
            format!("{member} is managed by {set}; publish its declaration through that set"),
        ));
    }
    Ok(())
}

/// A member's conflicts come from its owning set, rather than independent declaration leaves.
pub(super) fn conflicts_at(
    connection: &Connection,
    member: &str,
    at: Option<u64>,
) -> Result<Option<Vec<String>>, St3Error> {
    let Some(set) = owner(connection, member, at)? else {
        return Ok(None);
    };
    let view = selected(connection, at)?
        .into_iter()
        .find(|v| v.id == set)
        .ok_or_else(|| St3Error::new("owned-set-pending", "missing owning set"))?;
    Ok(Some(if view.blockers.is_empty() {
        Vec::new()
    } else {
        vec![view.claim]
    }))
}

pub(super) fn guard_member(connection: &Connection, member: &str) -> Result<(), St3Error> {
    if owner(connection, member, None)?.is_none() {
        let staged: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM claims WHERE subject=?1 AND json_extract(body,'$.owned_set') IS NOT NULL)",
            [member], |row|row.get(0)).map_err(internal)?;
        if staged {
            return Err(St3Error::new(
                "owned-set-pending",
                "staged member awaits its owning set revision",
            ));
        }
    }
    if let Some(set) = owner(connection, member, None)? {
        let view = selected(connection, None)?
            .into_iter()
            .find(|v| v.id == set)
            .ok_or_else(|| St3Error::new("owned-set-pending", "missing owning set"))?;
        if !view.blockers.is_empty() {
            return Err(St3Error::new("owned-set-pending", view.blockers.join("; ")));
        }
    }
    Ok(())
}

pub(super) fn guard_mission_start(connection: &Connection, mission: &str) -> Result<(), St3Error> {
    let subject = format!(
        "mission/{}",
        mission.strip_prefix("mission/").unwrap_or(mission)
    );
    guard_member(connection, &subject)?;
    if let Some(set) = owner(connection, &subject, None)? {
        let view = selected(connection, None)?
            .into_iter()
            .find(|v| v.id == set)
            .unwrap();
        if !view.receipt.members.contains_key(&subject) {
            return Err(St3Error::new(
                "mission-retired",
                format!("{subject} was retired by {set}"),
            ));
        }
    }
    Ok(())
}

pub(super) fn plan_tx(
    transaction: &Transaction<'_>,
    input: &NormalizedIntent,
    options: &Options,
) -> Result<Plan, St3Error> {
    let set = subject(&options.set)?;
    if let Some(policy) = &options.rollout {
        policy
            .validate()
            .map_err(|e| St3Error::new("invalid-rollout-policy", e.to_string()))?;
    }
    if options.source.repository.split('/').count() != 2
        || options.source.repository.split('/').any(str::is_empty)
        || !options.source.r#ref.starts_with("refs/heads/")
        || options.source.r#ref.len() == "refs/heads/".len()
        || !matches!(options.source.sha.len(), 40 | 64)
        || !options.source.sha.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(St3Error::new(
            "invalid-set-source",
            "source needs repository, full branch ref and immutable hex SHA",
        ));
    }
    if !input.mission_runs.is_empty()
        || !input.planning_sessions.is_empty()
        || !input.resource_refreshes.is_empty()
        || !input.replica_repairs.is_empty()
        || input.subjects.values().any(|s| {
            !matches!(s.kind.as_str(), "agent" | "schedule")
                || s.owner_run.is_some()
                || s.owner_step.is_some()
        })
    {
        return Err(St3Error::new(
            "unsupported-set-member",
            "owned sets manage top-level agents, missions and schedules only",
        ));
    }
    let old = selected(transaction, None)?
        .into_iter()
        .find(|v| v.id == set);
    let previous = old.as_ref().map(reference);
    let expected_set = if options.expected_set == "absent" {
        None
    } else {
        Some(if options.expected_set.starts_with("owned-set/") {
            options.expected_set.clone()
        } else {
            format!("{set}@{}", options.expected_set)
        })
    };
    let bundle_digest = canonical_hash(&(&input.subjects, &input.missions)).map_err(internal)?;
    let noop = old.as_ref().is_some_and(|v| {
        v.receipt.source == options.source
            && v.receipt.bundle_digest == bundle_digest
            && v.receipt.rollout == options.rollout
    });
    let mut blockers = Vec::new();
    let membership = fleet_membership_tx(transaction).map_err(internal)?;
    for member in membership.incarnations().filter(|m| m.end.is_none()) {
        let supported: Option<bool> = transaction.query_row(&canonical_sql(
            "SELECT json_extract(body,'$.fields.features.owned_sets')=1 FROM claims
             WHERE subject=?1 AND kind='daemon.started' AND origin=?2 ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
            params![format!("daemon/{}",member.name),member.name],|row|row.get(0)).optional().map_err(internal)?.flatten();
        if options.rollout.is_some() {
            let rollout_supported: Option<bool> = transaction.query_row(&canonical_sql(
                "SELECT json_extract(body,'$.fields.features.seat_rollout')=1 FROM claims
                 WHERE subject=?1 AND kind='daemon.started' AND origin=?2 ORDER BY CANONICAL_DESC(claims) LIMIT 1"),
                params![format!("daemon/{}",member.name),member.name],|row|row.get(0)).optional().map_err(internal)?.flatten();
            if rollout_supported != Some(true) {
                blockers.push(format!(
                    "host/{} has not advertised seat-rollout support; upgrade before activation",
                    member.name
                ));
            }
        }
        if supported != Some(true) {
            blockers.push(format!(
                "host/{} has not advertised owned-set support; upgrade before activation",
                member.name
            ));
        }
    }
    if !noop && previous != expected_set {
        blockers.push("selected set revision changed; preview again".into());
    }
    if let Some(old) = &old {
        blockers.extend(
            old.blockers
                .iter()
                .filter(|b| {
                    !(options.source.sequence > old.receipt.source.sequence
                        && b.starts_with("conflicting source sequence "))
                })
                .cloned(),
        );
        if old.receipt.source.repository != options.source.repository
            || old.receipt.source.r#ref != options.source.r#ref
        {
            blockers.push("source repository/ref is fixed at set creation".into());
        }
        if !noop && options.source.sequence <= old.receipt.source.sequence {
            blockers.push(
                "source sequence must increase; equal sequence requires identical SHA and content"
                    .into(),
            );
        }
    }
    let mut intent = input.clone();
    let live: BTreeMap<String, String> = input
        .subjects
        .iter()
        .map(|(s, d)| (s.clone(), desired_revision(d)))
        .chain(
            input
                .missions
                .values()
                .map(|m| (m.subject.clone(), m.revision.clone())),
        )
        .collect();
    let mut changes = BTreeMap::<String, String>::new();
    let mut effects = BTreeMap::<String, String>::new();
    let mut heads = BTreeMap::new();
    let mut adoptions = BTreeMap::new();
    let mut retired = old
        .as_ref()
        .map(|v| v.receipt.retired.clone())
        .unwrap_or_default();
    for (s, revision) in &live {
        let own = owner(transaction, s, None)?;
        if own.as_deref().is_some_and(|own| own != set) {
            blockers.push(format!("{s} belongs to another set"));
        }
        let tokens = if s.starts_with("mission/") {
            mission_definition_token_tx(transaction, s.trim_start_matches("mission/"))
                .map_err(internal)?
        } else {
            intent_leaves_tx(transaction, s).map_err(internal)?
        };
        if own.is_none() && !tokens.is_empty() {
            if !options.adopt.contains(s) {
                blockers.push(format!("explicit adoption required: {s}"));
            } else {
                adoptions.insert(s.clone(), tokens.clone());
            }
        }
        let existing = old.as_ref().and_then(|v| v.receipt.members.get(s));
        changes.insert(
            s.clone(),
            if existing.is_some_and(|m| m.revision == *revision) {
                "unchanged"
            } else if existing.is_some() {
                "changed"
            } else {
                "added"
            }
            .into(),
        );
        if existing.is_none_or(|m| m.revision != *revision) {
            let effect = if s.starts_with("mission/") {
                "publish definition; retain active runs".to_owned()
            } else if s.starts_with("schedule/") {
                "update future occurrences; retain created runs".to_owned()
            } else if let Some(prior) = existing {
                let old_subject = claim(transaction, &prior.claim, None)?
                    .and_then(|c| serde_json::from_value::<DesiredSubject>(c.body).ok());
                let old_member = old_subject.as_ref().and_then(|d| d.member.clone());
                match (
                    input.subjects.get(s).and_then(|d| d.member.as_ref()),
                    old_member,
                ) {
                    (Some(new), Some(old)) => {
                        let changed = new.launch_changes(&old);
                        if options.rollout.is_some() && !changed.is_empty() {
                            if old_subject.as_ref().and_then(|d| crate::accounts::harness_binding(&d.desired))
                                != input.subjects.get(s).and_then(|d| crate::accounts::harness_binding(&d.desired)) {
                                blockers.push(format!("{s}: when-idle requires the same native account binding"));
                            }
                            if let Err(refusal) = crate::native_resume::rollout_support(new) {
                                blockers.push(format!("{s}: {}", refusal.reason));
                            }
                            if new.host != old.host || new.driver != old.driver {
                                blockers.push(format!(
                                    "{s}: when-idle requires the same host and harness family"
                                ));
                            }
                            if !matches!(
                                new.driver.as_deref(),
                                Some("claude" | "codex" | "opencode" | "pi" | "omp")
                            ) {
                                blockers.push(format!(
                                    "{s}: when-idle requires a supported native harness"
                                ));
                            }
                        }
                        if changed.is_empty() {
                            "update declaration; launch unchanged".into()
                        } else {
                            format!(
                                "{}: {}",
                                if options.rollout.is_some() {
                                    "drain and resume native session"
                                } else {
                                    "restart runtime"
                                },
                                changed.join(", ")
                            )
                        }
                    }
                    _ => "start runtime".into(),
                }
            } else {
                "start runtime".into()
            };
            effects.insert(s.clone(), effect);
        }
        heads.insert(s.clone(), tokens);
        retired.remove(s);
    }
    for s in &options.adopt {
        if !live.contains_key(s) {
            blockers.push(format!("adoption is not in submitted membership: {s}"));
        }
    }
    let mut materialize = BTreeSet::new();
    if let Some(old) = &old {
        let known = known_members(transaction, &set, None)?;
        for (s, m) in &known {
            if old.receipt.retired.contains_key(s) {
                continue;
            }
            if !old.receipt.members.contains_key(s) && m.kind != "mission" {
                materialize.insert(s.clone());
            }
            if live.contains_key(s) {
                continue;
            }
            if options.rollout.is_some() && m.kind == "agent" {
                let old_member = claim(transaction, &m.claim, None)?
                    .and_then(|c| serde_json::from_value::<DesiredSubject>(c.body).ok())
                    .and_then(|d| d.member);
                if old_member
                    .as_ref()
                    .is_none_or(|member| crate::native_resume::rollout_support(member).is_err())
                {
                    blockers.push(format!(
                        "{s}: when-idle retirement requires a supported native harness"
                    ));
                }
            }
            changes.insert(s.clone(), "retiring".into());
            if m.kind == "mission" {
                let c = claim(transaction, &m.claim, None)?.ok_or_else(|| {
                    St3Error::new("owned-set-pending", "missing mission reference")
                })?;
                let spec: MissionSpec = serde_json::from_value(c.body).map_err(internal)?;
                let spec = crate::mission::retired_mission(spec)?;
                intent.missions.insert(spec.id.clone(), spec);
                heads.insert(
                    s.clone(),
                    mission_definition_token_tx(transaction, s.trim_start_matches("mission/"))
                        .map_err(internal)?,
                );
                effects.insert(s.clone(), "prevent new starts; retain active runs".into());
            } else {
                intent.subjects.insert(s.clone(), stop_declaration(s)?);
                heads.insert(
                    s.clone(),
                    intent_leaves_tx(transaction, s).map_err(internal)?,
                );
                effects.insert(
                    s.clone(),
                    if m.kind == "schedule" {
                        "stop future occurrences; retain runs"
                    } else {
                        if options.rollout.is_some() {
                            "drain and retire runtime; retain conversation and history"
                        } else {
                            "stop runtime; retain conversation and history"
                        }
                    }
                    .into(),
                );
            }
            retired.insert(s.clone(), m.clone());
        }
    }
    let retiring = changes
        .values()
        .filter(|v| v.as_str() == "retiring")
        .count();
    let old_count = old.as_ref().map_or(0, |v| v.receipt.members.len());
    let mass = retiring > 0 && (retiring >= 10 || retiring.saturating_mul(2) >= old_count);
    let digest = canonical_hash(&(
        &set,
        &previous,
        &options.source,
        &bundle_digest,
        &heads,
        &changes,
        &options.adopt,
        options.allow_empty,
        &options.rollout,
    ))
    .map_err(internal)?;
    let preview = Preview {
        rollout: options.rollout.clone(),
        set,
        previous,
        source: options.source.clone(),
        changes,
        effects,
        rollouts: BTreeMap::new(),
        expected_subjects: heads,
        digest,
        mass_retirement: mass,
        empty: live.is_empty(),
        blockers,
        noop,
    };
    Ok(Plan {
        intent,
        preview,
        retired,
        adoptions,
        bundle_digest,
        materialize,
    })
}

pub(super) fn validate_apply(plan: &Plan, options: &Options) -> Result<(), St3Error> {
    if !plan.preview.blockers.is_empty() {
        return Err(
            St3Error::new("owned-set-refused", plan.preview.blockers.join("; "))
                .with_detail("preview", json!(plan.preview)),
        );
    }
    if plan.preview.noop {
        return Ok(());
    }
    if options.expected_subjects != plan.preview.expected_subjects {
        return Err(St3Error::new(
            "stale-subject",
            "owned-set member heads changed; preview again",
        ));
    }
    if plan.preview.empty && !options.allow_empty {
        return Err(St3Error::new(
            "empty-owned-set",
            "intentional empty membership needs --allow-empty",
        )
        .with_detail("preview", json!(plan.preview)));
    }
    if plan.preview.mass_retirement
        && options.confirm_retire.as_deref() != Some(&plan.preview.digest)
    {
        return Err(St3Error::new(
            "mass-retirement-refused",
            "retirement requires confirmation of this exact preview",
        )
        .with_detail("preview", json!(plan.preview)));
    }
    Ok(())
}

pub(super) fn commit_tx(
    transaction: &Transaction<'_>,
    origin: &str,
    plan: &Plan,
    actor: Option<&str>,
    batch: &str,
) -> Result<ClaimRecord, St3Error> {
    let mut members = BTreeMap::new();
    let mut retired = plan.retired.clone();
    for (s, change) in &plan.preview.changes {
        let member = if s.starts_with("mission/") {
            transaction
                .query_row(
                    "SELECT claim_id,revision FROM mission_definitions WHERE mission_id=?1",
                    [s.trim_start_matches("mission/")],
                    |row| {
                        Ok(Member {
                            kind: "mission".into(),
                            claim: row.get(0)?,
                            revision: row.get(1)?,
                        })
                    },
                )
                .map_err(internal)?
        } else {
            let row = current_desired_row_tx(transaction, s)
                .map_err(internal)?
                .ok_or_else(|| St3Error::new("missing-set-member", s.clone()))?;
            Member {
                kind: if s.starts_with("agent/") {
                    "agent"
                } else {
                    "schedule"
                }
                .into(),
                claim: row.claim_id,
                revision: row.revision,
            }
        };
        if change == "retiring" {
            retired.insert(s.clone(), member);
        } else {
            members.insert(s.clone(), member);
        }
    }
    let revision = Revision {
        rollout: plan.preview.rollout.clone(),
        previous: plan.preview.previous.clone(),
        source: plan.preview.source.clone(),
        bundle_digest: plan.bundle_digest.clone(),
        members,
        retired,
        adoptions: plan.adoptions.clone(),
    };
    let hash = canonical_hash(&revision).map_err(internal)?;
    let predecessors = rows(transaction, None)?
        .into_iter()
        .filter(|v| Some(reference(v)) == revision.previous)
        .map(|v| v.claim)
        .collect::<Vec<_>>();
    append_claim_tx(
        transaction,
        origin,
        &plan.preview.set,
        "owned-set.revised",
        actor,
        &json!({"fields":{"revision":hash,"body":revision}}),
        &predecessors,
        Some(batch),
    )
    .map_err(internal)
}

/// Rebuild the existing desired and mission projections through exact set member references.
pub(super) fn project_tx(transaction: &Transaction<'_>) -> Result<(), St3Error> {
    for view in selected(transaction, None)? {
        if !view.blockers.is_empty() {
            continue;
        } // Existing runtimes hold; callers fence actions below.
        for (s, (m, retired, implicit)) in effective_members(transaction, &view, None)? {
            if m.kind == "mission" {
                let Some(c) = claim(transaction, &m.claim, None)? else {
                    continue;
                };
                transaction.execute("INSERT INTO mission_definitions(mission_id,revision,state,claim_id) VALUES (?1,?2,?3,?4)
                    ON CONFLICT(mission_id) DO UPDATE SET revision=excluded.revision,state=excluded.state,claim_id=excluded.claim_id",
                    params![s.trim_start_matches("mission/"),m.revision,if retired{"retired"}else{c.body["state"].as_str().unwrap_or("draft")},m.claim]).map_err(internal)?;
            } else {
                let desired: DesiredSubject = if implicit {
                    stop_declaration(&s)?
                } else {
                    serde_json::from_value(claim(transaction, &m.claim, None)?.unwrap().body)
                        .map_err(internal)?
                };
                transaction.execute("INSERT INTO desired(subject,kind,revision,claim_id,body,member,owner_run,owner_generation,owner_step) VALUES (?1,?2,?3,?4,?5,?6,NULL,NULL,NULL)
                    ON CONFLICT(subject) DO UPDATE SET kind=excluded.kind,revision=excluded.revision,claim_id=excluded.claim_id,body=excluded.body,member=excluded.member,owner_run=NULL,owner_generation=NULL,owner_step=NULL",
                    params![s,desired.kind,m.revision,m.claim,canonical_json_text(&desired.desired).map_err(internal)?,desired.member.as_ref().map(canonical_serialized_json_text).transpose().map_err(internal)?]).map_err(internal)?;
            }
        }
    }
    Ok(())
}

pub(super) fn desired_at(connection: &Connection, s: &str, at: u64) -> Result<Option<DesiredRow>> {
    let Some(set) = owner(connection, s, Some(at)).map_err(anyhow::Error::new)? else {
        return Ok(None);
    };
    let view = selected(connection, Some(at))
        .map_err(anyhow::Error::new)?
        .into_iter()
        .find(|v| v.id == set)
        .unwrap();
    if !view.blockers.is_empty() {
        return Ok(None);
    };
    let members = effective_members(connection, &view, Some(at)).map_err(anyhow::Error::new)?;
    let Some((m, _, implicit)) = members.get(s) else {
        return Ok(None);
    };
    if m.kind == "mission" {
        return Ok(None);
    }
    let d: DesiredSubject = if *implicit {
        stop_declaration(s).map_err(anyhow::Error::new)?
    } else {
        serde_json::from_value(
            claim(connection, &m.claim, Some(at))
                .map_err(anyhow::Error::new)?
                .unwrap()
                .body,
        )?
    };
    Ok(Some(DesiredRow {
        kind: d.kind,
        revision: m.revision.clone(),
        claim_id: m.claim.clone(),
        body: canonical_json_text(&d.desired)?,
        member: d
            .member
            .as_ref()
            .map(canonical_serialized_json_text)
            .transpose()?,
        owner_run: None,
        owner_generation: None,
    }))
}

pub(super) fn validate_receipt(subject_name: &str, body: &Value) -> Result<(), St3Error> {
    let revision: Revision = serde_json::from_value(body["fields"]["body"].clone())
        .map_err(|e| St3Error::new("invalid-owned-set", e.to_string()))?;
    if subject(subject_name)? != subject_name
        || body["fields"]["revision"].as_str()
            != Some(&canonical_hash(&revision).map_err(internal)?)
    {
        return Err(St3Error::new(
            "invalid-owned-set",
            "set revision has invalid identity/hash",
        ));
    }
    if revision
        .members
        .keys()
        .any(|s| revision.retired.contains_key(s))
    {
        return Err(St3Error::new(
            "invalid-owned-set",
            "a member cannot be live and retired",
        ));
    }
    for (s, m) in revision.members.iter().chain(&revision.retired) {
        if !matches!(m.kind.as_str(), "agent" | "mission" | "schedule")
            || !s.starts_with(&format!("{}/", m.kind))
            || m.claim.is_empty()
            || m.revision.is_empty()
        {
            return Err(St3Error::new(
                "invalid-owned-set",
                "invalid member reference",
            ));
        }
    }
    if revision.source.repository.split('/').count() != 2
        || !revision.source.r#ref.starts_with("refs/heads/")
        || !matches!(revision.source.sha.len(), 40 | 64)
        || !revision.source.sha.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(St3Error::new("invalid-owned-set", "invalid source"));
    }
    Ok(())
}

impl Store {
    pub fn owned_sets(&self) -> Result<Vec<View>, St3Error> {
        selected(&self.readers.get(), None)
    }
    /// Known replica names from graph membership, without sealing batches during a read.
    pub fn owned_set_replica_names(&self) -> Result<Vec<String>, St3Error> {
        let membership = fleet_membership_tx(&self.readers.get()).map_err(internal)?;
        Ok(membership
            .incarnations()
            .filter(|m| m.end.is_none())
            .map(|m| m.name.clone())
            .collect())
    }
    pub fn owned_set_history(&self, set: &str) -> Result<Vec<View>, St3Error> {
        let subject = subject(set)?;
        Ok(rows(&self.readers.get(), None)?
            .into_iter()
            .filter(|v| v.id == subject)
            .collect())
    }
    pub fn owned_set_effective_members(
        &self,
        view: &View,
    ) -> Result<Vec<(String, Member, bool)>, St3Error> {
        Ok(effective_members(&self.readers.get(), view, None)?
            .into_iter()
            .map(|(s, (m, retired, _))| (s, m, retired))
            .collect())
    }
    pub fn owned_set_preview(
        &self,
        intent: &NormalizedIntent,
        options: &Options,
    ) -> Result<Preview, St3Error> {
        self.connection
            .batched(|tx| plan_tx(tx, intent, options).map(|p| p.preview))
            .map_err(|e| St3Error::new("internal", e))?
    }
    pub fn owned_member_guard(&self, s: &str) -> Result<(), St3Error> {
        guard_member(&self.readers.get(), s)
    }
    /// Fence an external effect prepared from a declaration against the selected set.
    pub fn owned_desired_guard(&self, desired: &DesiredSubject) -> Result<(), St3Error> {
        let connection = self.readers.get();
        guard_member(&connection, &desired.subject)?;
        if let Some(set) = owner(&connection, &desired.subject, None)? {
            let view = selected(&connection, None)?
                .into_iter()
                .find(|v| v.id == set)
                .ok_or_else(|| St3Error::new("owned-set-pending", "missing owning set"))?;
            let members = effective_members(&connection, &view, None)?;
            let member = members.get(&desired.subject);
            if member.is_none_or(|(m, _, _)| m.revision != desired_revision(desired)) {
                return Err(St3Error::new(
                    "stale-set-member",
                    "declaration changed since this effect was prepared",
                ));
            }
        }
        Ok(())
    }
    pub fn apply_owned_set(
        &self,
        intent: &NormalizedIntent,
        options: &Options,
        key: &str,
        actor: &str,
    ) -> Result<ApplyResponse, St3Error> {
        let key = format!("owned-set:{}:{key}", options.set);
        self.apply_as_impl(
            intent,
            &options.expected_subjects,
            &key,
            Some(actor),
            Some(options),
        )
    }
}
