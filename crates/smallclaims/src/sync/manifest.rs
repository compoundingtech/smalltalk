//! Bound assembly before certificate verification; never return a partial manifest.
use super::*;
use crate::store::{CheckpointManifest, CheckpointManifestCursor, CheckpointManifestPage};
use std::io::{self, Write};

struct Count {
    bytes: usize,
    limit: usize,
}
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "checkpoint manifest exceeds its byte limit",
            ));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) struct Assembly {
    pub(super) manifest: CheckpointManifest,
    bytes: usize,
    limit: usize,
}
impl Assembly {
    pub(super) fn new(checkpoint: &str, cut_unix_ms: u128, limit: usize) -> Result<Self> {
        let manifest = CheckpointManifest {
            checkpoint: checkpoint.to_owned(),
            cut_unix_ms,
            ..Default::default()
        };
        let mut count = Count { bytes: 0, limit };
        serde_json::to_writer(&mut count, &manifest)
            .context("bound the checkpoint manifest header")?;
        Ok(Self {
            manifest,
            bytes: count.bytes,
            limit,
        })
    }

    pub(super) fn append(
        &mut self,
        after: Option<&CheckpointManifestCursor>,
        page: CheckpointManifestPage,
    ) -> Result<Option<CheckpointManifestCursor>> {
        anyhow::ensure!(
            page.checkpoint == self.manifest.checkpoint
                && page.cut_unix_ms == self.manifest.cut_unix_ms,
            "checkpoint manifest identity changed between pages"
        );
        if let Some(next) = &page.next {
            anyhow::ensure!(
                !page.envelopes.is_empty() || !page.claims.is_empty(),
                "checkpoint cursor advances without any rows"
            );
            if let Some(after) = after {
                anyhow::ensure!(
                    advances(after, next),
                    "checkpoint manifest cursor did not advance"
                );
            }
        }
        // The empty arrays and header are counted once. Add each exact row encoding and the
        // commas it introduces, without building a second manifest-sized JSON buffer.
        let mut count = Count {
            bytes: self.bytes,
            limit: self.limit,
        };
        count_rows(&mut count, self.manifest.envelopes.len(), &page.envelopes)?;
        count_rows(&mut count, self.manifest.claims.len(), &page.claims)?;
        let next = self.manifest.append(page).map_err(anyhow::Error::msg)?;
        self.bytes = count.bytes;
        Ok(next)
    }
}
fn count_rows<T: Serialize>(count: &mut Count, existing: usize, rows: &[T]) -> Result<()> {
    for (index, row) in rows.iter().enumerate() {
        if existing != 0 || index != 0 {
            count.write_all(b",")?;
        }
        serde_json::to_writer(&mut *count, row).context("bound checkpoint manifest rows")?;
    }
    Ok(())
}
fn advances(after: &CheckpointManifestCursor, next: &CheckpointManifestCursor) -> bool {
    use CheckpointManifestCursor::*;
    match (after, next) {
        (Envelope(a), Envelope(b)) => b > a,
        (Envelope(_), Claim(_)) => true,
        (Claim(a), Claim(b)) => b > a,
        (Claim(_), Envelope(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ClaimTombstone, EnvelopeKey, EnvelopeTombstone};
    fn key(n: u64) -> EnvelopeKey {
        EnvelopeKey {
            writer: "fixture".into(),
            sequence: n,
            envelope_hash: "a".repeat(64),
        }
    }
    fn page(n: u64, next: Option<CheckpointManifestCursor>) -> CheckpointManifestPage {
        let key = key(n);
        CheckpointManifestPage {
            checkpoint: "checkpoint/fixture".into(),
            cut_unix_ms: 42,
            envelopes: vec![EnvelopeTombstone {
                writer: key.writer,
                sequence: n,
                envelope_hash: key.envelope_hash,
                accepted_at_unix_ms: 1,
            }],
            claims: vec![],
            next,
        }
    }
    #[test]
    fn exact_aggregate_budget_counts_headers_rows_and_commas_before_append() {
        let mut full = CheckpointManifest {
            checkpoint: "checkpoint/fixture".into(),
            cut_unix_ms: 42,
            ..Default::default()
        };
        let mut final_page = page(2, None);
        final_page.claims = (1..=2)
            .map(|n| ClaimTombstone {
                id: format!("claim/{n}"),
                writer: "fixture".into(),
                sequence: n,
                envelope_hash: "a".repeat(64),
                subject: "note/fixture".into(),
                kind: "note.created".into(),
                actor: None,
                predecessors: vec!["claim/escaped-\"text".into()],
                operation_id: None,
                request_digest: None,
                accepted_at_unix_ms: 1,
            })
            .collect();
        full.append(page(1, None)).unwrap();
        full.append(final_page.clone()).unwrap();
        let size = serde_json::to_vec(&full).unwrap().len();
        let mut exact = Assembly::new("checkpoint/fixture", 42, size).unwrap();
        let next = exact
            .append(
                None,
                page(1, Some(CheckpointManifestCursor::Envelope(key(1)))),
            )
            .unwrap();
        assert!(
            exact
                .append(next.as_ref(), final_page.clone())
                .unwrap()
                .is_none()
        );
        assert_eq!(exact.bytes, size);
        assert_eq!(exact.manifest, full);
        let mut bounded = Assembly::new("checkpoint/fixture", 42, size - 1).unwrap();
        let next = bounded
            .append(
                None,
                page(1, Some(CheckpointManifestCursor::Envelope(key(1)))),
            )
            .unwrap();
        assert!(bounded.append(next.as_ref(), final_page).is_err());
        assert_eq!(bounded.manifest.envelopes.len(), 1);
        assert!(bounded.manifest.claims.is_empty());
    }
    #[test]
    fn repeated_backward_empty_and_changed_identity_pages_cannot_accumulate() {
        let after = CheckpointManifestCursor::Envelope(key(2));
        for next in [
            CheckpointManifestCursor::Envelope(key(2)),
            CheckpointManifestCursor::Envelope(key(1)),
        ] {
            let mut assembly = Assembly::new("checkpoint/fixture", 42, 4096).unwrap();
            assert!(assembly.append(Some(&after), page(3, Some(next))).is_err());
            assert!(assembly.manifest.envelopes.is_empty());
        }
        let mut assembly = Assembly::new("checkpoint/fixture", 42, 4096).unwrap();
        let mut empty = page(3, Some(CheckpointManifestCursor::Claim("later".into())));
        empty.envelopes.clear();
        assert!(assembly.append(Some(&after), empty).is_err());
        let mut changed = page(3, None);
        changed.cut_unix_ms = 43;
        assert!(assembly.append(None, changed).is_err());
        assert!(assembly.manifest.envelopes.is_empty());
        assert!(advances(
            &after,
            &CheckpointManifestCursor::Claim(String::new())
        ));
        assert!(!advances(
            &CheckpointManifestCursor::Claim("last".into()),
            &after
        ));
    }
}
