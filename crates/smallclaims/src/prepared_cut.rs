//! Pure checks for a future prepared replication cut.
//!
//! These checks consume owned facts captured by a qualified source. They do not read a Store,
//! authenticate a peer, construct a proof, or change the current replication protocol. In
//! particular, a matching token is insufficient unless the source has proved every input at
//! that cut and the legacy digest was made from SQLite's exact `json_array` bytes.

use sha2::{Digest as _, Sha256};

/// The native database and registered source lifetime that produced a cut. A file copy,
/// restore, source reinstallation, or schema replacement must change or refuse this identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceLifetime {
    pub database_id: String,
    pub owner_id: String,
    pub source_name: String,
    pub fingerprint: String,
    pub epoch: String,
    pub file_incarnation: String,
    pub copy_generation: u64,
    pub restore_generation: u64,
    pub schema_identity: String,
}

/// The complete inventory boundary, including the prefix retained by an incremental builder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryCut {
    pub prefix_count: u64,
    pub prefix_digest: String,
    pub envelope_count: u64,
    pub tail_rowid: Option<i64>,
    pub digest: String,
}

/// Every native marker required by the prepared producer and its consumer. These values must
/// come from one consistent cut; this type does not create that cut or maintain the markers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CutToken {
    pub lifetime: SourceLifetime,
    pub source_revision: u64,
    pub store_index: u64,
    pub graph_generation: i64,
    pub projection_generation: i64,
    pub inventory_generation: i64,
    pub pending_generation: i64,
    pub seeded_batch_rowid: i64,
    pub admission_generation: i64,
    pub signature_generation: i64,
    pub checkpoint_generation: i64,
    pub inventory: InventoryCut,
    /// The old wire digest computed from complete SQLite `json_array` rows at this cut.
    pub legacy_graph_digest: String,
}

/// An independent assertion from the source that it has finished all proof inputs. These
/// fields must never be filled from a partial page, a live Atomic, or a notice alone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedCut {
    pub token: CutToken,
    pub source_available: bool,
    pub native_complete: bool,
    pub inventory_complete: bool,
    pub sealed: bool,
    pub pending_known: bool,
    pub admission_complete: bool,
    pub projections_complete: bool,
    pub legacy: Option<LegacyProof>,
}

/// The source's legacy digest certificate. Only the old SQLite JSON-array byte stream is an
/// eligible format. The producer still has to prove bounded capture and full table coverage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyProof {
    pub format: LegacyFormat,
    pub digest: String,
    pub complete: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyFormat {
    SqliteJsonArrayV1,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CutRefusal {
    Cancelled,
    Changed,
    Malformed,
    Incomplete,
    LegacyUnavailable,
}

fn digest_is_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn token_well_formed(token: &CutToken) -> bool {
    let lifetime = &token.lifetime;
    let inventory = &token.inventory;
    !lifetime.database_id.is_empty()
        && !lifetime.owner_id.is_empty()
        && !lifetime.source_name.is_empty()
        && !lifetime.fingerprint.is_empty()
        && !lifetime.epoch.is_empty()
        && !lifetime.file_incarnation.is_empty()
        && !lifetime.schema_identity.is_empty()
        && inventory.prefix_count <= inventory.envelope_count
        && (inventory.envelope_count == 0) == inventory.tail_rowid.is_none()
        && digest_is_hex(&inventory.prefix_digest)
        && digest_is_hex(&inventory.digest)
        && digest_is_hex(&token.legacy_graph_digest)
}

/// Refuse a stale or incomplete Arc using only captured values. `requested` is the current
/// authoritative marker tuple, captured under the same independent coverage check as the
/// caller's request. A future cache getter must repeat that coverage check after this call.
pub fn validate_prepared_cut(
    requested: &CutToken,
    prepared: &PreparedCut,
    cancelled: bool,
) -> Result<(), CutRefusal> {
    if cancelled {
        return Err(CutRefusal::Cancelled);
    }
    if !token_well_formed(requested) || !token_well_formed(&prepared.token) {
        return Err(CutRefusal::Malformed);
    }
    if requested != &prepared.token {
        return Err(CutRefusal::Changed);
    }
    if !prepared.source_available
        || !prepared.native_complete
        || !prepared.inventory_complete
        || !prepared.sealed
        || !prepared.pending_known
        || !prepared.admission_complete
        || !prepared.projections_complete
    {
        return Err(CutRefusal::Incomplete);
    }
    let Some(legacy) = &prepared.legacy else {
        return Err(CutRefusal::LegacyUnavailable);
    };
    if legacy.format != LegacyFormat::SqliteJsonArrayV1
        || !legacy.complete
        || !digest_is_hex(&legacy.digest)
        || legacy.digest != requested.legacy_graph_digest
    {
        return Err(CutRefusal::LegacyUnavailable);
    }
    Ok(())
}

/// Bytes and order must be those returned by SQLite `json_array` for each of the old digest
/// tables. This reproduces `store::digest_queries` without a connection or a Rust JSON rewrite.
/// It does not bound the cost of obtaining arbitrary large legacy SQLite values.
pub struct LegacyDigestBuilder(Sha256);

impl Default for LegacyDigestBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl LegacyDigestBuilder {
    pub fn new() -> Self {
        let mut digest = Sha256::new();
        digest.update(b"st3-logical-digest-v1\0");
        Self(digest)
    }

    pub fn table<'a>(&mut self, name: &str, sqlite_json_rows: impl IntoIterator<Item = &'a [u8]>) {
        self.0.update(name.as_bytes());
        self.0.update([0]);
        for row in sqlite_json_rows {
            self.0.update((row.len() as u64).to_be_bytes());
            self.0.update(row);
        }
    }

    pub fn finish(self) -> String {
        hex::encode(self.0.finalize())
    }
}

/// A future handshake must bind capability to this *current* authenticated exchange and peer
/// incarnation. Current FleetAuth signatures alone do not provide this freshness certificate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionToken {
    pub local_owner_id: String,
    pub peer_id: String,
    pub peer_incarnation: String,
    pub session_id: String,
    pub request_digest: String,
}

impl SessionToken {
    fn well_formed(&self) -> bool {
        !self.local_owner_id.is_empty()
            && !self.peer_id.is_empty()
            && !self.peer_incarnation.is_empty()
            && !self.session_id.is_empty()
            && digest_is_hex(&self.request_digest)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityEvidence {
    pub session: SessionToken,
    pub authenticated: bool,
    pub fresh: bool,
    pub prepared_v1: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProofDigests {
    pub inventory: String,
    pub projection: String,
    pub legacy: String,
}

impl ProofDigests {
    fn well_formed(&self) -> bool {
        digest_is_hex(&self.inventory)
            && digest_is_hex(&self.projection)
            && digest_is_hex(&self.legacy)
    }
}

/// The signed remote assertion and the actual body must agree. `qualified_producer` is an
/// externally established source certificate, not something this helper can infer from a flag
/// on the wire. Absence or falsehood of any field keeps projection comparison pending.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteCompleteness {
    pub session: SessionToken,
    pub authenticated: bool,
    pub qualified_producer: bool,
    pub complete: Option<bool>,
    pub inventory_complete: bool,
    pub projections_complete: bool,
    pub legacy_complete: bool,
    pub asserted: ProofDigests,
    pub transmitted: ProofDigests,
}

#[derive(Clone, Copy)]
pub enum PeerLane<'a> {
    /// Kept for compatibility, without a two-sided prepared-cut guarantee.
    Legacy,
    Prepared {
        capability: Option<&'a CapabilityEvidence>,
        completeness: Option<&'a RemoteCompleteness>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComparisonDecision {
    RetryLocalCut(CutRefusal),
    LegacyUnqualified,
    PendingPeerProof,
    Eligible,
}

impl ComparisonDecision {
    /// The same gate applies to projection equality, projection first-sync observation,
    /// compare_graphs, and heal_due. Log-only authority sync has a separate contract.
    pub fn permits_projection_actions(self) -> bool {
        self == Self::Eligible
    }
}

/// Decide only whether a future *prepared* exchange may compare projections. A pending result
/// does not discard committed receipt counts or prevent existing inventory catch-up; the
/// receive adapter must retain those semantics independently. No current exchange calls this.
pub fn decide_comparison(
    requested: &CutToken,
    prepared: &PreparedCut,
    cancelled: bool,
    current_session: &SessionToken,
    lane: PeerLane<'_>,
) -> ComparisonDecision {
    if let Err(reason) = validate_prepared_cut(requested, prepared, cancelled) {
        return ComparisonDecision::RetryLocalCut(reason);
    }
    if matches!(lane, PeerLane::Legacy) {
        return ComparisonDecision::LegacyUnqualified;
    }
    if !current_session.well_formed()
        || current_session.local_owner_id != requested.lifetime.owner_id
    {
        return ComparisonDecision::PendingPeerProof;
    }
    let (Some(capability), Some(completeness)) = (match lane {
        PeerLane::Prepared {
            capability,
            completeness,
        } => (capability, completeness),
        PeerLane::Legacy => unreachable!(),
    }) else {
        return ComparisonDecision::PendingPeerProof;
    };
    if capability.session != *current_session
        || !capability.authenticated
        || !capability.fresh
        || !capability.prepared_v1
        || completeness.session != *current_session
        || !completeness.authenticated
        || !completeness.qualified_producer
        || completeness.complete != Some(true)
        || !completeness.inventory_complete
        || !completeness.projections_complete
        || !completeness.legacy_complete
        || !completeness.asserted.well_formed()
        || completeness.asserted != completeness.transmitted
        || completeness.transmitted.inventory != requested.inventory.digest
    {
        return ComparisonDecision::PendingPeerProof;
    }
    ComparisonDecision::Eligible
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::digest_queries;
    use rusqlite::Connection;

    fn digest(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    fn cut() -> CutToken {
        CutToken {
            lifetime: SourceLifetime {
                database_id: "database-a".into(),
                owner_id: "local-a".into(),
                source_name: "replication".into(),
                fingerprint: "fingerprint-a".into(),
                epoch: "epoch-a".into(),
                file_incarnation: "file-a".into(),
                copy_generation: 1,
                restore_generation: 2,
                schema_identity: "schema-a".into(),
            },
            source_revision: 3,
            store_index: 4,
            graph_generation: 5,
            projection_generation: 6,
            inventory_generation: 7,
            pending_generation: 8,
            seeded_batch_rowid: 9,
            admission_generation: 10,
            signature_generation: 11,
            checkpoint_generation: 12,
            inventory: InventoryCut {
                prefix_count: 1,
                prefix_digest: digest('a'),
                envelope_count: 2,
                tail_rowid: Some(13),
                digest: digest('b'),
            },
            legacy_graph_digest: digest('c'),
        }
    }

    fn prepared() -> PreparedCut {
        let token = cut();
        PreparedCut {
            legacy: Some(LegacyProof {
                format: LegacyFormat::SqliteJsonArrayV1,
                digest: token.legacy_graph_digest.clone(),
                complete: true,
            }),
            token,
            source_available: true,
            native_complete: true,
            inventory_complete: true,
            sealed: true,
            pending_known: true,
            admission_complete: true,
            projections_complete: true,
        }
    }

    fn session() -> SessionToken {
        SessionToken {
            local_owner_id: "local-a".into(),
            peer_id: "peer-a".into(),
            peer_incarnation: "boot-a".into(),
            session_id: "challenge-a".into(),
            request_digest: digest('d'),
        }
    }

    fn peer() -> (CapabilityEvidence, RemoteCompleteness) {
        let session = session();
        let body = ProofDigests {
            inventory: cut().inventory.digest,
            projection: digest('e'),
            legacy: cut().legacy_graph_digest,
        };
        (
            CapabilityEvidence {
                session: session.clone(),
                authenticated: true,
                fresh: true,
                prepared_v1: true,
            },
            RemoteCompleteness {
                session,
                authenticated: true,
                qualified_producer: true,
                complete: Some(true),
                inventory_complete: true,
                projections_complete: true,
                legacy_complete: true,
                asserted: body.clone(),
                transmitted: body,
            },
        )
    }

    fn decision(
        requested: &CutToken,
        prepared: &PreparedCut,
        capability: &CapabilityEvidence,
        completeness: &RemoteCompleteness,
    ) -> ComparisonDecision {
        decide_comparison(
            requested,
            prepared,
            false,
            &session(),
            PeerLane::Prepared {
                capability: Some(capability),
                completeness: Some(completeness),
            },
        )
    }

    #[test]
    fn every_native_marker_and_inventory_boundary_refuses_a_changed_cut() {
        let baseline = prepared();
        let changes: &[fn(&mut CutToken)] = &[
            |c| c.lifetime.database_id.push('x'),
            |c| c.lifetime.owner_id.push('x'),
            |c| c.lifetime.source_name.push('x'),
            |c| c.lifetime.fingerprint.push('x'),
            |c| c.lifetime.epoch.push('x'),
            |c| c.lifetime.file_incarnation.push('x'),
            |c| c.lifetime.copy_generation += 1,
            |c| c.lifetime.restore_generation += 1,
            |c| c.lifetime.schema_identity.push('x'),
            |c| c.source_revision += 1,
            |c| c.store_index += 1,
            |c| c.graph_generation += 1,
            |c| c.projection_generation += 1,
            |c| c.inventory_generation += 1,
            |c| c.pending_generation += 1,
            |c| c.seeded_batch_rowid += 1,
            |c| c.admission_generation += 1,
            |c| c.signature_generation += 1,
            |c| c.checkpoint_generation += 1,
            |c| c.inventory.prefix_count = 0,
            |c| c.inventory.prefix_digest = digest('d'),
            |c| c.inventory.envelope_count += 1,
            |c| c.inventory.tail_rowid = Some(14),
            |c| c.inventory.digest = digest('d'),
            |c| c.legacy_graph_digest = digest('d'),
        ];
        for (index, change) in changes.iter().enumerate() {
            let mut requested = cut();
            change(&mut requested);
            assert_eq!(
                validate_prepared_cut(&requested, &baseline, false),
                Err(CutRefusal::Changed),
                "changed field {index}"
            );
        }
        assert_eq!(
            validate_prepared_cut(&cut(), &baseline, true),
            Err(CutRefusal::Cancelled)
        );
    }

    #[test]
    fn partial_local_proof_and_nonlegacy_bytes_never_validate() {
        let mut candidate = prepared();
        let changes: &[fn(&mut PreparedCut)] = &[
            |p: &mut PreparedCut| p.source_available = false,
            |p: &mut PreparedCut| p.native_complete = false,
            |p: &mut PreparedCut| p.inventory_complete = false,
            |p: &mut PreparedCut| p.sealed = false,
            |p: &mut PreparedCut| p.pending_known = false,
            |p: &mut PreparedCut| p.admission_complete = false,
            |p: &mut PreparedCut| p.projections_complete = false,
        ];
        for change in changes {
            change(&mut candidate);
            assert_eq!(
                validate_prepared_cut(&cut(), &candidate, false),
                Err(CutRefusal::Incomplete)
            );
            candidate = prepared();
        }
        candidate.legacy = None;
        assert_eq!(
            validate_prepared_cut(&cut(), &candidate, false),
            Err(CutRefusal::LegacyUnavailable)
        );
        candidate = prepared();
        candidate.legacy.as_mut().unwrap().format = LegacyFormat::Other;
        assert_eq!(
            validate_prepared_cut(&cut(), &candidate, false),
            Err(CutRefusal::LegacyUnavailable)
        );
        candidate = prepared();
        candidate.legacy.as_mut().unwrap().digest = digest('d');
        assert_eq!(
            validate_prepared_cut(&cut(), &candidate, false),
            Err(CutRefusal::LegacyUnavailable)
        );
        candidate = prepared();
        candidate.token.inventory.envelope_count = 0;
        assert_eq!(
            validate_prepared_cut(&cut(), &candidate, false),
            Err(CutRefusal::Malformed)
        );
    }

    #[test]
    fn legacy_builder_hashes_exact_sqlite_json_array_bytes() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE old_a(id TEXT PRIMARY KEY, value TEXT, number REAL);
                 INSERT INTO old_a VALUES ('a', 'quote\" slash\\ newline
', 1.5);
                 INSERT INTO old_a VALUES ('b', 'π', NULL);
                 CREATE TABLE old_b(id TEXT PRIMARY KEY, flag INTEGER);
                 INSERT INTO old_b VALUES ('x', 1);",
            )
            .unwrap();
        let queries = [
            (
                "old_a",
                "SELECT json_array(id,value,number) FROM old_a ORDER BY id",
            ),
            ("old_b", "SELECT json_array(id,flag) FROM old_b ORDER BY id"),
        ];
        let expected = digest_queries(&connection, &queries).unwrap();
        let mut builder = LegacyDigestBuilder::new();
        for (name, query) in queries {
            let mut statement = connection.prepare(query).unwrap();
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|row| row.unwrap().into_bytes())
                .collect::<Vec<_>>();
            builder.table(name, rows.iter().map(Vec::as_slice));
        }
        assert_eq!(builder.finish(), expected);
        let mut altered = LegacyDigestBuilder::new();
        altered.table("old_a", [b"[\"a\",\"other\",1.5]".as_slice()]);
        assert_ne!(altered.finish(), expected);
    }

    #[test]
    fn prepared_peer_requires_current_authenticated_session_and_complete_body() {
        let (capability, completeness) = peer();
        assert_eq!(
            decision(&cut(), &prepared(), &capability, &completeness),
            ComparisonDecision::Eligible
        );
        let mut changed_capability = capability.clone();
        changed_capability.session.peer_incarnation = "restarted".into();
        assert_eq!(
            decision(&cut(), &prepared(), &changed_capability, &completeness),
            ComparisonDecision::PendingPeerProof
        );
        changed_capability = capability.clone();
        changed_capability.session.session_id = "stale".into();
        assert_eq!(
            decision(&cut(), &prepared(), &changed_capability, &completeness),
            ComparisonDecision::PendingPeerProof
        );
        changed_capability = capability.clone();
        changed_capability.session.local_owner_id = "other-core".into();
        assert_eq!(
            decision(&cut(), &prepared(), &changed_capability, &completeness),
            ComparisonDecision::PendingPeerProof
        );
        changed_capability = capability.clone();
        changed_capability.prepared_v1 = false;
        assert_eq!(
            decision(&cut(), &prepared(), &changed_capability, &completeness),
            ComparisonDecision::PendingPeerProof
        );
        changed_capability = capability.clone();
        changed_capability.fresh = false;
        assert_eq!(
            decision(&cut(), &prepared(), &changed_capability, &completeness),
            ComparisonDecision::PendingPeerProof
        );
        changed_capability = capability.clone();
        changed_capability.authenticated = false;
        assert_eq!(
            decision(&cut(), &prepared(), &changed_capability, &completeness),
            ComparisonDecision::PendingPeerProof
        );
        let mut changed_proof = completeness.clone();
        for value in [None, Some(false)] {
            changed_proof.complete = value;
            assert_eq!(
                decision(&cut(), &prepared(), &capability, &changed_proof),
                ComparisonDecision::PendingPeerProof
            );
        }
        changed_proof = completeness.clone();
        changed_proof.inventory_complete = false;
        assert_eq!(
            decision(&cut(), &prepared(), &capability, &changed_proof),
            ComparisonDecision::PendingPeerProof
        );
        changed_proof = completeness.clone();
        changed_proof.asserted.projection = digest('f');
        assert_eq!(
            decision(&cut(), &prepared(), &capability, &changed_proof),
            ComparisonDecision::PendingPeerProof
        );
        changed_proof = completeness.clone();
        changed_proof.transmitted.inventory = digest('f');
        changed_proof.asserted.inventory = digest('f');
        assert_eq!(
            decision(&cut(), &prepared(), &capability, &changed_proof),
            ComparisonDecision::PendingPeerProof
        );
        changed_proof = completeness.clone();
        changed_proof.qualified_producer = false;
        assert_eq!(
            decision(&cut(), &prepared(), &capability, &changed_proof),
            ComparisonDecision::PendingPeerProof
        );
        // An unequal, but complete, remote graph is eligible for comparison and healing.
        changed_proof = completeness.clone();
        changed_proof.transmitted.legacy = digest('f');
        changed_proof.asserted.legacy = digest('f');
        assert_eq!(
            decision(&cut(), &prepared(), &capability, &changed_proof),
            ComparisonDecision::Eligible
        );
    }

    #[test]
    fn legacy_catch_up_is_unqualified_and_missing_proof_never_enables_actions() {
        let (capability, completeness) = peer();
        let legacy = decide_comparison(&cut(), &prepared(), false, &session(), PeerLane::Legacy);
        assert_eq!(legacy, ComparisonDecision::LegacyUnqualified);
        assert!(!legacy.permits_projection_actions());
        let missing = decide_comparison(
            &cut(),
            &prepared(),
            false,
            &session(),
            PeerLane::Prepared {
                capability: None,
                completeness: Some(&completeness),
            },
        );
        assert_eq!(missing, ComparisonDecision::PendingPeerProof);
        assert!(!missing.permits_projection_actions());
        let mut changed_local = cut();
        changed_local.pending_generation += 1;
        let changed = decision(&changed_local, &prepared(), &capability, &completeness);
        assert_eq!(
            changed,
            ComparisonDecision::RetryLocalCut(CutRefusal::Changed)
        );
        assert!(!changed.permits_projection_actions());
        assert_eq!(
            decide_comparison(&cut(), &prepared(), true, &session(), PeerLane::Legacy),
            ComparisonDecision::RetryLocalCut(CutRefusal::Cancelled)
        );
    }
}
