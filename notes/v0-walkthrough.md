---
date: 2026-05-05
audience: human reviewer (myobie)
purpose: walk through the v0 bash CLI as a play — what commands exist, what files are written where, what each step looks like from the DX side
---

# smalltalk v0 — a walkthrough in scenes

> **Historical note.** This doc captures the v0 bash CLI shape (the
> project was named `coord` at the time). Three things have changed
> since:
>
> 1. **brief-017:** message verbs are now nested under `coord message
>    <verb>` (alias `coord msg <verb>`). Translation: `coord send` →
>    `coord message send`, `coord ls` → `coord message ls`, etc.
> 2. **brief-009 item 3:** the project was renamed `coord` →
>    `smalltalk` (long) / `st` (canonical short). All three CLI names
>    install side-by-side and behave identically; the examples below
>    still using `coord …` work unchanged.
> 3. **brief-009 items 1/2:** the `tasks/` and `journal/` folders + CLI
>    verbs were removed in the slim-down. References to `coord task` /
>    `coord tasks` / `coord journal` below are historical.
>
> See [walkthrough.md](walkthrough.md) for the current play.

This is what happens when two agents talk through coord. Every command shown was run for real against `/tmp/coord-dx3/` during DX verification of brief-003. Every file path is exactly what landed on disk.

## Cast

- **alice** — an agent hosted on machine A
- **bob** — an agent hosted on machine B
- **coord** — the CLI
- **rsync** — the messenger; runs periodically, doesn't think

## Setting

Two machines, each with `$COORD_ROOT` (default: `~/.local/state/coord`). The folders are conceptually one shared workspace, but each machine has its own copy. `rsync` is the only thing that crosses the wire.

```
$COORD_ROOT/
  <identity>/         # one folder per addressable participant
    .machine-id        # which machine hosts this identity
    inbox/             # messages addressed to <identity>, not yet processed
    archive/           # messages <identity> has processed
    .status            # optional: available | busy | dnd | unset
```

LAYOUT.md is the binding spec. It says: append-only writes, rename-only consumer ops, archive presence is the tombstone.

## The command surface

```
coord init <identity>                                 # bootstrap a new identity on this machine
coord send <to> [--from ID] [--subject S]             # write a message (body from stdin)
            [--in-reply-to F] [--tags T,T] [--priority low|normal|high]
coord ls [<identity>] [--archive] [--count] [--since UNIX_MS] [--from ID]
coord read <identity> <filename> [--raw] [--archive]
coord archive <identity> <filename>
coord archive trim [<identity>] [--older-than DURATION] [--keep-last N] [--dry-run]
coord thread <identity> <filename>
coord watch <identity> [--with-subject] [--since UNIX_MS] [--interval MS] [--once]
coord status [<identity>] [<state>]                   # state ∈ {available, busy, dnd}
coord sync push <peer>
coord sync pull <peer>
coord sync sweep                                      # run the archive-as-tombstone sweep manually
coord sync --all
```

`<peer>` is one of:
- `local:<path>` (used in tests / single-host setups)
- `host[:path]` (ssh target)
- a name from `$COORD_CONFIG/peers.yaml`

Every command except `init` runs an implicit pre-command **sweep** first — if `archive/X.md` exists locally, any matching `inbox/X.md` is removed before the command does its work. That's what keeps reads consistent in the face of asymmetric sync.

---

## Act I — Cold start

*Two fresh machines, no coord state on either.*

**alice (on A):**
```
$ coord init alice
initialized: alice
```

**Filesystem on A after Act I:**
```
~/.local/state/coord/
├── .machine-id            "machine-a"
└── alice/
    ├── .machine-id        "machine-a"   ← this is the codex P1 marker that
    ├── archive/                            distinguishes a hosted identity
    └── inbox/                              from a synced peer copy
```

**bob (on B):** same thing, `coord init bob`. Both machines are now ready.

**Why it matters:** the per-identity `.machine-id` file is small but load-bearing — it's how `coord send --from <X>` knows whether `<X>` is actually hosted here vs. an artifact synced from someone else. Without it, a typo in `--from` would invent a new identity. With it, typos get rejected loudly.

---

## Act II — alice sends bob a message

**alice (on A):**
```
$ echo "hey, can you take a look at this?" | coord send bob --from alice --subject "review request"
1777971233168-machine-a-62qzgz.md
```

The single line of stdout is the filename, ready to pipe into other commands.

**What just happened on A's disk:**
```
~/.local/state/coord/
├── alice/                              (unchanged — alice's identity still empty)
└── bob/                                ← created on the fly. alice has a local
    └── inbox/                            view of bob's folder, even though
        └── 1777971233168-machine-a-       bob is hosted on B.
            62qzgz.md
```

The file's contents:
```markdown
---
from: alice
to: bob
ts: 2026-05-05T10:13:53.168Z
subject: "review request"
---
hey, can you take a look at this?
```

**Filesystem on B:** still empty inbox. Nothing has crossed the wire yet.

**Key insight:** alice writing to `bob/inbox/` *is* the send. There is no outbox folder, no staging, no separate "queue this for delivery" step. The action of writing to the recipient's inbox folder is the protocol.

---

## Act III — sync runs

A cron on alice's machine fires `coord sync push local:/tmp/coord-dx3/b` (or in production, an ssh target). What happens:

1. Pre-command sweep on A: walks every `<id>/archive/X.md`, ensures no matching `<id>/inbox/X.md`. Currently no archives, so this is a no-op.
2. `rsync -a $COORD_ROOT/ <peer>/` copies the whole tree to B.
3. Post-command sweep on A: same as pre. Still a no-op.

**Filesystem on B after Act III:**
```
~/.local/state/coord/
├── .machine-id            "machine-b"
├── alice/                 ← brand new on B; synced from A
│   ├── archive/
│   └── inbox/             (alice has an empty inbox; that's fine)
│   (no .machine-id under alice/  — that file is excluded from sync)
└── bob/
    ├── .machine-id        "machine-b"   ← bob's hosted-here marker, untouched
    ├── archive/
    └── inbox/
        └── 1777971233168-machine-a-62qzgz.md   ← arrived
```

The `.machine-id` exclusion is the key trick: `alice/` exists on B as a synced copy, but B doesn't lie and claim to host alice — there's no `alice/.machine-id` on B.

---

## Act IV — bob is busy, alice notices

**bob (on B), before reading messages:**
```
$ coord status bob busy
status: busy
```

**B's disk:**
```
bob/.status              "busy"
```

Next sync (in either direction) propagates the file. Now A also has `bob/.status = busy`. Alice can:

```
$ coord status bob
busy
```

(She's reading B's status from her synced copy. It's "informational, not contractual" per IDEA.md — alice is free to send anyway.)

She decides to wait. She does nothing.

---

## Act V — bob reads, replies, archives

**bob (on B):**
```
$ coord ls bob
# 1 message in inbox
1777971233168-machine-a-62qzgz.md
```

(Note: pluralization is correct — "1 message" not "1 messages". And the implicit pre-command sweep ran, so even if A had pushed a stale copy of the file, the count would be correct.)

```
$ coord read bob 1777971233168-machine-a-62qzgz.md
# inbox/1777971233168-machine-a-62qzgz.md
from:    alice
to:      bob
ts:      2026-05-05T10:13:53.168Z
subject: review request

hey, can you take a look at this?
```

bob writes a reply. Note the `--in-reply-to` for threading:

```
$ echo "yes, looking now" | coord send alice --from bob \
      --in-reply-to 1777971233168-machine-a-62qzgz.md \
      --subject "re: review request"
1777971300094-machine-b-h3kk7v.md
```

**B's disk:**
```
alice/
└── inbox/
    └── 1777971300094-machine-b-h3kk7v.md   ← bob's reply, in alice's inbox
                                              (still on B's disk; sync hasn't
                                              run yet)
```

bob then archives alice's original:

```
$ coord archive bob 1777971233168-machine-a-62qzgz.md
archived
```

**B's disk after archive:**
```
bob/
├── archive/
│   └── 1777971233168-machine-a-62qzgz.md   ← moved
└── inbox/                                  ← empty
    (alice's original is gone from inbox)
```

bob clears his status:
```
$ coord status bob available
status: available
```

---

## Act VI — sync brings everyone level

Now sync runs (let's say B pushes; in practice both machines would push and pull on their own schedules):

**Step by step inside `coord sync push local:A`:**

1. **Pre-sweep on B**: B has `archive/1777971233168-...md` and no inbox copy. No-op.
2. **`rsync -a B/ A/`**: copies B's tree to A. After this:
   - A receives `bob/archive/1777971233168-...md` (new on A)
   - A receives `alice/inbox/1777971300094-...md` (bob's reply)
   - A receives `bob/.status = available`
3. **Post-sweep on A** (because the sweep is universal, every coord command runs it): A walks every `archive/X.md` and removes the matching `inbox/X.md`. **A's `bob/inbox/1777971233168-...md` (alice's old copy of what she sent) gets removed** because `bob/archive/1777971233168-...md` now exists on A.

**A's disk after Act VI:**
```
alice/
├── archive/                                (still empty)
└── inbox/
    └── 1777971300094-machine-b-h3kk7v.md   ← bob's reply arrives
bob/
├── .status                                 "available"
├── archive/
│   └── 1777971233168-machine-a-62qzgz.md   ← alice's original, now archived
└── inbox/                                  ← empty (Z1 sweep cleaned this)
```

**This is the load-bearing trick.** Without the sweep, A would still have `bob/inbox/1777971233168-...md` (alice's copy of what she sent). On the *next* sync round, A's inbox copy would push back to B, recreating the file there. Bob would see it again, archive it again, sync, recreate, archive — an infinite tug-of-war.

The sweep breaks the loop: **on every coord command, every machine reconciles "if there's an archive copy, the inbox copy must go."** Idempotent; safe to run anywhere; converges in one round.

---

## Act VII — alice sees the reply, walks the thread

**alice (on A):**
```
$ coord ls alice
# 1 message in inbox
1777971300094-machine-b-h3kk7v.md
```

```
$ coord read alice 1777971300094-machine-b-h3kk7v.md
# inbox/1777971300094-machine-b-h3kk7v.md
from:        bob
to:          alice
ts:          2026-05-05T10:15:00.094Z
subject:     re: review request
in-reply-to: 1777971233168-machine-a-62qzgz.md

yes, looking now
```

```
$ coord thread alice 1777971300094-machine-b-h3kk7v.md
1777971233168-machine-a-62qzgz.md  alice  review request
  1777971300094-machine-b-h3kk7v.md  bob  re: review request
```

The thread reaches alice's *original message* even though alice never had a separate "sent" log — because after sync, alice's machine has `bob/archive/1777971233168-...md` (the message she sent, which bob has now processed). The thread walker scans every `<id>/{inbox,archive}/` under `$COORD_ROOT`, so it finds the chain across identity sub-folders.

**Alice's "what have I sent recently?" view** is naturally `coord ls bob --archive` (which lists files in bob's archive on alice's machine = messages alice sent that bob has processed) plus `coord ls bob` (messages alice sent that bob hasn't yet processed). No separate "sent" folder is needed.

---

## Act VIII — alice archives the conversation

```
$ coord archive alice 1777971300094-machine-b-h3kk7v.md
archived
```

A's disk:
```
alice/
├── archive/
│   └── 1777971300094-machine-b-h3kk7v.md
└── inbox/                                 ← empty
```

After the next sync, B's tree will mirror this — alice's archive of bob's reply propagates back to B, and the universal sweep on B keeps things consistent (no inbox copy survives there either).

---

## Act IX — trim

Eventually alice wants to clean up. She does it carefully:

```
$ coord archive trim alice --older-than 30d --dry-run
1577836800010-machine-a-old1aa.md
1577836800020-machine-a-old2aa.md
1577836800030-machine-a-old3aa.md
# would trim 3 files (dry run; nothing deleted)
```

She likes the list. She runs for real:

```
$ coord archive trim alice --older-than 30d
1577836800010-machine-a-old1aa.md
1577836800020-machine-a-old2aa.md
1577836800030-machine-a-old3aa.md
# trimmed 3 files
```

Caveat documented in `lib/cmd_archive.sh`: `archive trim` doesn't yet converge across machines under plain `rsync -a`. If A trims X but B hasn't run trim yet, on the next sync B's archived X will resurrect on A. Real fix needs LAYOUT.md changes (a tombstone protocol or scoped `--delete` for archive). Out of scope until we hit it for real.

---

## End of play

**The folder is the API.** Every artifact is plain text on disk, browsable with `cat` / `ls` / `find`. Sync is rsync. The only smart thing is the sweep — and it's one rule, ten lines of bash.

**What I think is good:**
- The single-folder mental model survives. `<id>/inbox/` is "messages for me," `<id>/archive/` is "messages I've handled," and they're the same on every machine.
- alice's "sent" view is just her local copy of bob's folder. No separate concept. Free.
- Threading walks across identity trees because the data is there to walk.
- Init, send, ls, read, archive, sync — six verbs to do everything.

**What I think is wrong or rough:**
- Reading `--archive` to see "what I sent" is non-obvious. Should be a `coord ls --sent` shortcut.
- `coord status <id>` returning `unset` when there's no `.status` file is fine for now, but I'm not sure agents will know what to do with it. Maybe `--default available` would be friendlier.
- `coord watch` defaults to "since now" which means starting a watcher misses files that were already in the inbox. `--once` defaults to the same. The tests work around this with `--since 0`. Real users will trip on it.
- Filenames are 36+ characters because `<machine-id>` defaults to a UUID. Pinned ids like `machine-a` work but `coord init` doesn't help you set them — you have to `echo machine-a > .machine-id` by hand.
- `archive trim` cross-machine convergence (the deferred bug). Open question whether to solve in v0 or document and move on.
- `peers.yaml` aliases work but there's no `coord peer add <name> <spec>` — you edit a YAML file.

These are the kinds of things I want your eyes on. When you have time, walk through this doc, mark up what bothers you, and we'll bundle a brief-004.
