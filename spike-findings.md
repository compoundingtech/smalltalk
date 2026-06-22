# Spike: minimize FS churn (issue #2)

**Branch:** `spike/sweep-perf` · **Status:** spike complete, no `src/` changes made.
**Author:** spike-claude · **Scope:** measure + design, do *not* implement.

## TL;DR

- On a **warm page cache, local SSD**, against a realistic root (200 identities
  × 5 000 archive entries = ~1 M message files), a no-op `coord message ls`
  takes **7.4 s** end-to-end. Node startup is 0.1 s of that. **The other ~7.3 s
  is the mandatory per-command `sweep()`.** `thread` takes **68 s**, `overview`
  takes **88 s**. On a network FS these numbers get multiplied by the per-op
  RTT and become minutes-to-unusable.
- The single highest-leverage fix is a **~10-line rewrite of `sweep()` to
  iterate the *inbox* instead of the *archive*.** The tombstone invariant only
  ever *removes inbox files*, so enumerating the (huge, historical) archive is
  pure waste. Probe result: **4.6 ms vs 7035 ms — 1516× — with the identical
  twin set, no new files, no LAYOUT change.** This alone makes concern #1 a
  non-issue.
- `thread` and `overview` (concern #2) read/stat the **entire archive history**
  for their own reasons and are *not* fixed by sweep changes. They need their
  own bounded-read fixes.
- GC (concern #3): LAYOUT explicitly punts retention to the implementation, and
  `coord archive trim` **already exists**. The real gap is that `sync` runs
  `rsync -a` **without `--delete`**, so trims never propagate — and adding
  `--delete` naively would **resurrect archived messages** (see Reliability).

### Recommended ship order

1. **Inbox-driven `sweep()`** — kills the per-command tax (1516×). Pure code
   change, correctness-identical, no spec change. Ship this first, alone.
2. **Drop `sweep()` from write-only paths** (`send`) and scope read-path sweep
   to the touched identity. Second layer; matters mostly on network FS.
3. **Bounded reads for `overview` / `thread`** — stop reading the archive in
   recent-activity; cache thread frontmatter. This is the concern-#2 work.
4. **GC**: keep `archive trim` opt-in; design a *paired inbox+archive*,
   tombstone-preserving trim before ever touching `rsync --delete`.

---

## Method

- **Synthetic root** built by `spike-tools/gen-root.mjs` (deterministic, no
  `Date.now`/random): 200 identities, 5 000 archive entries each (~1 M files),
  5 identities with 10 live inbox items, 5 un-swept inbox/archive *twins* per
  identity (so `sweep` exercises its byte-compare + unlink path), and a
  depth-20 cross-identity reply chain for `thread`. Generation: ~1 001 070 files
  in 65 s.
- **Timing** by `spike-tools/bench.mjs`, which imports the typed cores directly
  (`sweep`, `cmdLs`, `cmdSend`, `cmdThread`, `cmdOverview`) so the numbers are
  FS work, not per-invocation Node startup. One warm-up call + N measured
  iterations; min/median reported.
- **End-to-end** numbers use the real CLI (`node --experimental-strip-types
  src/cli.ts …`) so they include Node startup + the dispatcher's `runPreSweep`.

### Caveats (be honest about these)

- **Warm cache only.** This box has no passwordless `sudo`, so I could not
  `purge` the page cache for a true cold-cache run. **Cold-cache and
  network-FS numbers are therefore *worse* than everything below** — these are
  the optimistic floor. The load-bearing quantity is the **operation count**
  (readdir / stat / read), which I report alongside times; multiply by your
  FS's per-op latency to project. Warm local ≈ **6 µs/stat**; an S3-FUSE/9P
  mount is commonly **0.5–5 ms/stat** — i.e. ~100–800× these times.
- Single machine (M-class macOS, APFS SSD). Absolute ms will differ on other
  hardware; the **ratios** and **op counts** are the portable result.
- After the first `sweep` iteration the 5 twins/identity are gone, so the
  steady-state `sweep()` median reflects the pure stat-storm (no compares) —
  which is the dominant cost anyway.

---

## Baseline numbers

Warm cache, local SSD, root = 200 × 5 000 (~1 M files). Cores called directly
(no Node startup) unless noted.

| op | median | what it intrinsically does |
|---|---:|---|
| `sweep()` alone | **7035 ms** | `readdir(root)` + per-id `readdir(archive)` + **1 000 020 `existsSync`** |
| `ls` own inbox (no sweep) | **0.1 ms** | one `readdir(inbox)` + filename filter; **scale-independent** |
| `ls --json` (no sweep) | 0.2 ms | + `readFile` per matched inbox file (small) |
| `send` (no sweep) | 0.2 ms | `assertFolder` (2 stat) + 1 `O_EXCL` write; **O(1)** |
| `thread` depth-20 (no sweep) | **68 712 ms** | re-reads the **whole tree** once per chain node |
| `overview` (no sweep) | **88 480 ms** | `members --enrich` (whole-tree stat) + reads **every** message |
| `ls` + presweep (**real core cost**) | **7304 ms** | sweep dominates a 0.1 ms command |
| `coord message ls` (**real, subprocess**) | **7.4 s** | Node startup 0.1 s + sweep 7.3 s + ls 0.1 ms |
| `coord --help` (Node floor, sweep skipped) | 0.10 s | process spawn only |

**Sweep scales linearly with total archive count** (it's one `existsSync` per
archive entry across the whole root):

| total archive files | `sweep()` median |
|---:|---:|
| ~500 (10 × 50) | 2.3 ms |
| ~200 k (200 × 1 000) | 936 ms |
| ~1 M (200 × 5 000) | 7035 ms |

≈ **6 µs per archive entry, warm.** The formula is
`sweep_cost ≈ (total archive files) × per-stat-latency`. That per-stat term is
what explodes on a network FS.

### The key shape of the problem

The intrinsic per-command work is **sub-millisecond** (`ls`, `send`). Everything
slow is **proportional to total history, not active work**:

- `sweep()` — 1 stat per *archived* message, every command.
- `overview` / `members --enrich` — 1 stat + 1 read per *archived* message.
- `thread` — the whole tree re-read once per node in the chain.

That is the thing to fix: decouple per-command cost from accumulated history.

---

## Cost model, grounded in `src/`

**Every command pays sweep.** `cli.ts` `runPreSweep()` calls `sweep(root)`
before dispatch (skipped only for `help`); the embedded API does the same via
`lib.ts` `presweep()` before nearly every method — so **the MCP server pays it
on every tool call too**.

`common.ts` `sweep()`:

```
readdir(root)                                  // 1
for id in root:
  readdir(id/archive)                          // I
  for name in id/archive:                      // I × A
    existsSync(id/inbox/name)                   // ← I×A stats  (the storm)
    if present: read both, compare, maybe rm    // bounded: only un-swept twins
```

At 200 × 5 000 that's **1 readdir + 200 readdir + 1 000 000 `existsSync`** per
command. The byte-compare is bounded (fires only on the inbox∩archive
intersection), so **the cost is the stat storm**, independent of how many twins
actually exist.

`overview.ts` `computeRecentActivity()` walks every identity's `inbox` **and**
`archive`, and for **every** file does `statSync` + `readFile` +
`parseFrontmatter` — only to keep the top-10 by mtime. `members --enrich`
(`computeLastActivity`) stats every inbox/archive entry across the root. So
`overview` reads ~1 M files to display ~10 rows.

`thread.ts` is worst: `findChildrenOf()` does `readdir(root)` + per-id
`readdir(inbox)`/`readdir(archive)` + `readFile`+`parseFrontmatter` of **every
message in the tree**, and `collectDescendants()` calls it **once per node** in
the chain. `locateAnywhere()` re-walks the whole root per filename lookup. Net:
**O(chain-depth × total messages)** reads — ~20 M reads here.

---

## Finding A — inbox-driven sweep (ship this first)

`sweep()`'s entire job is to enforce: *if `archive/X` exists, `inbox/X` must
not (when identical)*. The only mutation it ever makes is **removing an inbox
file**. An archive entry with no inbox twin requires no action — so enumerating
the archive is wasted work. The intersection `inbox ∩ archive` is **always a
subset of `inbox`**, which is small (active work) while `archive` is huge
(history). **Iterate the inbox:**

```
for id in readdir(root):
  for name in readdir(id/inbox):     // small: active items only
    if existsSync(id/archive/name) and identical(inbox, archive):
      rm(id/inbox/name)
```

Cost drops from `O(total archive)` to `O(total inbox)`. Probe
(`spike-tools/sweep-fast.mjs`, compare-only so it's repeatable):

| variant | full root (1 M) | fresh root (400 k, twins intact) |
|---|---:|---:|
| current (`existsSync` per archive entry) | 7035 ms · 1 000 020 stats | 2441 ms · 400 020 stats · 1000 twins |
| readdir(archive)+set-intersect | 3334 ms · 0 stats | 1033 ms · 0 stats · 1000 twins |
| **inbox-driven (iterate inbox)** | **4.6 ms · 56 stats** | **39.6 ms · 1050 stats · 1000 twins** |

**1516× at 1 M files; all three find the identical twin set** (correctness
equivalence demonstrated on the fresh root: 1000 twins each). Note the
middle row — even the set-intersect variant still pays 3.3 s just to `readdir`
1 M archive entries; **only the inbox-driven form avoids touching the archive
at all.**

- **Risk: none material.** Same byte-equality guard before any unlink → no new
  message-loss or double-archive risk. It enumerates a strict subset of what it
  must consider.
- **No LAYOUT change.** "Every operation runs a sweep" still holds — the sweep
  is just fast. No `.index` file, no migration, no cross-machine coordination.
- This makes the per-command sweep tax **negligible** (4.6 ms at 1 M), which is
  why it's the first and possibly *only* thing concern #1 needs.

---

## Finding B — touched-identity sweep (network-FS refinement)

Even inbox-driven, sweep does `I` `readdir(inbox)` over the *whole* root per
command — 200 readdirs × (0.5–5 ms network RTT) ≈ 0.1–1 s/command for nothing.
So scope sweep to what the command actually touches:

| command | reads/writes | needs to sweep |
|---|---|---|
| `send <to>` | writes a brand-new uniquely-named file to `<to>/inbox` | **nothing** — a fresh filename can't have a pre-existing twin. Sweep here is pure waste. |
| `ls [<id>]` | reads `<id>/inbox` (default self) | **just `<id>`** |
| `read <id> <fn>` | reads one file | **just `<id>`** (or just that file's twin) |
| `archive <id> <fn>` | moves inbox→archive itself | nothing extra (it *is* the writer) |
| `thread` / `overview` / `members` | cross-tree reads | ideally **none** — read paths shouldn't depend on a global sweep (below) |

**Why a per-command sweep exists at all:** plain `rsync` pulls a peer's
`inbox/X` *and* `archive/X` (tombstone) onto this machine; the local inbox copy
must then be reaped (LAYOUT "Archive is the tombstone"). That makes sweep
fundamentally a **post-sync reconciliation** step — it belongs in the sync path
(where `sync.ts` already calls it), not before every read and write.

**Implementation outline:** give `sweep()` an optional identity list;
`runPreSweep`/`presweep` pass `[]` for `send`, `[target]` for `ls`/`read`, and
the sync path keeps the full sweep. For self-inbox reads (`ls`, `overview` of
self), prefer a **lazy per-entry check**: when listing an inbox, `existsSync` the
archive twin of each *returned* entry (O(inbox size)) instead of relying on a
prior global sweep.

**Risk analysis — what breaks if we miss a sweep:**

- **No message is ever lost.** The archive copy is the durable tombstone; the
  inbox copy is the redundant one. Worst case of a skipped sweep is a
  *tombstoned message lingering as "unread"* until the next sync-sweep — a
  **visibility** bug, not data loss, and not a LAYOUT-tombstone violation
  (archive stays authoritative).
- The dangerous direction — removing an inbox copy that *isn't* truly archived —
  is still guarded by byte-equality, which neither scoping nor inbox-driven
  iteration changes. So no new loss risk in either fix.
- **One spec wrinkle to flag for myobie:** LAYOUT says "*Every coord operation
  runs a sweep before doing its work… The sweep step is mandatory.*" Dropping
  sweep from `send` and scoping reads is arguably a **wording change** to that
  sentence (intent — convergence — is preserved by sweeping in sync + lazy
  per-read checks). Finding A needs no such change; Finding B does. **Decision
  for myobie before B ships.**

---

## Finding C — archive-index sketch

**Verdict: not needed for sweep** once Finding A lands (inbox-driven sweep
never enumerates the archive, so there's nothing for an index to skip). An index
is only interesting for the **read-heavy views** that legitimately need archive
contents.

Had we kept archive-driven sweep, the minimal index would be a per-identity
**`.swept` generation marker**: skip an identity's archive entirely when both
`inbox/` and `archive/` dir-mtimes are ≤ the marker's mtime (turning sweep into
`O(identities)` dir-stats). Invalidation = any write bumps a dir mtime;
missing/corrupt marker = treat as dirty and full-sweep (safe default). The
hazard is **dir-mtime reliability on network FSes** (S3-FUSE may not update it),
which would silently skip a needed sweep → stale-unread. Inbox-driven sweep
sidesteps this entirely, which is another reason to prefer A over any index.

Where an index *does* earn its keep: a small append-only **`archive/.index`**
(one line per archived file: `filename  from  subject  in-reply-to`,
recipient-written at archive time) would let `thread` and `overview` answer
"who replied to X" / "recent activity" **without opening every message**. Format
= newline-delimited TSV; writer = the `archive` command (it already has the
frontmatter in hand); invalidated/rebuilt lazily if line-count ≠ `readdir`
count; missing/corrupt = fall back to the current full scan. That's the right
shape for concern #2 — but it's a bigger change than Finding A and should follow
the cheap wins.

---

## Finding D — read-heavy views (the concern-#2 work)

These are slow **independent of sweep** and need their own fixes:

- **`overview`**: `computeRecentActivity` reads every message in every
  inbox+archive to surface 10 recent items. Fixes, cheapest first:
  (1) **use `mtime` only** — it already sorts by `statSync` mtime, so it does
  not need to `readFile`/`parseFrontmatter` every file up front; read
  frontmatter for the **top-N after sorting** (drops ~1 M reads to ~10).
  (2) Skip `archive/` in recent-activity, or bound it to "archived in the last
  N days" via the filename timestamp (no stat needed — the ts is in the name).
  (3) `members --enrich` `lastActivity` could read a single per-identity
  `lastActivity` sidecar instead of stat-ing the whole tree.
- **`thread`**: replace the repeated whole-tree scans. (1) Build the
  child-index **once** per invocation, not once per node (memoize
  `findChildrenOf`'s full pass). (2) Better: back it with the `archive/.index`
  above so the reply graph is read from N small index files, not N×M messages.
  (3) `locateAnywhere` should cache its `readdir(root)` for the duration of a
  call.

Filenames already embed a sortable `<unix-ms>` prefix — **lean on it** to bound
scans by time without statting.

---

## Finding E — GC framing

**It's a coord concern, and LAYOUT already says so:** "Trim policy (when archive
gets cleaned up; tombstone retention horizon)" is listed under *"What's not in
this document… for the implementation to decide."* So coord should own the
mechanism; the *policy* (how long to keep) is the deployer's knob.

**Mechanism already exists:** `coord archive trim --older-than DURATION |
--keep-last N [--dry-run]` is implemented (`src/commands/archive.ts`). The gaps:

1. It's **manual and per-identity** — there's no scheduled/age-based auto-run
   and no fleet-wide story.
2. **`sync` uses `rsync -a` without `--delete`** (`src/commands/sync.ts`), so a
   trim on machine A never propagates: B keeps the files, and a later pull
   B→A **re-introduces** what A trimmed. Net: archives are effectively
   **monotonic across the fleet** regardless of local trims.

**Smallest viable shape:** keep `archive trim` opt-in; add an **age-based
retention config** (`retain_archive_days`) that a deployer can set and a
periodic `coord sync`/cron applies via `archive trim --older-than`. Defer
`rsync --delete` until the resurrection hazard below is solved — do **not** flip
it on as a convenience.

---

## Reliability constraints (loud)

1. **`rsync --delete` for GC can resurrect already-read messages.** If A trims
   `archive/X` (tombstone gone) and B still holds `inbox/X` (or `archive/X`), a
   `--delete` sync can remove the tombstone on one side while an inbox copy
   survives on the other — and the next view/sweep promotes that inbox copy back
   to a **live unread message**. **Never `--delete` across the inbox/archive
   boundary, and only trim `inbox` + `archive` as a matched pair.** Any GC that
   removes a tombstone must guarantee no inbox twin exists anywhere in the
   fleet — which plain rsync cannot guarantee. This is the single biggest
   correctness trap in concern #3.
2. **Skipping/scoping sweep never loses a message** (archive is the durable
   tombstone). Worst case is a stale "unread" until the next sync-sweep — a
   visibility bug. Acceptable, but it's why the **full sweep must stay in the
   sync path** even after Findings A/B.
3. **Inbox-driven and set-intersect sweep are correctness-identical** to the
   current sweep (same byte-equality guard; same twin set found in the probe).
   No new loss/double-archive risk.
4. **LAYOUT-tombstone wording.** Finding A preserves "every operation runs a
   sweep" verbatim. Finding B (drop sweep from `send`, scope reads) changes that
   sentence's literal meaning and should be confirmed with myobie before
   shipping — convergence is preserved (sync sweeps; reads check lazily), but
   the spec text is binding.
5. **`overview` mtime fix** must keep reading frontmatter for the items it
   actually displays — don't drop subject/from from the top-N, only from the
   discarded tail.

---

## Reproduce

```sh
# build a synthetic root (knobs via env; deterministic)
IDENTITIES=200 ARCHIVE=5000 INBOX_IDS=5 INBOX_EACH=10 TWINS=5 CHAIN=20 \
  OUT=/tmp/coord-spike-full node spike-tools/gen-root.mjs   # prints thread seed

# warm baseline of the four hot commands
ROOT=/tmp/coord-spike-full SEED=<seed.md> IDENTITY=id-0002 ITERS=5 \
  node --experimental-strip-types spike-tools/bench.mjs

# prove the inbox-driven sweep (compare-only; 3-way)
ROOT=/tmp/coord-spike-full node spike-tools/sweep-fast.mjs
```

Probe scripts live in `spike-tools/` and do **not** import or modify `src/`
beyond read-only core calls in `bench.mjs`.
