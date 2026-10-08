//! Keyed inputs to the native human-attention run predicate, before public row filtering.
//!
//! The source owner supplies complete per-source replacements in a certified namespace.
//! This component does not qualify a source or switch production reads. Missions consume
//! its indexed run dependency, independently of public attention and login availability.
//! All storage binds to main. The owner must validate the physical table and index
//! definitions at its cut; requiring the run index only prevents a missing-index fallback.
use super::*;
use serde::{Deserialize, Serialize};
use smallclaims::ivm::install::Namespace;

const PAGE: usize = 128;
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS main.local_attention_human_membership (
 namespace TEXT NOT NULL, family TEXT NOT NULL, source TEXT NOT NULL,
 run TEXT NOT NULL, eligible BLOB NOT NULL,
 PRIMARY KEY(namespace,family,source,run)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS main.attention_human_membership_run
 ON local_attention_human_membership(namespace,run,eligible,family,source);
CREATE INDEX IF NOT EXISTS main.attention_human_membership_clock
 ON local_attention_human_membership(namespace,eligible,run,family,source);
"#;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Family {
    Person,
    Review,
    Planning,
    Revision,
}
impl Family {
    fn name(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Review => "review",
            Self::Planning => "planning",
            Self::Revision => "revision",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Membership {
    pub run: String,
    #[serde(with = "eligibility")]
    pub eligible: u128,
}

// Installer mutations travel through serde_json::Value, which cannot represent an
// arbitrary u128 numeric token. Keep the full clock domain as decimal source data.
mod eligibility {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(
        value: &u128,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<u128, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Empty replacements are meaningful: closing or removing a source retracts its runs.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Input {
    pub family: Family,
    pub source: String,
    pub runs: Vec<Membership>,
}
impl Input {
    /// `items` must be the native source output before current_attention's common filter.
    /// Only person asks carry an accepted-time predicate in human_attention_runs; public
    /// requested_at/waiting_since timestamps must not delay reviews or ordinary person steps.
    pub(crate) fn from_items(
        family: Family,
        source: String,
        items: &[AttentionItemView],
        person_ask_accepted_at: Option<u128>,
    ) -> Result<Self> {
        anyhow::ensure!(
            items.len() <= PAGE,
            "human source contribution page exceeds 128"
        );
        anyhow::ensure!(
            family == Family::Person || person_ask_accepted_at.is_none(),
            "ask eligibility on another human source family"
        );
        let eligible = person_ask_accepted_at.unwrap_or(0);
        let mut runs = BTreeSet::new();
        for item in items {
            let supported = match family {
                Family::Person => item.kind == "person-step",
                Family::Review => item.kind == "human-gate",
                Family::Planning => item.kind == "launch-approval",
                Family::Revision => item.kind == "revision-approval",
            };
            anyhow::ensure!(
                supported,
                "attention item does not belong to its human source family"
            );
            if let Some(run) = &item.mission_run {
                runs.insert(run.clone());
            }
        }
        let input = Self {
            family,
            source,
            runs: runs
                .into_iter()
                .map(|run| Membership { run, eligible })
                .collect(),
        };
        input.validate()?;
        Ok(input)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.source.is_empty() && self.source.len() <= 4096 && self.runs.len() <= PAGE,
            "human source identity or contribution bound"
        );
        let mut seen = BTreeSet::new();
        for row in &self.runs {
            anyhow::ensure!(
                row.run.starts_with("mission-run/")
                    && row.run.len() <= 4096
                    && seen.insert(&row.run),
                "human run identity or duplicate"
            );
            anyhow::ensure!(
                self.family == Family::Person || row.eligible == 0,
                "non-person human membership has no time filter"
            );
        }
        Ok(())
    }
}

pub(crate) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

#[derive(Debug, Default)]
pub(crate) struct Changes {
    /// Includes OLD and NEW run IDs, even when their membership starts in the future.
    pub affected_runs: BTreeSet<String>,
    pub writes: usize,
}

/// Replace at most one source's bounded contribution; unchanged rows incur no writes.
/// Each returned run is an exact dependency key for a mission source's native capture.
/// The caller spends the returned writes against its shared publication page budget.
pub(crate) fn replace(
    tx: &Transaction<'_>,
    namespace: &Namespace,
    input: &Input,
) -> Result<Changes> {
    input.validate()?;
    let old: BTreeMap<String, Vec<u8>> = tx.prepare_cached(
        "SELECT run,eligible FROM main.local_attention_human_membership WHERE namespace=?1 AND family=?2 AND source=?3 ORDER BY run LIMIT 129"
    )?.query_map(params![namespace.as_str(),input.family.name(),input.source], |row| Ok((row.get(0)?,row.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    anyhow::ensure!(
        old.len() <= PAGE,
        "previous human source contribution exceeds 128"
    );
    let next: BTreeMap<String, Vec<u8>> = input
        .runs
        .iter()
        .map(|row| (row.run.clone(), row.eligible.to_be_bytes().to_vec()))
        .collect();
    let affected_runs: BTreeSet<String> = old
        .keys()
        .chain(next.keys())
        .filter(|run| old.get(*run) != next.get(*run))
        .cloned()
        .collect();
    anyhow::ensure!(
        affected_runs.len() <= PAGE,
        "human source replacement exceeds shared page bound"
    );
    for run in &affected_runs {
        if let Some(eligible) = next.get(run) {
            tx.execute("INSERT INTO main.local_attention_human_membership VALUES(?1,?2,?3,?4,?5) ON CONFLICT(namespace,family,source,run) DO UPDATE SET eligible=excluded.eligible",params![namespace.as_str(),input.family.name(),input.source,run,eligible])?;
        } else {
            tx.execute("DELETE FROM main.local_attention_human_membership WHERE namespace=?1 AND family=?2 AND source=?3 AND run=?4",params![namespace.as_str(),input.family.name(),input.source,run])?;
        }
    }
    Ok(Changes {
        writes: affected_runs.len(),
        affected_runs,
    })
}

pub(crate) struct Run {
    pub run: String,
    pub waiting: bool,
    /// A mission watches this indexed run key; it does not inherit login/public-row readiness.
    pub dependencies: BTreeSet<String>,
    pub next_deadline: Option<u128>,
}

/// Call in the source owner's certified read snapshot, with that snapshot's captured clock.
/// EXISTS and the next eligible key use the same run index, irrespective of contributor count.
pub(crate) fn selected(
    connection: &Connection,
    namespace: &Namespace,
    runs: &[String],
    captured_at: u128,
) -> Result<Vec<Run>> {
    anyhow::ensure!(runs.len() <= PAGE, "human selected run page exceeds 128");
    let at = captured_at.to_be_bytes().to_vec();
    let mut output = Vec::with_capacity(runs.len());
    for run in runs {
        anyhow::ensure!(
            run.starts_with("mission-run/") && run.len() <= 4096,
            "human selected run identity"
        );
        let waiting = connection.query_row("SELECT EXISTS(SELECT 1 FROM main.local_attention_human_membership INDEXED BY attention_human_membership_run WHERE namespace=?1 AND run=?2 AND eligible<=?3)",params![namespace.as_str(),run,at],|row|row.get(0))?;
        let next: Option<Vec<u8>> = connection.query_row("SELECT eligible FROM main.local_attention_human_membership INDEXED BY attention_human_membership_run WHERE namespace=?1 AND run=?2 AND eligible>?3 ORDER BY eligible LIMIT 1",params![namespace.as_str(),run,at],|row|row.get(0)).optional()?;
        let next_deadline = next
            .map(|bytes| {
                Ok::<_, anyhow::Error>(u128::from_be_bytes(
                    bytes
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("human eligibility encoding"))?,
                ))
            })
            .transpose()?;
        output.push(Run {
            run: run.clone(),
            waiting,
            dependencies: BTreeSet::from([run.clone()]),
            next_deadline,
        });
    }
    Ok(output)
}

/// Bounded namespace cleanup, spending one shared row budget.
pub(crate) fn reclaim(tx: &Transaction<'_>, namespace: &Namespace, limit: usize) -> Result<bool> {
    anyhow::ensure!(
        (1..=PAGE).contains(&limit),
        "human namespace cleanup page exceeds 128"
    );
    tx.execute("DELETE FROM main.local_attention_human_membership WHERE (namespace,family,source,run) IN (SELECT namespace,family,source,run FROM main.local_attention_human_membership WHERE namespace=?1 ORDER BY family,source,run LIMIT ?2)",params![namespace.as_str(),limit])?;
    Ok(!tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.local_attention_human_membership WHERE namespace=?1)",
        [namespace.as_str()],
        |row| row.get::<_, bool>(0),
    )?)
}

#[cfg(test)]
mod tests;
