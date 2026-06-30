# smalltalk — LAYOUT

The minimal invariants every smalltalk implementation must respect.
Anything not in this file is open for the implementation to decide
and iterate on.

> Naming note: the project is mid-phase-rename from `coord` to
> `smalltalk`. The `$COORD_ROOT` env var and `~/.local/state/coord/`
> default both remain honored as legacy aliases — see [Agent
> resolution](#agent-resolution) for the dual-honor chain.

## The root

`$ST_ROOT` (default `~/.local/state/smalltalk`; legacy `$COORD_ROOT` /
`~/.local/state/coord` honored) holds one sub-folder per **agent**.

```
$ST_ROOT/
  alice/
    inbox/
    archive/
  bob/
    inbox/
    archive/
```

The folder is 100% syncable — every file under `$ST_ROOT/` participates in
sync. There are no machine-local marker files inside the folder.

## Agent rules

Vocabulary note: brief-009 item 3 renamed the project's primary noun
from **identity** to **agent**. On-disk paths still read
`<root>/<name>/{inbox,archive}` — the folder *name* IS the agent's name
— but the type, env-var, CLI verb, and MCP tool surfaces all prefer
`agent` over `identity` now. The old names remain honored as
deprecated aliases for one release cycle (see Identity resolution
below).

- An **agent** is a sub-folder name under `$ST_ROOT/`. The folder name
  is the agent's id.
- An agent is normally hosted by a single participant — that
  participant's machine is the canonical place where the agent's
  writes originate. A *shared* agent (a "team alias" — `dispatch`,
  `oncall`, etc.) is also supported: multiple participants keep the
  folder synced and share the inbox. See
  [walkthrough.md](notes/walkthrough.md) for usage patterns.
- Lowercase ASCII alphanumeric, hyphens, and periods. Must start and end
  with an alphanumeric. Periods encode hierarchy in the flat namespace
  (e.g. `persona.session-1.child-7`) — see issue #1. No upper bound on
  length beyond what the underlying filesystem accepts.
- Reserved names (must not be used as an agent name): `inbox`,
  `archive`, `resources`, `status`, `name`, `available`, `busy`,
  `away`, `dnd`, `offline`, `unknown`, `members`, `agents`,
  `overview`.
- Every agent sub-folder contains at minimum two folders: `inbox/`
  and `archive/`. One further optional folder, `resources/`, may also
  be present (see below). No other sub-folders are part of the
  convention.
- An agent may *optionally* have a single-line `<agent>/name` file
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

## Attachments

A sender may drop additional files alongside the canonical
`<unix-ms>-<rand6>.md` message file in `<recipient>/inbox/`. Files
sharing that message's prefix are **attachments of that message**, by
prefix association:

```
inbox/
  1719012345-abc123.md              # the message file
  1719012345-abc123.options.json    # structured payload
  1719012345-abc123.schema.json     # optional schema describing the payload
```

Any file shape is fine — JSON, CSV, image, tarball. Attachments sync
like everything else under `$ST_ROOT/` and are inspectable with plain
`ls` / `cat`. Their schema and interpretation are the participants'
concern, not smalltalk's — smalltalk guarantees only that the bytes
arrive and that the shared prefix is preserved.

Lifecycle is opt-in by default. Bare `archive`, `read`, and `trim`
operate on the canonical `.md` file only — tooling that wants
attachments coupled passes `--with-attachments` (see below). Without
that flag, attachments stay where they were written; smalltalk doesn't
move or reclaim them.

- `st message archive <file> --with-attachments` moves every
  prefix-sibling alongside the `.md`. Atomic on conflict: if any
  sibling has a divergent archive twin, the whole operation refuses
  before moving anything.
- `st message archive trim --with-attachments` deletes archive
  prefix-siblings whose `.md` is being trimmed.
- `st message ls --orphans` lists prefix-siblings in the folder
  (inbox by default, archive with `--archive`) whose canonical `.md`
  is no longer present — i.e. files left behind by an earlier bare
  `archive`.
- `sweep` extends the tombstone invariant to prefix-siblings: an
  inbox sibling byte-identical to an archive twin is removed iff a
  matching `archive/X.md` exists. This keeps the family from
  resurrecting on the next `rsync` after `archive --with-attachments`.

This is all opt-in surface — bare `archive` still leaves siblings
behind, matching pre-issue-#8 semantics for callers that prefer to
own attachment lifecycle themselves.

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
file at `$ST_ROOT/bob/inbox/<filename>` is addressed to `bob`. The
timestamp is *not* in the frontmatter either — the filename's `<unix-ms>`
prefix is the canonical send time.

## The folders

- **`inbox/`** holds messages addressed to this identity that have not yet
  been processed.
- **`archive/`** holds messages that have been processed. Move (`mv`) is the
  only operation that puts a file in archive.
- **`resources/`** *(optional)* holds annotated URLs the identity has
  chosen to surface to peers — links to PRs they own, pty sessions
  they supervise, repos, docs, anything URL-shaped. Each file is
  `<unix-ms>-<rand6>.md` (same grammar as messages); frontmatter
  carries `url:` (required) plus optional `title:` / `tags:` /
  `relation:`; body is an optional markdown description. **Only the
  identity owner writes here**; peers read via sync but never write.
  The folder is created lazily on first `st resource add`. URLs
  accept any scheme — the convention is `https://` for the web,
  `pty://<session-name>` for a pty session, and otherwise whatever
  scheme the participants agree on.

  The optional **`relation:`** field describes the agent's
  relationship to the URL. Free-form string; **never inferred**
  (absent by default). The canonical, non-enforced values are
  `owns`, `relates-to`, and `depends-on`; agents may invent their own
  (`considers-blocking`, `mentored-by`, whatever fits). A resource
  with no `relation:` is fully first-class — the bare URL is the
  primary thing the field annotates.

## Append-only and rename-only

- Producers only ever **create** new files in `<recipient>/inbox/` with
  globally unique names.
- Consumers only ever **rename** files (`mv inbox/X.md archive/X.md`).
- Nobody modifies a file's contents after creation.
- Trim is the only place outright deletion happens, and only on `archive/`.

## Sending

To send agent `bob` a message: write a new file to
`$ST_ROOT/bob/inbox/<filename>.md` with the frontmatter above. That's
it. The act of writing the file *is* the send. There is no separate outbox
folder.

If sender and recipient are on different machines, the file gets there via
whatever sync mechanism is configured. The sender doesn't know or care.

## Receiving

To receive: list and read files in `$ST_ROOT/<self>/inbox/`. To mark a
message as processed: `mv` it to `$ST_ROOT/<self>/archive/`.

## Agent resolution

A smalltalk implementation needs to know "which agent is acting" for any
command that operates on `<self>`. The convention does not auto-detect
this from on-disk state. Instead, agent resolution is one of:

- The `ST_AGENT` environment variable (preferred), OR
- The `ST_IDENTITY` env var (deprecated, with a one-time stderr migration notice), OR
- The `COORD_IDENTITY` env var (legacy, with the same notice), OR
- An explicit agent argument on the command (e.g. `--from <agent>`).

Implementations should error loudly when none are provided rather than
guessing. The three-level env-var fallback exists so per-machine
config (e.g. `pty.toml`) can migrate from `COORD_IDENTITY` →
`ST_IDENTITY` → `ST_AGENT` at its own pace; the next major release
drops the legacy honors.

## Archive is the tombstone

This is the one subtle rule that makes plain bidirectional `rsync` converge
correctly across machines:

> If `archive/X.md` exists on this machine, then `inbox/X.md` must not.
>
> The same rule extends to prefix-sibling attachments (issue #8): an
> inbox `X.<ext>` byte-identical to an `archive/X.<ext>` is removed
> by `sweep` iff a matching `archive/X.md` exists, so the family is
> reclaimed together when `archive --with-attachments` is used. Bare
> `archive` doesn't move siblings, so this generalization is a no-op
> for callers who keep attachments out-of-band.

**Sweep is a convergence operation, not transactional.** It restores
the invariant in three places: (1) on-demand via `st sweep`;
(2) lazily on read — when a reader opens an inbox file whose
byte-identical twin exists in archive, the inbox copy is removed and
the archive copy is returned instead (one stat + one byte-compare,
bounded); (3) before AND after every `st sync` push/pull.
Idempotent — safe to run repeatedly, on any machine, in any order.

Operations on `inbox/` and `archive/` **MUST NOT** depend on a recent
sweep for correctness. The invariant is restored as work flows through
the system, not before every read or write. Tooling that ran an inline
presweep before every command (some earlier implementations did) is
fine to do — it's just expensive at scale and not part of the contract.

The sweep-on-sync is the load-bearing one: without it, `rsync` would
resurrect archived messages into peers' inboxes on every push.

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
  `away` recipients; `st ding`'s SUPPRESS_STATES intentionally does
  NOT suppress `away` arrivals.
- `busy` — focused work in progress; please defer notifications.
  `st ding` buffers and flushes on the next status flip.
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
settable by the user — `st status --set unknown` is rejected.

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
rsync -a $ST_ROOT/  peer:$ST_ROOT/
rsync -a peer:$ST_ROOT/  $ST_ROOT/
# then sweep:
for archived in $ST_ROOT/*/archive/*.md; do
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
- CLI surface (`st message send`, `st message ls`, etc.). Naming,
  flags, output formats.
- How sync is invoked, what peer specs look like, whether there's an
  `--all` form.
- Identity bootstrap UX (folder creation, env var setup hints).
- Threading, watch, search, and every other read-side feature.
- File formats beyond the frontmatter requirement above.

If something here turns out to be wrong, we change it.
