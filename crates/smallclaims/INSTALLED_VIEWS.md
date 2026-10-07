# Namespace operators in the shared view registry

A production source adapter can explicitly expose a proved Installer namespace through the
existing Views and event registry. This installs no source adapter, transport or migration;
ordinary views preserve their fingerprint bytes and default behavior. No bridge SQL runs
for an ordinary view read. Namespace mode adds indexed metadata validation to its reads.

Declare `View::installed_source() -> Some(source)` and use exactly the same name and raw
fingerprint in its `Definition` and `install::Operator`. The fingerprint must cover all
input, extractor, dependency, authority, clock and public-row semantics. Namespace mode
has its own versioned view fingerprint, does not dispatch legacy claim/local callbacks,
and rejects legacy register/head reads. Every operator table/index/query includes Namespace.
The source owner supplies full replacements and proves complete source coverage; this
bridge is not an admission, signer, canonical-order or full-card reducer implementation.

Explicit setup, after source attestation and normal missing-on-populated initialization:

```rust,ignore
views.register_installed(transaction, &installer, view)?;
let job = installer.start(transaction, view, limits, now_ms)?;
// Explicit indexed scan pages and journal catch-up; never on GET or ordinary startup.
// The two certificates must describe the same covered source state, independently:
let position = installer.position(transaction, source)?;
let outcome = views.catch_up_installed(
    transaction, &installer, &job, &position, certified_graph_cut, now_ms,
)?;
```

Registration fences output; it cannot adopt an older ready root. The typed catch-up call
runs one normal bounded Installer page and promotes only a fresh namespace completed by
that call, in the same transaction. It verifies compiled operator/view/source binding,
exact current SourcePosition and graph epoch/prefix. SourcePosition revision is a source
mutation count, not a claim store index; its epoch can also differ from graph epoch.
`certified_graph_cut` is an independent source-owner attestation, not one constructed from
MAX index alone. Its entire shared registry must be processed or fenced. Equality with the
actual graph frontier is a necessary check, not proof of source/extractor/authority coverage.
Publication does not enumerate every output key; it advances availability and semantic
version for an explicit full authorized bounded-window refresh. Rollback hides all of it.

After live `Installer::record` calls, synchronize in the SAME source transaction:

```rust,ignore
let result = views.sync_installed(
    transaction, &installer, view, &current_source_position, certified_graph_cut,
    installed::Changed::Keys(&semantic_old_and_new_memberships),
)?;
```

Keys include removals and all semantic membership/order/content/authorization changes.
The per-call bound is 1024 distinct nonempty keys, each at most 4096 bytes and together at
most 1 MiB. `Changed::Refresh` explicitly requests a full authorized bounded-window refresh.
An unchanged root with no changed keys preserves semantic/key generations. Unrelated graph
progress may advance the shared cut without rewriting the view. Live sync requires the
same active namespace, cannot clear a later gap, and returns `SyncOutcome::Fenced` on
logical unavailability or excessive changed keys, preserving valid source admission and
the last certificate/cut. Bridge storage errors propagate to the outer transaction.

Use `Views::installed_root(connection, &installer, view)` and `events::capture` in the SAME
short read snapshot as the authorized namespace rows. Ordinary token/readiness validation
checks compiled binding, namespace, root/source identity, epoch, current revision and
semantic generation, as well as the graph cut. Missing or altered state is unavailable;
no read heals it. This is a processing certificate, not an arbitrary row-integrity or
authorization proof; operator evidence and authorized row validation remain mandatory.
A root changed without synchronization cannot remain publicly Ready,
even if an old ready flag is present. Commit notification and packet delivery are still the
existing single Publisher/graph-watch adapter; a wake is not authorization or action proof.

Source owners MUST mirror/fence every relevant Installer/source lifecycle mutation through
this bridge in its transaction. Direct Installer mutations bypassing that contract reject
fresh reads but do not guarantee an availability notification. Raw capture gaps must fence
both namespace/source state and registered view state (and be independently rejected by
source-certified reads). Installation callbacks, source capture, local observations,
deadlines, authority, repair/rank changes, DDL/uninstall, restore, retention and incompatible
source-identity transitions remain owner obligations. This PR has no incompatible binding
replacement or checkpoint participation protocol; incompatible registration is refused.
The bridge performs no full-history reconstruction or production activation. Callback,
bootstrap and commit-inclusive writer cost remain separate measurement gates.
