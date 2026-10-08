//! Source-owned canonical STOP arrangement. Source producers certify complete OLD/NEW
//! declarations and canonical batch/record corrections at the same native cut.
//! This indexed component supplies no capture coverage or production read authority.
use super::*;
use smallclaims::ivm::install::Namespace;

const MAX_ID: usize = 4096;
const MAX_FACT: usize = 16 * 1024;
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS local_attention_stop_heads(
 namespace TEXT NOT NULL,claim TEXT NOT NULL,requester TEXT NOT NULL,
 canonical_key BLOB NOT NULL CHECK(typeof(canonical_key)='blob'),fact TEXT NOT NULL,
 PRIMARY KEY(namespace,claim)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS attention_stop_head_by_requester
 ON local_attention_stop_heads(namespace,requester,canonical_key DESC,claim DESC);";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub requester: String,
    pub key: canonical::ClaimKey,
}
impl Head {
    fn encoded(&self, claim: &str) -> Result<(Vec<u8>, String)> {
        anyhow::ensure!(
            !self.requester.is_empty() && self.requester.len() <= MAX_ID && self.key.5 == claim,
            "STOP requester or canonical claim identity"
        );
        anyhow::ensure!(
            self.key.1.len() <= MAX_ID && self.key.3.len() <= MAX_ID && self.key.5.len() <= MAX_ID,
            "STOP canonical component bound"
        );
        let key = canonical::sortable_key(&self.key);
        let fact = serde_json::to_string(&self.key)?;
        anyhow::ensure!(
            key.len() <= MAX_FACT && fact.len() <= MAX_FACT,
            "STOP canonical fact bound"
        );
        Ok((key, fact))
    }
}

pub(crate) fn create_schema(c: &Connection) -> Result<()> {
    c.execute_batch(SCHEMA)?;
    Ok(())
}

pub(crate) struct Changes {
    /// OLD and NEW requester keys must both be invalidated after a declaration rekey.
    pub affected_requesters: BTreeSet<String>,
    pub writes: usize,
}

/// One declaration birth, correction, or removal. `None` includes a declaration that
/// ceases to be a STOP. All lookups are keyed; caller accounts for reads/writes/bytes.
pub(crate) fn replace(
    tx: &Transaction<'_>,
    ns: &Namespace,
    claim: &str,
    next: Option<&Head>,
) -> Result<Changes> {
    anyhow::ensure!(
        !claim.is_empty() && claim.len() <= MAX_ID,
        "STOP claim identity bound"
    );
    let next = next
        .map(|head| {
            head.encoded(claim)
                .map(|(key, fact)| (head.requester.clone(), key, fact))
        })
        .transpose()?;
    // Read lengths before materializing potentially corrupted local facts.
    let lengths: Option<(usize,usize,usize)> = tx.query_row(
        "SELECT length(CAST(requester AS BLOB)),length(canonical_key),length(CAST(fact AS BLOB)) FROM local_attention_stop_heads WHERE namespace=?1 AND claim=?2",
        params![ns.as_str(),claim], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    ).optional()?;
    if let Some((requester, key, fact)) = lengths {
        anyhow::ensure!(
            requester <= MAX_ID && key <= MAX_FACT && fact <= MAX_FACT,
            "retained STOP fact bound"
        );
    }
    let old: Option<(String,Vec<u8>,String)> = tx.query_row(
        "SELECT requester,canonical_key,fact FROM local_attention_stop_heads WHERE namespace=?1 AND claim=?2",
        params![ns.as_str(),claim], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    ).optional()?;
    if old == next {
        return Ok(Changes {
            affected_requesters: BTreeSet::new(),
            writes: 0,
        });
    }
    let affected_requesters = old
        .iter()
        .chain(next.iter())
        .map(|row| row.0.clone())
        .collect();
    let writes = if let Some((requester, key, fact)) = next {
        tx.execute("INSERT INTO local_attention_stop_heads VALUES(?1,?2,?3,?4,?5) ON CONFLICT(namespace,claim) DO UPDATE SET requester=excluded.requester,canonical_key=excluded.canonical_key,fact=excluded.fact",
            params![ns.as_str(),claim,requester,key,fact])?
    } else {
        tx.execute(
            "DELETE FROM local_attention_stop_heads WHERE namespace=?1 AND claim=?2",
            params![ns.as_str(), claim],
        )?
    };
    Ok(Changes {
        affected_requesters,
        writes,
    })
}

/// Indexed maximum, including future STOP declarations exactly as the native oracle does.
/// Caller supplies one short snapshot so both bounded statements observe the same row.
pub(crate) fn maximum(c: &Connection, ns: &Namespace, requester: &str) -> Result<Option<Head>> {
    anyhow::ensure!(
        !requester.is_empty() && requester.len() <= MAX_ID,
        "STOP requester bound"
    );
    let row: Option<(usize,usize,usize)> = c.query_row(
        "SELECT length(CAST(claim AS BLOB)),length(canonical_key),length(CAST(fact AS BLOB)) FROM local_attention_stop_heads WHERE namespace=?1 AND requester=?2 ORDER BY canonical_key DESC,claim DESC LIMIT 1",
        params![ns.as_str(),requester], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    ).optional()?;
    let Some((claim_bytes, key_bytes, fact_bytes)) = row else {
        return Ok(None);
    };
    anyhow::ensure!(
        claim_bytes <= MAX_ID && key_bytes <= MAX_FACT && fact_bytes <= MAX_FACT,
        "STOP maximum fact bound"
    );
    let (claim, key, fact): (String, Vec<u8>, String) = c.query_row(
        "SELECT claim,canonical_key,fact FROM local_attention_stop_heads WHERE namespace=?1 AND requester=?2 ORDER BY canonical_key DESC,claim DESC LIMIT 1",
        params![ns.as_str(), requester],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let head = Head {
        requester: requester.into(),
        key: serde_json::from_str(&fact)?,
    };
    anyhow::ensure!(
        head.encoded(&claim)? == (key, fact),
        "STOP maximum fact changed"
    );
    Ok(Some(head))
}
