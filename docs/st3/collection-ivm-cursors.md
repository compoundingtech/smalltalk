# Collection IVM cursor integration

This integration is an inactive production seam. No complete collection adapter is
registered by default. The person-step/custom attention, harness and mission-selection
views are partial operators; their Ready status does not certify a whole collection.
Existing collections continue using their current reader until a complete adapter is
explicitly admitted.

`Store::ivm_views()` and `Store::ivm_publisher()` provide the explicitly installed shared
registry and one serialized Publisher Arc. The collection bridge uses `from_shared`,
never attaches another Publisher and never substitutes the claims-only runtime.
One socket subscribes before its first authorized snapshot. All reads use the same
Store snapshot for authorization, source cut, availability, key/status pages and rows.

A source adapter receives the entire subscription, authorized session, snapshot boundary,
a bounded page of private invalidation keys and the previously delivered public rows.
Keys are hints, not row contents or proof that retained rows are current. Select bounded
ordered IDs after authorization and literal query filtering, verify each reused row's
current generation/dependencies at this cut, and fetch only changed/promoted rows.
Content, removals, rank, membership, count and `has_more` must all agree with this cut.
Do not fold history or recompute a complete collection in this callback. A replacement
subscription resets delivered state and cursors; page seek tokens remain separate from
watch replay cursors.

The consumer emits existing v0 `snapshot`, `changes`, `resync` and `error` frames. It
acknowledges both key and availability cursors only after successful delivery or proven
silence. Bounded continuations retain their snapshot version; changed versions retry
from the old acknowledged boundary. Journal expiry or provider replacement emits a
resync followed by a fresh authoritative snapshot. Availability Ready refreshes rows
even without changed keys. While unavailable, old rows stay stale; the server never
invents removals. Reconnect always starts a new snapshot, without a v0 replay cursor.

IVM subscriptions use committed notices and explicit continuations, without the legacy
periodic window reader. Source-owned captured deadline transitions must publish
transactional invalidations. Protocol pings do not read windows. The current subscription
and physical read limit is sixteen; clients negotiating with older daemons retain eight.

Activation requires transaction-owned admitted old/new facts and reverse dependencies,
accepted repair/signature/rank changes, authenticated local replacements and captured
clock inputs. Every covered mutation must enter the existing foundation Installer/source
lifecycle in its source transaction; unsupported/missing coverage must produce a gap and
committed unavailability. Publishing a largest index or running `after_projection` is
insufficient. The consumer additionally checks admission against the current Store index,
but that check cannot establish same-index authority/local coverage. Populated sources
require explicit bounded install/backfill/catchup and parity before publication.

Isolated real Store/socket controls use a complete invented local-only source. They prove
transport delivery, convergence and silence, not production adapter certification.
