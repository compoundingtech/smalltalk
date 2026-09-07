# Agent desired-state E2E prototype

Date: 2026-08-04

## Question

Can one typed Agent Spec desired-state model express reversible
whole-agent absence, legacy retirement compatibility, rationale validity, and
existing `keep`/`adopt-only` behavior without creating a second runtime state
machine?

## Method

A disposable Rust model enumerated the complete bounded product of three
desired states, three runtime observations, two task lifecycles, and two keep
values. An independent invariant oracle checked 9 validity cases and 36
planning cases. The surviving model was then patched into an isolated st2 clone
based on the full strict-admission/revision-receipt stack and exercised through
the real parser, source author, planner, PTY backend, exec backend, message bus,
roster, inventory, human listing, and cleanup.

## Result

The oracle passed all 45 cases. In the real run, initial reconcile launched a
sibling, worker PTY, and generated worker DING. Suspending the worker tore down
exactly the worker and DING while adopting the sibling with its existing PID.
After backend settlement both worker tasks were absent. Resume relaunched
exactly the worker and DING and again adopted the sibling. A message delivered
before suspension retained the same inbox filename after resume.

The task inventory needed two distinct levels: task `desiredState=absent` and
`agentDesiredState=suspended` with its rationale. Presence also remained an
independent last-observed value, confirming it cannot represent lifecycle
intent.

## Resolved decisions

- Running is accepted explicitly but canonical authoring omits it.
- New suspended and retired states require a rationale; legacy retirement does
  not. Any mixed old/new syntax is invalid, including `retired #false`.
- Suspended health permits only keep-pinned dead records; retired health permits
  no records.
- Resume preserves `keep` and `adopt-only`; it has no checkpoint semantics.
- Generated companions follow the owning agent desired state. Durable inbox,
  context, resources, and declaration state do not.
- Authoring receipts report durable intent, not runtime completion.

## Conclusion

Yes. One closed desired-state model handles reversible suspension and
retirement without adding a runtime state machine. The production design must
retain the stronger retirement completion predicate, preserve task policies on
resume, and expose declaration intent separately from task absence, presence,
and runtime observation.

## VRS Impact

The result adds root R41 and R28, Agent Spec field rule F18, Doctor R07, the
agent desired state/suspension/retirement/rationale ontology, and corresponding
wire and acceptance text. It does not change the vision.

The disposable prototype itself was deleted after these results were lifted
into requirements, tests, and production code.
