//! Constant-size canonical operation reduction over stored and checkpointed claim metadata.
#[derive(Default)]
pub struct OperationMetadata {
    minimum: Option<String>,
    canonical: Option<String>,
    fallback: Option<String>,
    conflict: bool,
}

impl OperationMetadata {
    /// Add one `(request digest, claim ID)`, distinguishing stored claims from tombstones.
    pub fn include(&mut self, digest: String, claim: String, stored: bool) {
        match self.minimum.as_ref().map(|minimum| digest.cmp(minimum)) {
            None | Some(std::cmp::Ordering::Less) => {
                self.conflict |= self.minimum.is_some();
                self.minimum = Some(digest);
                self.canonical = stored.then(|| claim.clone());
            }
            Some(std::cmp::Ordering::Greater) => self.conflict = true,
            Some(std::cmp::Ordering::Equal) => {
                if stored && self.canonical.as_ref().is_none_or(|canonical| &claim < canonical) {
                    self.canonical = Some(claim.clone());
                }
            }
        }
        if stored && self.fallback.as_ref().is_none_or(|fallback| &claim < fallback) {
            self.fallback = Some(claim);
        }
    }

    /// Return no row when only tombstones remain; otherwise choose a stored canonical claim.
    pub fn row(self) -> Option<(String, String, String)> {
        let fallback = self.fallback?;
        let canonical = self.canonical.unwrap_or(fallback);
        let state = if self.conflict { "conflict" } else { "active" };
        Some((self.minimum.expect("a stored claim supplies a digest"), canonical, state.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::OperationMetadata;
    use proptest::prelude::*;

    fn old_row(records: &[(String, String, bool)]) -> Option<(String, String, String)> {
        let stored: Vec<_> = records.iter().filter(|(_, _, stored)| *stored).collect();
        if stored.is_empty() { return None; }
        let mut claims: Vec<_> = records.iter().map(|(digest, claim, _)| (digest, claim)).collect();
        claims.sort();
        let digest = claims[0].0;
        let canonical = stored.iter().filter(|(candidate, _, _)| candidate == digest)
            .map(|(_, claim, _)| claim).min()
            .or_else(|| stored.iter().map(|(_, claim, _)| claim).min()).unwrap();
        let state = if claims.iter().all(|(candidate, _)| *candidate == digest) { "active" } else { "conflict" };
        Some((digest.clone(), canonical.clone(), state.into()))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn streaming_metadata_matches_the_old_canonical_reduction(
            records in prop::collection::vec(("[a-c]{0,4}", "[a-c]{0,4}", any::<bool>()), 0..40)
        ) {
            let mut forward = OperationMetadata::default();
            let mut reverse = OperationMetadata::default();
            for (digest, claim, stored) in &records {
                forward.include(digest.clone(), claim.clone(), *stored);
            }
            for (digest, claim, stored) in records.iter().rev() {
                reverse.include(digest.clone(), claim.clone(), *stored);
            }
            let expected = old_row(&records);
            prop_assert_eq!(forward.row(), expected.clone());
            prop_assert_eq!(reverse.row(), expected);
        }
    }
}
