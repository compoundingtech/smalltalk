//! Candidate pages and their dependency accumulator commit together. A page never authorizes
//! a partial result. Relevant authority changes dirty the cursor in queue_linked_tx.
use super::*;

pub(super) fn rank(verdict: &Verdict) -> u8 {
    match verdict {
        Verdict::Verified => 0,
        Verdict::Held(_) => 1,
        Verdict::Invalid(_) => 2,
        Verdict::Unsigned => 3,
    }
}
pub(super) fn candidate_count(connection: &Connection, claim_id: &str) -> Result<u64> {
    Ok(connection
        .prepare_cached("SELECT candidates FROM claim_signature_counts WHERE claim_id=?1")?
        .query_row([claim_id], |row| row.get(0))
        .optional()?
        .unwrap_or(0))
}
pub(super) fn complete(connection: &Connection, claim_id: &str) -> Result<bool> {
    Ok(connection.prepare_cached("SELECT EXISTS(SELECT 1 FROM claim_signature_judging p JOIN claim_signature_counts c USING(claim_id) WHERE p.claim_id=?1 AND p.complete=1 AND p.dirty=0 AND p.blocked=0 AND p.candidate_count=c.candidates)")?
        .query_row([claim_id],|row|row.get(0))?)
}
pub(super) fn validate_cached(connection: &Connection, row: &ClaimRow) -> Result<()> {
    let (content,signature,verdict,reason)=connection.prepare_cached("SELECT content_digest,best_signature,best_verdict,best_reason FROM claim_signature_judging WHERE claim_id=?1")?.query_row([&row.id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,Option<String>>(2)?,row.get::<_,Option<String>>(3)?)))?;
    anyhow::ensure!(
        content == content_digest(&row.subject, &row.kind, row.actor.as_deref(), &row.body),
        "signature-state-corrupt: claim content differs from completed judgment"
    );
    let cached = cached_verdict(connection, &row.id)?.ok_or_else(|| {
        anyhow::anyhow!("signature-state-corrupt: completed judgment has no cached verdict")
    })?;
    anyhow::ensure!(
        verdict.as_deref() == Some(cached.name()) && reason.as_deref() == cached.reason(),
        "signature-state-corrupt: cached verdict differs from completed judgment"
    );
    let facade = stored_signature(connection, &row.id)?.ok_or_else(|| {
        anyhow::anyhow!("signature-state-corrupt: completed judgment has no signature")
    })?;
    anyhow::ensure!(
        signature.as_deref() == Some(serde_json::to_string(&facade)?.as_str()),
        "signature-state-corrupt: signature facade differs from completed judgment"
    );
    Ok(())
}

pub(super) fn unavailable(connection: &Connection, claim_id: &str) -> Result<Option<String>> {
    let count = candidate_count(connection, claim_id)?;
    let pending: bool=connection.prepare_cached("SELECT EXISTS(SELECT 1 FROM claim_signature_judging WHERE claim_id=?1 AND (complete=0 OR dirty=1 OR blocked=1))")?.query_row([claim_id],|row|row.get(0))?;
    if pending || (count > CANDIDATE_PAGE as u64 && !complete(connection, claim_id)?) {
        return Ok(Some(format!(
            "signature-judgment-incomplete: {claim_id} has {count} candidates"
        )));
    }
    Ok(None)
}

pub(super) type Judgment = (Verdict, Option<ClaimSignature>);
/// At most budget candidates are judged in this writer loan. Unprocessed work stays queued.
pub(super) fn advance(
    connection: &Connection,
    roots: &BTreeSet<(String, String)>,
    claim_id: &str,
    memo: &RefCell<BTreeMap<String, Verdict>>,
    budget: usize,
) -> Result<(Option<Judgment>, usize)> {
    let count = candidate_count(connection, claim_id)?;
    let Some(row) = claim_row(connection, claim_id)? else {
        return Ok((None, 0));
    };
    let roots_digest = crate::hash::canonical_hash(roots)?;
    let previous=connection.prepare_cached("SELECT candidate_count,roots_digest,last_signature,best_signature,best_verdict,best_reason,complete,dirty,blocked FROM claim_signature_judging WHERE claim_id=?1")?
        .query_row([claim_id],|row|Ok((row.get::<_,u64>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,Option<String>>(4)?,row.get::<_,Option<String>>(5)?,row.get::<_,bool>(6)?,row.get::<_,bool>(7)?,row.get::<_,bool>(8)?))).optional()?;
    let (last, mut best, mut blocked) = match previous {
        Some((n, root, last, signature, verdict, reason, false, false, blocked))
            if n == count && root == roots_digest =>
        {
            if let Some(name) = &verdict
                && !matches!(name.as_str(), "verified" | "held" | "invalid" | "unsigned")
            {
                anyhow::bail!(
                    "signature-state-corrupt: unknown candidate accumulator verdict {name}"
                );
            }
            (
                last,
                signature
                    .zip(verdict)
                    .map(|(signature, verdict)| (verdict_from(&verdict, reason), signature)),
                blocked,
            )
        }
        _ => {
            // Retained candidates can become relevant again. Keep their dependency
            // superset incrementally; clearing/rebuilding it here would make a single
            // loan proportional to the whole candidate set. Forget/recheck own cleanup.
            connection.prepare_cached("INSERT INTO claim_signature_judging(claim_id,candidate_count,roots_digest,content_digest,last_signature,complete,dirty) VALUES (?1,?2,?3,?4,'',0,0) ON CONFLICT(claim_id) DO UPDATE SET candidate_count=excluded.candidate_count,roots_digest=excluded.roots_digest,content_digest=excluded.content_digest,last_signature='',best_signature=NULL,best_verdict=NULL,best_reason=NULL,complete=0,dirty=0,notified=0,blocked=0")?.execute(params![claim_id,count,roots_digest,content_digest(&row.subject,&row.kind,row.actor.as_deref(),&row.body)])?;
            (String::new(), None, false)
        }
    };
    let texts=connection.prepare_cached("SELECT signature FROM claim_signature_candidates WHERE claim_id=?1 AND signature>?2 ORDER BY signature LIMIT ?3")?
        .query_map(params![claim_id,last,budget as i64],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let candidates = texts
        .iter()
        .map(|text| Ok(serde_json::from_str::<ClaimSignature>(text)?))
        .collect::<Result<Vec<_>>>()?;
    let (verdict, links, signature) =
        judge_candidates(connection, roots, &row, candidates, 0, memo)?;
    blocked |= matches!(&verdict,Verdict::Held(reason) if reason.starts_with("signature-judgment-incomplete:"));
    for link in &links {
        connection
            .prepare_cached(
                "INSERT OR IGNORE INTO claim_verdict_links(link,claim_id) VALUES (?1,?2)",
            )?
            .execute(params![link, claim_id])?;
    }
    if let Some(signature) = signature {
        let text = serde_json::to_string(&signature)?;
        if best
            .as_ref()
            .is_none_or(|(old, old_text)| (rank(&verdict), &text) < (rank(old), old_text))
        {
            best = Some((verdict, text));
        }
    }
    let through = texts.last().cloned().unwrap_or(last);
    let finished = texts.len() < budget;
    let (best_name, best_reason, best_signature) =
        best.as_ref()
            .map_or((None, None, None), |(verdict, signature)| {
                (
                    Some(verdict.name()),
                    verdict.reason(),
                    Some(signature.as_str()),
                )
            });
    connection.prepare_cached("UPDATE claim_signature_judging SET last_signature=?1,best_signature=?2,best_verdict=?3,best_reason=?4,complete=?5,blocked=?6 WHERE claim_id=?7")?.execute(params![through,best_signature,best_name,best_reason,finished,blocked,claim_id])?;
    if !finished {
        connection
            .prepare_cached("INSERT OR IGNORE INTO claim_verdict_queue(claim_id) VALUES (?1)")?
            .execute([claim_id])?;
        return Ok((None, texts.len()));
    }
    if count != 0 && best.is_none() {
        anyhow::bail!(
            "signature-state-corrupt: nonempty candidate judgment has no result for {claim_id}"
        );
    }
    let result = if let Some((verdict, text)) = best {
        (
            if blocked {
                Verdict::Held(
                    "signature-judgment-incomplete: an authority dependency is pending".into(),
                )
            } else {
                verdict
            },
            Some(serde_json::from_str(&text)?),
        )
    } else {
        (Verdict::Unsigned, None)
    };
    Ok((Some(result), texts.len()))
}
