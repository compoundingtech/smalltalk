# Native record coverage fixtures

`codex.jsonl` and `claude.jsonl` use native transcript record types and shapes
already exercised by `external_sessions` regression tests and the visibility audit.
They are constructed contract fixtures with invented contents, not live transcript captures.
They keep harness-specific types and nested message/response shapes instead of
crossing unrelated types, roles and display flags between harnesses.

`omp-parity.jsonl` is entirely synthetic. It covers every typed OMP tool call/output
family, extension and bookkeeping view, hidden extensions, and assistant usage/context
metadata. Field shapes follow native OMP records; no live conversation text is copied.
Pi exercises the same normalizer and fixture. The task output names `ParityChild`
for owner-local child-conversation linking.
The todo snapshot uses the five native OMP statuses: `completed`, `pending`,
`in_progress`, `blocked`, and `abandoned`; `done` is not an OMP todo status.

The coverage test also reads the existing redacted OMP native resume/tool-result
captures (Pi shares that JSONL session format), and extracts actual message `info`
and native `part` objects from the installed OpenCode 1.18.34 admission capture in
`crates/st-drivers/tests/fixtures/harness-admission/opencode-1.18.34.json`.
Those objects are the data stored in OpenCode's message/part rows, not its outer
SSE notification envelope. Unknown future types are separate constructed probes.
