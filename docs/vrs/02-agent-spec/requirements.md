# st2 Agent Spec field-change conformance requirements

## Context

The canonical Agent Spec and its proof corpus live in
[compoundingtech/evals at commit `e9b53e79b05b1c0e1d7eea02db2eaba47376fe05`](https://github.com/compoundingtech/evals/blob/e9b53e79b05b1c0e1d7eea02db2eaba47376fe05/AGENT-SPEC.md).
st2 is one implementation. Another implementation, including a future st3, can
target the same contract and proofs.

This sub-VRS defines desired st2 conformance when a normalized Agent Spec
changes work on one host. It records st2-specific gaps; it does not own or copy
the Agent Spec. General field-change behavior that the canonical specification
does not yet define is proposed until a matching evals specification and proof
change adopts it.

This work supports root [R01, R06, R11, R13 through R19, R24 through R26, and
R31](../requirements.md).
It applies to every st2 tool that runs an agent or test. A valid local catalog
and host-local runtime state are sufficient. It requires no compare-and-swap
(CAS), lock service, cross-host call, or external registry.

[spec.md](./spec.md) defines the proposed field-change rules and current st2
implementation gaps. The root st2 VRS has authority for st2 behavior; the
canonical evals Agent Spec remains the authoring authority.

Field lookup: [F01](./spec.md#f01), [F02](./spec.md#f02),
[F03](./spec.md#f03), [F04](./spec.md#f04), [F05](./spec.md#f05),
[F06](./spec.md#f06), [F07](./spec.md#f07), [F08](./spec.md#f08),
[F09](./spec.md#f09), [F10](./spec.md#f10), [F11](./spec.md#f11),
[F12](./spec.md#f12), [F13](./spec.md#f13), [F14](./spec.md#f14),
[F15](./spec.md#f15), [F16](./spec.md#f16), [F17](./spec.md#f17), and F18 in
the same field-rules specification.

## Shared invariants

- **SPEC-R01 Normalize and classify before action.** Compare the normalized
  effects, not source bytes alone. The comparison includes the render plan,
  resolved addresses and paths, task set, launch fingerprints, policy,
  retirement state, and host membership. Formatting, order, comments, or a
  source path change are `no-op` only when every normalized effect is equal.
  A source-only `no-op` authorizes no write or notification. It does not block
  healing of independently absent or dead work. A provider field has no core
  effect until a provider converts it into core fields.

- **SPEC-R02 Do not change work whose proof is incomplete.** Before a change,
  validate all input that can affect the related agents, tasks, and files.
  Prove the current owner of each affected process and file. When input is
  partial, unreadable, ambiguous, or conflicting, st2 retains the latest locally
  proved ownership. Such input never proves removal. Destructive action requires
  proof that binds the exact catalog, host, owner, task ID, and live incarnation
  (one task generation).
  Work without this proof stays in `hold`. Independent agents, tasks, and files
  can proceed when their input and ownership proof are complete. No prior
  synchronized snapshot, deletion record, or CAS is required.

- **SPEC-R03 Preserve live work unless the field rule changes it.** Metadata,
  live context, render data, Resource data, and future policy do not restart a
  healthy task. A changed workspace, render target, or Resource that is visible
  to a surviving task causes one post-commit notification. Absent or dead work
  boots with the latest desired data and gets no change notification. All spawn
  inputs form a versioned launch fingerprint. A healthy fingerprint mismatch is
  visible as `drifted` or `unknown`, but it does not authorize replacement.
  Unrelated work receives no action.
  Presentation changes use exact-ID metadata projection only; they do not alter
  launch fingerprints or authorize lifecycle work.

- **SPEC-R04 Change membership and lifecycle only for exact IDs.** Add only a
  missing ID. Remove only an exactly attributed old ID. Retirement stops the
  declared set and prevents relaunch. An identity, task name, or task ID change
  is remove-old then add-new, not an inferred rename. Each host acts only on its
  local projection. A host change is independent removal on the old host and
  addition on the new host, never process migration. The hosts require no shared
  order, receipt, or proof. Catalog skew can cause temporary overlap or absence;
  each host retains its local last-known-good ownership.

  Suspension is reversible desired absence, not a second runtime state
  machine. It tears down the same exact owned task set as retirement, including
  derived companions, but permits explicitly keep-pinned dead records and
  preserves all catalog-backed durable state. Retirement completes only when
  every declared task record is absent. Returning to running uses ordinary
  `keep`, `adopt-only`, ownership, and launch rules.

- **SPEC-R05 Plan every related action before mutation.** Plan every action,
  refusal, proof, conflict, and rollback condition for the related agents,
  tasks, and files. Use `FENCE`, `REMOVE/QUIESCE`, `MATERIALIZE`, `ADD/BOOT`,
  `NOTIFY`, then `VERIFY/REPORT`, and omit empty phases. Fence and remove an
  exact conflicting old incarnation before an add. Use final desired bytes for
  the add. A deletion requires explicit desired state and ownership proof, and
  never removes a catalog source declaration. Roll back only when rollback is
  proved; otherwise hold or refuse before a dependent phase. Explicit
  replacement authority is required for a drifted live incarnation.

- **SPEC-R06 Send one quiet event after commit.** When a committed workspace,
  render, or Resource change is visible to a survivor, write one coalesced
  durable inbox event and then try DING. DING is best-effort delivery through the
  configured adapter; this contract requires no terminal input or harness
  behavior. The event has a stable ID, and repeating that ID has no additional
  effect. It identifies the agent, host, changed path, and change class, but
  contains no file contents or secrets. Delivery failure does not remove the
  event or roll back the commit. Do not notify for unchanged, failed,
  rolled-back, periodic, new, replaced, or retired work. A notification cannot
  restart work or cause another notification.

## Evidence boundary

After approval, an external matrix must prove every
[field rule](./spec.md#field-rules) and
[acceptance case](./spec.md#acceptance-cases) through public CLI behavior and
isolated PTY and exec state. Unit tests alone do not prove this contract.
