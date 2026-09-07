# Doctor requirements

Doctor checks one catalog from one host. It follows the root
[vision](../vision.md). It refines [R03](../requirements.md),
[R04](../requirements.md), and [R08](../requirements.md).
It does not define fleet health.

## Requirements

- **DOCTOR-R01 Diagnostic subject:** One run checks one selected catalog and one
  selected host. The result does not claim that a remote host is available.
- **DOCTOR-R02 Gate result:** A healthy result returns zero. Any diagnosed
  problem returns non-zero. Each problem names the affected declaration, task,
  runtime, catalog, or supervision subject.
- **DOCTOR-R03 Read-only diagnosis:** A run must not change catalog, presence,
  or runtime state. It must not reconcile, launch, stop, reap, repair, or
  materialize work.
- **DOCTOR-R04 Bounded non-interactive operation:** Doctor and its external
  probes must not read the caller's standard input. A probe must stop within a
  fixed time. A timed-out probe is a failed check, not a hung caller.
- **DOCTOR-R05 Active local health:** Doctor checks each active declaration on
  the selected host. It checks that declared work is alive and that presence is
  maintained. It does not check remote availability.
- **DOCTOR-R06 Retired absence:** A retired declaration is healthy only when all
  declared task records are absent. A live or dead record is unhealthy. A
  retired declaration does not require presence or active-declaration checks.
- **DOCTOR-R07 Suspended absence:** A suspended declaration is healthy only
  when no declared task is alive and every retained dead task record is
  explicitly keep-pinned. It does not require presence or active-declaration
  checks. A dead non-keep record is unhealthy because ordinary reconciliation
  still owes its collection. This weaker predicate does not satisfy retirement.
- **DOCTOR-R08 Outbound message health:** Doctor validates each local agent's
  sender ledger without creating or repairing sender state. An invalid ledger
  is a failed check that says the agent cannot send. An unavailable ledger is
  healthy because no sender history exists.

The [Retirement health invariant](../../../INVARIANTS.md) and its tests prove
the retirement rule. The [specification](spec.md) owns the mechanism, check
groups, and known gaps.
