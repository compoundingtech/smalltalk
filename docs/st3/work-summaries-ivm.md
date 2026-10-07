# Work summary source operator

`store/work_summaries_ivm.rs` prepares the private `st3.work-summaries.v1`
relation. It replaces the algorithm behind progress/completion summary history
folds with canonical indexed candidates and captured-time maintenance. It is
not registered by the default runtime and no production reader is switched.

Each admitted `work.progress` or `work.submitted` contributes at most one
subject/attempt/register candidate. Selection preserves the existing reader's
canonical tuple, current attempt, trimmed nonempty summaries, sparse legacy
body fields and exclusion of every `handoff_acknowledged` field (including
null). Repaired originals remain eligible, matching the existing summary
reader. Explicit retractions maintain old and new subject/attempt keys.

The operator selects the latest candidate through its writer-maintained clock
using the indexed `smallclaims.ivm` contribution relation. Canonical sortable
ranks start with a fixed 16-byte big-endian accepted time, so an exclusive next
millisecond prefix includes every tie at the captured instant. Both clock and
deadline storage preserve the complete u128 domain, including its maximum.
The coupling to the foundation's versioned rank/layout requires a new
fingerprint if that representation changes.

Future candidates remain invisible. Each affected key records its first future
accepted time; an indexed deadline queue visits only due keys. A clock page
handles at most 1,024 keys. Partial pages and regressed clocks are explicitly
unavailable. Reads never advance the clock or fall back to raw claim history.
Unrelated kinds, losing older candidates and duplicate contributions do not
rewrite summary output. A completion revision with the same text is unchanged
because the existing public summary has no completion timestamp/revision.

Prepared interfaces:

- `definitions() -> Vec<Box<dyn View>>` registers the source relation.
- `clock_page(tx, views, captured_time, limit) -> Result<usize>` is writer-only.
- `row(connection, views, subject, attempt, captured_time)` returns summaries.
- `rows(connection, views, selected, captured_time)` reads at most 501 selected
  subject/attempt pairs in one query.
- `next_deadline(connection, views, captured_time)` returns the first future
  candidate time with an indexed seek.

Changed keys are private JSON subject/attempt tuples, not public work IDs.
The work dependency adapter must invalidate the corresponding public step.
An absent summary means empty summary fields; it does not establish that a
step exists, belongs to a current generation or is authorized for a caller.

Before adoption, the shared Store source owner must capture every admitted
claim mutation and canonical rank dependency in the writer transaction,
certify a complete contiguous projected source cut, and drain due clock pages.
The admitted/projected/snapshot frontier equality checked by reads is necessary
but does not certify complete hooks. The tests' explicit capture after completed
Store actions is a fixture, not a production largest-index certificate.

Fresh empty installation and compatible persisted state are supported by the
foundation contract. Populated installation remains fenced until an explicit
reviewed installer provides all candidates. Custom local rows have no accepted
checkpoint/digest/restore participation yet; do not enable checkpoint dropping
or claim cross-runtime recovery from this module.

Full work collection adoption separately requires effective run/generation/root
state, requests, blockers, leases, timing, wake, response, queue and actor
authority dependencies. This summary relation supplies none of those proofs.
