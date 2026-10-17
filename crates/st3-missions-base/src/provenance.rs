//! Mission revision provenance is a sidecar, deliberately outside MissionSpec and its hash.
use std::collections::BTreeMap;

use kdl::{KdlDocument, KdlNode};
use rusqlite::{Connection, OptionalExtension as _, params};
use serde_json::Value;
pub use st3_schema::provenance::{Provenance, Source};

use crate::model::{NormalizedIntent, St3Error};

fn invalid(message: impl Into<String>) -> St3Error {
    St3Error::new("invalid-mission-provenance", message)
}

fn bare(node: &KdlNode, arguments: usize, children: bool) -> Result<(), St3Error> {
    if node.ty().is_some()
        || node.entries().len() != arguments
        || node.entries().iter().any(|entry| {
            entry.name().is_some() || entry.ty().is_some() || entry.value().as_string().is_none()
        })
        || node.children().is_some() != children
    {
        return Err(invalid(format!(
            "provenance `{}` needs {arguments} plain string arguments and {} block",
            node.name().value(),
            if children { "a" } else { "no" }
        )));
    }
    Ok(())
}

fn string(node: &KdlNode) -> Result<String, St3Error> {
    bare(node, 1, false)?;
    Ok(node.entries()[0].value().as_string().unwrap().to_owned())
}

fn singleton(body: &KdlDocument, name: &str) -> Result<String, St3Error> {
    let nodes = body
        .nodes()
        .iter()
        .filter(|node| node.name().value() == name)
        .collect::<Vec<_>>();
    if nodes.len() != 1 {
        return Err(invalid(format!("provenance needs exactly one `{name}`")));
    }
    string(nodes[0])
}

pub fn parse_block(node: &KdlNode) -> Result<Provenance, St3Error> {
    bare(node, 0, true)?;
    let body = node.children().unwrap();
    if body.nodes().iter().any(|node| {
        !matches!(
            node.name().value(),
            "reason" | "source" | "decision" | "evidence"
        )
    }) {
        return Err(invalid(
            "provenance only accepts reason, source, decision and evidence",
        ));
    }
    let sources = body
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "source")
        .collect::<Vec<_>>();
    if sources.len() != 1 {
        return Err(invalid("provenance needs exactly one `source`"));
    }
    bare(sources[0], 0, true)?;
    let source = sources[0].children().unwrap();
    if source.nodes().iter().any(|node| {
        !matches!(
            node.name().value(),
            "repository" | "commit" | "path" | "renderer"
        )
    }) {
        return Err(invalid(
            "provenance source only accepts repository, commit, path and renderer",
        ));
    }
    let references = |name: &str| {
        body.nodes()
            .iter()
            .filter(|node| node.name().value() == name)
            .map(string)
            .collect::<Result<Vec<_>, _>>()
    };
    let provenance = Provenance {
        reason: singleton(body, "reason")?,
        source: Source {
            repository: singleton(source, "repository")?,
            commit: singleton(source, "commit")?,
            path: singleton(source, "path")?,
            renderer: singleton(source, "renderer")?,
        },
        decision: references("decision")?,
        evidence: references("evidence")?,
    };
    provenance
        .validate()
        .map_err(|error| invalid(error.message))?;
    Ok(provenance)
}

pub fn parse(document: &KdlDocument) -> Result<BTreeMap<String, Provenance>, St3Error> {
    let mut result = BTreeMap::new();
    for mission in document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "mission")
    {
        let Some(body) = mission.children() else {
            continue;
        };
        let nodes = body
            .nodes()
            .iter()
            .filter(|node| node.name().value() == "provenance")
            .collect::<Vec<_>>();
        if nodes.len() > 1 {
            return Err(invalid("a mission can have only one provenance block"));
        }
        if let Some(node) = nodes.first() {
            let id = mission
                .entries()
                .first()
                .and_then(|entry| entry.value().as_string())
                .ok_or_else(|| invalid("provenance needs a named mission"))?;
            result.insert(id.to_owned(), parse_block(node)?);
        }
    }
    Ok(result)
}

pub fn subject(mission: &str, revision: &str) -> String {
    format!(
        "mission/{}@{revision}",
        mission.strip_prefix("mission/").unwrap_or(mission)
    )
}

pub fn read(
    connection: &Connection,
    mission: &str,
    revision: &str,
) -> anyhow::Result<Option<Provenance>> {
    let body = connection.query_row(
        &smallclaims::store::canonical_sql("SELECT body FROM claims WHERE subject=?1 AND kind='mission.provenance' ORDER BY CANONICAL_ASC(claims) LIMIT 1"),
        [subject(mission, revision)], |row| row.get::<_, String>(0),
    ).optional()?;
    body.map(|body| {
        let body: Value = serde_json::from_str(&body)?;
        Ok(serde_json::from_value(
            body["fields"]["provenance"].clone(),
        )?)
    })
    .transpose()
}

pub fn validate_publication(
    connection: &Connection,
    intent: &NormalizedIntent,
) -> Result<(), St3Error> {
    for (id, provenance) in &intent.mission_provenance {
        provenance
            .validate()
            .map_err(|error| invalid(error.message))?;
        let mission = intent
            .missions
            .get(id)
            .ok_or_else(|| invalid("provenance has no mission"))?;
        let exists = connection.query_row("SELECT EXISTS(SELECT 1 FROM mission_revisions WHERE mission_id=?1 AND revision=?2)", params![id, mission.revision], |row| row.get::<_, bool>(0)).map_err(internal)?;
        let previous = read(connection, id, &mission.revision).map_err(internal)?;
        if previous
            .as_ref()
            .is_some_and(|previous| previous != provenance)
            || (exists && previous.is_none())
        {
            return Err(St3Error::new(
                "immutable-mission-provenance",
                "provenance is fixed at first publication; change the mission definition to publish a new revision",
            ));
        }
    }
    Ok(())
}

pub fn internal(error: impl std::fmt::Display) -> St3Error {
    St3Error::new("internal", error.to_string())
}

pub fn render(provenance: &Provenance) -> String {
    let mut lines = vec![
        "PROVENANCE".to_owned(),
        format!("  Reason: {}", provenance.reason),
        format!("  Repository: {}", provenance.source.repository),
        format!("  Commit: {}", provenance.source.commit),
        format!("  Path: {}", provenance.source.path),
        format!("  Renderer: {}", provenance.source.renderer),
    ];
    lines.extend(
        provenance
            .decision
            .iter()
            .map(|reference| format!("  Decision: {reference}")),
    );
    lines.extend(
        provenance
            .evidence
            .iter()
            .map(|reference| format!("  Evidence: {reference}")),
    );
    format!("{}\n", lines.join("\n"))
}

#[cfg(any(test, feature = "test-support"))]
pub mod tests {
    pub fn block(reason: &str) -> String {
        format!(
            r#"provenance {{
            reason {reason:?}
            source {{
                repository "https://example.invalid/orchard/missions"
                commit "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                path "missions/orchard.kdl"
                renderer "orchard-renderer-v1"
            }}
            decision "decision/opaque-approved-scope"
            decision "https://example.invalid/decisions/42"
            evidence "doc/orchard/proof@opaque-reference"
        }}"#
        )
    }

    pub fn source(goal: &str, reason: Option<&str>) -> String {
        format!(
            "version 2\nmission \"orchard\" state=\"ready\" {{ {}\n goal {goal:?}; step \"build\" {{ goal {goal:?}; }} }}",
            reason.map(block).unwrap_or_default()
        )
    }
}
