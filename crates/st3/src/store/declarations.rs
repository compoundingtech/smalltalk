//! Canonical publication values shared by dry-run diffs and definition reads.
use super::*;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeclarationDiff {
    /// Null means no previously published declaration; adoption includes the unmanaged value.
    pub before: Option<Value>,
    pub after: Value,
    /// JSON pointers; arrays are compared as a whole, objects by field.
    pub fields: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublicationDefinition {
    pub kind: String,
    pub subject: String,
    pub declaration: Value,
    pub revision: String,
    pub token: String,
}

fn desired_value(desired: &DesiredSubject) -> Result<Value, St3Error> {
    serde_json::to_value(desired).map_err(internal)
}

fn proposed(intent: &NormalizedIntent, subject: &str) -> Result<Option<Value>, St3Error> {
    if let Some(desired) = intent.subjects.get(subject) {
        return desired_value(desired).map(Some);
    }
    subject
        .strip_prefix("mission/")
        .and_then(|id| intent.missions.get(id))
        .map(|mission| serde_json::to_value(mission).map_err(internal))
        .transpose()
}

/// Read the selected graph definition, including an owned set's exact member references.
fn selected(
    connection: &Connection,
    subject: &str,
    at: Option<u64>,
) -> Result<Option<PublicationDefinition>, St3Error> {
    let selected = if let Some(id) = subject.strip_prefix("mission/") {
        // Mission selection is projected transactionally with the owned-set winner.
        let row: Option<(String, String, String)> = connection
            .query_row(
                "SELECT r.body, d.revision, d.claim_id FROM mission_definitions d
             JOIN mission_revisions r ON r.mission_id=d.mission_id AND r.revision=d.revision
             WHERE d.mission_id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(internal)?;
        row.map(|(body, revision, token)| {
            let mission: MissionSpec = serde_json::from_str(&body).map_err(internal)?;
            Ok((
                serde_json::to_value(mission).map_err(internal)?,
                revision,
                token,
            ))
        })
        .transpose()?
    } else {
        desired_row_at(connection, subject, at)
            .map_err(internal)?
            .map(|row| {
                // A derived owned-set retirement need not have its own intent.desired claim.
                let desired = DesiredSubject {
                    subject: subject.into(),
                    kind: row.kind,
                    desired: serde_json::from_str(&row.body).map_err(internal)?,
                    member: row
                        .member
                        .map(|body| serde_json::from_str(&body).map_err(internal))
                        .transpose()?,
                    owner_run: row.owner_run,
                    owner_generation: row.owner_generation,
                    owner_step: owned_sets::claim(connection, &row.claim_id, at)?
                        .and_then(|claim| claim.body["owner_step"].as_str().map(str::to_owned)),
                };
                Ok((desired_value(&desired)?, row.revision, row.claim_id))
            })
            .transpose()?
    };
    Ok(
        selected.map(|(declaration, revision, token)| PublicationDefinition {
            kind: "publication-definition".into(),
            subject: subject.into(),
            declaration,
            revision,
            token,
        }),
    )
}

fn changed_fields(
    before: Option<&Value>,
    after: Option<&Value>,
    path: &str,
    fields: &mut BTreeSet<String>,
) {
    if before == after {
        return;
    }
    if let (Some(Value::Object(before)), Some(Value::Object(after))) = (before, after) {
        for key in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
            let escaped = key.replace('~', "~0").replace('/', "~1");
            changed_fields(
                before.get(key),
                after.get(key),
                &format!("{path}/{escaped}"),
                fields,
            );
        }
    } else {
        fields.insert(path.into());
    }
}

pub(super) fn diffs<'a>(
    connection: &Connection,
    intent: &NormalizedIntent,
    subjects: impl Iterator<Item = &'a str>,
    at: Option<u64>,
) -> Result<BTreeMap<String, DeclarationDiff>, St3Error> {
    let mut diffs = BTreeMap::new();
    for subject in subjects {
        let Some(after) = proposed(intent, subject)? else {
            continue;
        };
        let before = selected(connection, subject, at)?.map(|definition| definition.declaration);
        let mut fields = BTreeSet::new();
        changed_fields(before.as_ref(), Some(&after), "", &mut fields);
        diffs.insert(
            subject.into(),
            DeclarationDiff {
                before,
                after,
                fields,
            },
        );
    }
    Ok(diffs)
}

impl Store {
    pub(crate) fn publication_definition(
        &self,
        subject: &str,
        at: u64,
    ) -> Result<Option<PublicationDefinition>, St3Error> {
        selected(&self.readers.get(), subject, Some(at))
    }
}
