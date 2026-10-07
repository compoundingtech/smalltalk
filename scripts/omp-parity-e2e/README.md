# OMP conversation parity proof

Runs an isolated, real st3 daemon, its client HTTP API, the shared TypeScript
conversation renderer, and interactive stui. Requires binaries containing the
incremental/keyset and OMP parity changes, including the live palette affordance
for opening a saved `session/...` id. It never launches OMP, calls a model, modifies
an original transcript, or simulates a live OMP process.

From the repository root, with previously built binaries:

```bash
scripts/omp-parity-e2e/run --st3 "$PWD/target/debug/st3" --stui "$PWD/target/debug/stui" --out "$PWD/tmp/omp-parity-e2e"
```

Optional: add `--corpus /absolute/path/to/session.jsonl` for private first-page
before/after-append timings and full-window parity checks; add `--keep` to retain
the isolated runtime for diagnosis. The original corpus is read-only. Corpus
copies, API responses, and daemon logs stay in the isolated directory, not the
output directory; `--keep` therefore retains potentially sensitive data. Reports
contain only kind/view/source-type labels, counts, timings, and synthetic proof.

Dependencies: bash, curl, jq, Node with native TypeScript support (Node 22.18+ or
24+), tmux, realpath, mktemp, and the usual Unix core utilities. Run the renderer
alone with `node scripts/omp-parity-e2e/render.mjs tmp/omp-parity-e2e/page.json`.
`OMP_E2E_TIMEOUT` controls each polling/HTTP timeout in seconds (default 45).
`TMPDIR` controls temporary runtime and tmux socket placement. HOME and all XDG
state/config/cache/runtime paths are overridden; ST3_ENDPOINT/ST3_PERSON are set
only to the isolated daemon's Unix socket and `person/omp-e2e`.

Every check writes `PASS` or `FAIL` to `<out>/report.txt`. Independent checks
continue after failure; failed setup dependencies stop immediately. The final
exit is nonzero if any check failed.
Synthetic pages, chunk responses, TS rows, parent stui capture (`stui.txt`) and
child capture (`stui-child.txt`) are retained as evidence. Cleanup stops the
terminal and daemon and removes isolation unless `--keep` was supplied.

## Assertions

- Dependencies, executable binaries, readable fixtures, doctor readiness,
  discovery by historical native session id, and successful API fetches.
- Each tool-call view: bash, edit, write, read, search, todo, ask, task, hub, eval,
  generic. Each tool-output view: bash, edit, todo, ask, task, hub, generic.
- Each bookkeeping view: irc, job, skill, compaction, model_change,
  thinking_level, title, session_exit, tool_start. An `irc` kind exists and every
  tool-start block is internal.
- Model, context, cost, and todos header fields have transcript provenance,
  timestamp and value; their values are populated. A task result links
  ParityChild to an `external-child` conversation.
- The identical unnegotiated request has neither blocks nor header.
- The linked child includes exactly the four child message record ids, with
  start, tool-result and completion content, and no parent records.
- Limit 200 produces an older-page cursor. Full negotiated and legacy walks
  (bounded at 20 pages) exhaust history, contain unique ids, and have identical
  id sets. An oversized-content ref exists and fetches.
- Appending assistant usage/model appears on the newest page; cost increases
  by exactly $0.25, model becomes synthetic/appended-model, and context is 31.
- The original cursor still fetches page two; its ordered ids are unchanged;
  original page one plus post-append page two have no duplicate or skipped ids
  in that range. A full old-keyset walk after append equals the entire baseline
  id set without duplicates. The old ref returns identical bytes, size and ref
  after append, with size greater than 8 KiB.
- Shared TS rendering runs and shows a bash `$ ` row, todo checklist mark,
  child `open session/` row, IRC sender/message, and transcript header.
- Real stui connects, opens its palette and the saved parent, renders parent
  content, a bash command row, transcript model header and child open row.
  Activating that row shows child completion without parent assistant content.
- Both renderers hide internal-only tool-start rows instead of drawing them
  standalone.
- Optional corpus discovery and both timed fetches succeed. A full negotiated
  walk (maximum 100 pages of 200) must terminate with unique entry ids, zero
  unknown blocks, and a typed view on every tool call. Full-window kind/view
  counts and before/after timings are informational, not performance thresholds.
  If unknown blocks exist, their source-type counts are printed without payloads.

## Terminal navigation

Ctrl+K opens the live palette; type the discovered full `session/...` id and
Enter opens the saved conversation. Shift+O selects full conversation density;
`o` opens all tool outputs; End follows the recent bash/subagent showcase.
The child card's `open session/...` row is a left-button `PaneIntent::Open` hit,
not a keyboard focus/Enter binding. The harness finds that row in capture-pane
and sends a real SGR mouse click at its coordinates. All waits poll for content
with a deadline; there are no fixed startup sleeps.

The isolated tmux session is named `omp-e2e`, with a unique socket under
`${TMPDIR:-/tmp}/agent-tmux-sockets/omp-e2e-*.sock` (not the ambient server).
The exact `tmux -S ... attach -t omp-e2e` monitor command is printed on startup.
If the saved-session palette affordance is missing, the stui check fails rather
than creating a host-visible fake OMP process or substituting a demo renderer.
