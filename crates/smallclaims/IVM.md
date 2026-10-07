# Incremental views in smallclaims

`smallclaims::ivm` maintains keyed derived state in the same transaction as admitted
claims. It is an opt-in library API. This change does not install views in the daemon,
switch application readers, migrate existing graphs, or replay history at open or read time.

A `View` declares its durable and local input kinds, a version fingerprint and a maximum
contribution/dependency count. `contributions` maps one admitted claim to keyed registers;
heads use canonical rank rather than arrival order. Indexed retraction preserves old keys.
Custom operators can maintain their own indexed tables through `affected_keys`,
`canonical_dependencies`, `maintain_key` and `maintain_local_key`.

The fingerprint must describe the input registry, eligibility, authority, key/dependency
extraction and output semantics. Contributions, custom output, semantic generations and
source cuts commit or roll back together. A failed operator rolls back its own savepoint
and becomes unavailable while preserving valid source admission.

## Small register example

This is a latest-field register, not a complete operational agent card or authority policy.
The application-shaped examples below cover more complex restricted relations.

```rust
use std::{collections::BTreeMap, sync::Arc};
use anyhow::{Context, Result};
use serde_json::json;
use smallclaims::{ClaimInput, ClaimRecord, Store};
use smallclaims::ivm::{Contribution, Definition, View, Views, source_cut};
use smallclaims::ivm::runtime::ViewRuntime;
use smallclaims::store::canonical;

struct Status;
impl View for Status {
    fn definition(&self) -> Definition {
        Definition {
            name: "example-status",
            fingerprint: "status.v1;example.status;subject-key;canonical.v1",
            kinds: &["example.status"],
            local_kinds: &[],
            max_contributions: 1,
        }
    }
    fn contributions(
        &self, claim: &ClaimRecord, key: &canonical::ClaimKey,
    ) -> Result<Vec<Contribution>> {
        Ok(vec![Contribution {
            key: claim.subject.clone(), register: "status".into(),
            value: claim.body["fields"]["status"].clone(),
            rank: canonical::sortable_key(key),
        }])
    }
}

fn main() -> Result<()> {
    let runtime = Arc::new(ViewRuntime::new(Views::new(vec![Box::new(Status)])?)?);
    let store = Store::open_memory("example", runtime.clone())?;
    store.append_claim(&ClaimInput {
        subject: "agent/example".into(), kind: "example.status".into(), actor: None,
        fields: BTreeMap::from([("status".into(), json!("ready"))]),
        evidence: vec![], expected_subject: None, idempotency_key: None,
    })?;
    let head = store.read_snapshot(|_| {
        let connection = store.readers.get();
        let cut = source_cut(&connection)?.context("source unavailable")?;
        runtime.views.head(&connection, "example-status", "agent/example", "status", cut)
    })?;
    assert_eq!(head.context("missing status")?.value, json!("ready"));
    Ok(())
}
```

Production runtimes retain their own schema and authority admission. They call
`Views::change` for admitted old/new inputs and `publish_cut` in that transaction.
`ViewRuntime` is a claims-only reference runtime, with Plain-like schema acceptance;
it must not replace an application's authorization policy. Its default is no history
backfill. Missing or incompatible views on populated sources stay unavailable.

## Reads and invalidations

Capture readiness, source cut and actual output inside one read snapshot. `head` checks
that cut; `readiness` and `availability` expose missing, pending and fenced evidence.
Availability revisions are distinct from semantic generations: unrelated admission can
make the source pending without changing a key's answer. `changed_keys` is a coalesced
current-key invalidation feed with retained removals and gap detection. It is not durable
transition replay, an action request, authorization, or an immutable historical page.
Commit-only notifications and subscribe-before-snapshot/recheck are consumer-owned.

The opt-in [`events` feed](IVM_EVENTS.md) supplies bounded committed keyed invalidations,
independent availability delivery, stable provider identity and explicit retained floors.
It uses the existing writer commit observer; graph-watch still owns client transport and
authorization. The reference runtime gates replacement-repair views pending a proved
repair-before-projection eligibility implementation.

For current observations, `LocalChange` captures old/new keys and a separate local generation.
Clock, deadline, ownership and external-file dependencies require explicit adapters;
replicated claim frontiers cannot stand in for them.

## Explicit installation

`ivm::install::Installer` supports bounded, resumable shadow namespaces, indexed extraction
pages, transactional mutation journals, quotas, cancellation and atomic publication.
`ivm::claim_source::ClaimSource` is an opt-in retained claims-only source adapter. No
installer job is started by ordinary Store open, repair, checkpoint or a reader. Callers
explicitly register complete source coverage, start a job, release each extraction snapshot
before writing its page, catch up and publish. See [the installer contract](tests/IVM_INSTALL.md)
and [the retained source contract](tests/IVM_CLAIM_SOURCE.md).

Historical extraction does not seed captured rank provenance. Ordinary sealing after a
populated-source install therefore conservatively fences output until explicit recovery.
Unsupported rank edits, incompatible epochs and incomplete source coverage also fence.
The reference runtime refuses checkpoint lifecycle before work; exclusive runtime/file
ownership is a precondition. It cannot join a production checkpoint fleet or certify
retention, restore, mixed-build compatibility or a cross-handle capability fence.

## Application-shaped controls

[The fixture guide](tests/IVM_REAL_VIEWS.md) describes agent cards, account limits, fleet
membership, mailbox unread counts, mission run/step trees and desired-by-host candidates.
These use real Store append and signed replication, shuffled/duplicate delivery, rollback,
reopen and independent test-only raw-history oracles. Their documented restrictions matter:
full authority closure, owned-set omissions/repair and the general ordered run tree are
consumer work; registers alone do not prove those operators.

Run the 64 controls with normal Cargo configuration and the configured target runner:

```sh
cargo test --locked -p smallclaims \
  --test ivm --test ivm_install --test ivm_real_views --test ivm_claim_source
```

Canonical legacy rank calculation retains its existing same-batch COUNT cost. Candidate
retention and commit-inclusive writer cost are not bounded by the correctness results.
Integrators must measure relevant unchanged-answer history growth, genuine fanout, SQL/VM
work, retained bytes and writer wait/hold before activating a reader. Each reader migration
is a separate PR; this crate addition does not activate one.

The opt-in [read-after-write API](IVM_AFTER_WRITE.md) returns database-bound receipts and waits reactively for certified source processing plus view readiness.
