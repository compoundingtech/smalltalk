# coord — LAYOUT

The minimal invariants every coord implementation must respect. Anything not
in this file is open for the implementation to decide and iterate on.

## The root

`$COORD_ROOT` (default `~/.local/state/coord`) holds one sub-folder per
**identity**.

```
$COORD_ROOT/
  alice/
    inbox/
    archive/
  bob/
    inbox/
    archive/
```

The folder is 100% syncable — every file under `$COORD_ROOT/` participates in
sync. There are no machine-local marker files inside the folder.

## Identity rules

- An **identity** is a sub-folder name under `$COORD_ROOT/`. The folder name
  is the identity's id.
- An identity is normally hosted by a single participant — that participant's
  machine is the canonical place where the identity's writes originate. A
  *shared* identity (a "team alias" — `dispatch`, `oncall`, etc.) is also
  supported: multiple participants keep the folder synced and share the
  inbox. See [walkthrough.md](notes/walkthrough.md) for usage patterns.
- Lowercase ASCII alphanumeric, hyphens, and periods. Must start and end
  with an alphanumeric. Periods encode hierarchy in the flat namespace
  (e.g. `persona.session-1.child-7`) — see issue #1. No upper bound on
  length beyond what the underlying filesystem accepts.
- Reserved names (must not be used as an identity): `inbox`, `archive`,
  `tasks`, `journal`, `status`, `name`, `available`, `busy`, `away`,
  `dnd`, `offline`, `unknown`, `members`, `overview`.
- Every identity sub-folder contains at minimum two folders: `inbox/` and
  `archive/`. Two further optional folders, `tasks/` and `journal/`, may
  also be present (see below). No other sub-folders are part of the
  convention.
- An identity may *optionally* have a single-line `<identity>/name` file
  containing a human-friendly display name. Synced like everything else.

## Filenames

Every message file in `inbox/` or `archive/` has a globally unique name:

```
<unix-ms>-<rand6>.md
```

- `<unix-ms>` — 13-digit Unix time in milliseconds.
- `<rand6>` — six characters from Crockford base32 (`0-9a-z` minus `i`,
  `l`, `o`, `u`).

Sortable by time. Effectively unique without coordination — at human-scale
write rates the rand6 namespace (~10⁹) eliminates collisions.

## File contents

Each message is markdown with a YAML frontmatter block:

```markdown
---
from: alice
subject: optional subject line
in-reply-to: <filename of message being replied to>
---
Body goes here, as markdown.
```

Required frontmatter key: `from`. All others are optional.

Readers must be permissive — missing or malformed frontmatter is treated as
an untyped message; the body is still readable.

The recipient (`to:`) is *not* in the frontmatter. The path tells you: a
file at `$COORD_ROOT/bob/inbox/<filename>` is addressed to `bob`. The
timestamp is *not* in the frontmatter either — the filename's `<unix-ms>`
prefix is the canonical send time.

## The folders

- **`inbox/`** holds messages addressed to this identity that have not yet
  been processed.
- **`archive/`** holds messages that have been processed. Move (`mv`) is the
  only operation that puts a file in archive.
- **`tasks/`** *(optional)* is the identity's own work queue. Each file is
  a markdown document with optional YAML frontmatter (`priority`, `tags`,
  `due`, `status`). Unlike `inbox/` and `archive/`, files in `tasks/` are
  mutable — the identity owner edits them as work progresses. **Only the
  identity owner writes here**; other participants read via sync but never
  write. Single-writer keeps rsync convergence trivial: there is no
  multi-machine concurrent mutation to reconcile. The folder is created
  lazily by the CLI; an identity that has no tasks simply has no `tasks/`
  folder. Filenames are not constrained by the inbox grammar — `coord
  task new` defaults to `<unix-ms>-<slug>.md`, but any markdown filename
  is accepted by readers.
- **`journal/`** *(optional)* is the identity's own terse work log,
  audience-facing. Each entry is its own markdown file named
  `<unix-ms>-<slug>.md`, optionally with `topic` / `tags` frontmatter.
  **Only the identity owner writes here**; other participants read via
  sync. **Append-only at file granularity** — never edit an entry once
  written, only add new ones. Distinct from `tasks/` (which carry
  state) and from `inbox/archive/` (which are messages from others):
  a journal entry is a short narrative from this identity to everyone
  else. Terse by convention — a few sentences, not a stream-of-thought
  log; if you need more, write it in a task body or a brief. The folder
  is created lazily on first `coord journal new`.

## Append-only and rename-only

- Producers only ever **create** new files in `<recipient>/inbox/` with
  globally unique names.
- Consumers only ever **rename** files (`mv inbox/X.md archive/X.md`).
- Nobody modifies a file's contents after creation.
- Trim is the only place outright deletion happens, and only on `archive/`.

## Sending

To send identity `bob` a message: write a new file to
`$COORD_ROOT/bob/inbox/<filename>.md` with the frontmatter above. That's
it. The act of writing the file *is* the send. There is no separate outbox
folder.

If sender and recipient are on different machines, the file gets there via
whatever sync mechanism is configured. The sender doesn't know or care.

## Receiving

To receive: list and read files in `$COORD_ROOT/<self>/inbox/`. To mark a
message as processed: `mv` it to `$COORD_ROOT/<self>/archive/`.

## Identity resolution

A coord implementation needs to know "which identity is acting" for any
command that operates on `<self>`. The convention does not auto-detect this
from on-disk state. Instead, identity resolution is one of:

- The `COORD_IDENTITY` environment variable, OR
- An explicit identity argument on the command (e.g. `--from <id>`).

Implementations should error loudly when neither is provided rather than
guessing.

## Archive is the tombstone

This is the one subtle rule that makes plain bidirectional `rsync` converge
correctly across machines:

> If `archive/X.md` exists on this machine, then `inbox/X.md` must not.

Every coord operation runs a **sweep** before doing its work: for each file
in any `<identity>/archive/`, remove the matching file from the same
identity's `inbox/` if it exists. This is idempotent — safe to run
repeatedly, on any machine, in any order.

The sweep restores the invariant after `rsync` has copied an archived
message back into a peer's inbox.

## Status (optional)

An identity may have a `<identity>/status` file containing exactly one of:
`offline`, `available`, `busy`, `away`, `dnd`. Single-line, no
frontmatter. The only writer is the identity's owner. Consumers may
consult it; producers do not have to respect it.

The five settable states represent three distinct presence levels plus
two opt-out signals:

- `available` — present and watching for new traffic.
- `away` — present but not actively engaged (e.g. a tab is hidden, the
  agent is in the middle of a long-running task and isn't reading
  inbox). Different from `busy` — `away` is "not looking right now";
  `busy` is "actively don't ping me." Senders may still deliver to
  `away` recipients; `coord ding`'s SUPPRESS_STATES intentionally
  does NOT suppress `away` arrivals.
- `busy` — focused work in progress; please defer notifications.
  `coord ding` buffers and flushes on the next status flip.
- `dnd` — same suppression behavior as `busy`; semantic difference is
  the writer's intent (do-not-disturb is "stronger" than busy).
- `offline` — gone; deliberate, distinct from missing-file (also
  reported as `offline` but for the absence-of-evidence reason).

When `<identity>/status` is absent, the effective state is `offline`.

`unknown` is a sixth, **derived** state that consumers report when a
status file's mtime is older than ~15 minutes (the
`STATUS_STALE_MS` constant in `src/common.ts`). The owning agent
hasn't refreshed status in a while, so whatever the file says is no
longer trusted. `unknown` is never written to disk and is not
settable by the user — `coord status --set unknown` is rejected.

The MCP server's periodic refresh (`STATUS_REFRESH_MS`, 5 min) keeps
the mtime fresh for the current recorded value while the server runs,
so an idle but alive agent doesn't drift into `unknown`. Refresh +
shutdown semantics apply uniformly to every on-disk state, including
`away`.

The MCP server writes `offline` to its identity's status file on
shutdown (`SIGTERM`, `SIGINT`, or any transport close) so peers see
the right state immediately rather than waiting for the
mtime-staleness fallback.

The `status` file is synced like every other file in the folder. There is
no separate status protocol.

## What sync looks like

Sync moves files between machines. The convention does not mandate any
particular sync tool, but plain bidirectional `rsync` is the floor:

```sh
rsync -a $COORD_ROOT/  peer:$COORD_ROOT/
rsync -a peer:$COORD_ROOT/  $COORD_ROOT/
# then sweep:
for archived in $COORD_ROOT/*/archive/*.md; do
  inbox=$(echo "$archived" | sed 's|/archive/|/inbox/|')
  [ -e "$inbox" ] && rm "$inbox"
done
```

(That's illustrative pseudocode, not the implementation.)

The sweep step is mandatory. Without it, archived messages would be
resurrected from peers' inboxes on every sync.

## What's not in this document

Everything below is for the implementation to decide and iterate on:

- Trim policy (when archive gets cleaned up; tombstone retention horizon).
- CLI surface (`coord message send`, `coord message ls`, etc.). Naming,
  flags, output formats.
- How sync is invoked, what peer specs look like, whether there's an
  `--all` form.
- Identity bootstrap UX (folder creation, env var setup hints).
- Threading, watch, search, and every other read-side feature.
- File formats beyond the frontmatter requirement above.

If something here turns out to be wrong, we change it.
