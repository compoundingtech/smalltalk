---
date: 2026-05-05
audience: anyone evaluating smalltalk's data-loss properties
purpose: argument that the sync algorithm doesn't silently lose messages, with explicit assumptions and where they break
---

# smalltalk — no-loss argument

> Originally written when the project was named `coord`. The argument
> is unchanged by the rename; CLI examples below use the current
> canonical `st` name with `coord` working identically as an alias.

The claim of this document: **under a small set of assumptions, every message file ever written into a participating smalltalk folder reaches one of three terminal states — still in some machine's inbox, in some machine's archive, or explicitly trimmed — and never silently disappears.**

This isn't a formal proof. It's a careful sketch you can check by hand. It exists so a reviewer can poke at the assumptions and tell us where they think it breaks.

## Assumptions

1. **Globally unique filenames.** Every message file has a name of the form `<unix-ms>-<rand6>.md` where `<unix-ms>` is a 13-digit Unix time in milliseconds and `<rand6>` is six characters from Crockford base32 (`0-9a-z` minus `i,l,o,u`, namespace size 32⁶ ≈ 1.07×10⁹). Two different writers producing the same name in the same millisecond requires both an ms collision and a rand6 collision; the joint probability is vanishingly small at any human-scale write rate.

2. **Writes are atomic.** Every implementation must use a write strategy where readers never see a partial file. The reference bash implementation uses `safe_atomic_write` (an `O_EXCL` write under bash's `noclobber`, which fails if the file already exists), or a `mv` from a sibling temp file to the final path. Both are atomic at the filesystem layer.

3. **Sync is `rsync -a` without `--delete`.** Rsync transfers each file once, computing checksums to verify integrity in transit. Without `--delete`, rsync only copies files in; it never removes files on the destination.

4. **The sweep rule is correctly implemented.** Every smalltalk command, before doing its work, removes any `<id>/inbox/X.md` for which `<id>/archive/X.md` exists *on the same machine*. The sweep operates only on local state; it does not consult or modify other machines' trees.

5. **The only deletion operation is `st archive trim`.** No other command in the CLI deletes message files.

## Claim

For every message file `M` that is ever successfully written by `st send`, at every later moment in time, `M` exists at some path on at least one participating machine — either `<recipient>/inbox/M.md` or `<recipient>/archive/M.md` — until and unless `st archive trim` is explicitly invoked.

## Argument

Consider an arbitrary message `M` written by sender `S` into `<recipient>/inbox/M.md` on machine `S`. We trace what can happen to `M`.

### What rsync can do to M

By assumption 3, rsync only ever copies `M` from a source that has it to a destination that doesn't. It never removes `M` from any tree. Rsync's checksums (assumption 3) ensure the bytes arrive intact.

So the only way for `M` to disappear from a machine's filesystem after rsync runs is by the explicit action of one of coord's own commands.

### What smalltalk commands can do to M

We enumerate every smalltalk command and check which ones can remove `M`:

- `st send` — only ever creates files. Cannot remove `M`.
- `st ls`, `st read`, `st status`, `st watch`, `st thread` — read-only with respect to message files. Cannot remove `M`. (They do trigger the implicit sweep; see below.)
- `st archive` — moves `<recipient>/inbox/M.md` to `<recipient>/archive/M.md` on the local machine. `M` still exists, just at a different path.
- `st sync push` / `st sync pull` / `st sync sweep` — invoke rsync (assumption 3) plus the sweep (assumption 4). Rsync can't remove `M` (above). The sweep can remove `<id>/inbox/M.md` only if `<id>/archive/M.md` exists *on the same machine*. So removal of an inbox copy of `M` requires that an archive copy of `M` also exists locally — `M` is not lost, just relocated.
- `st archive trim` — explicit user-initiated deletion of files in `archive/` matching the trim filter. This is the only command that ever removes a file from the filesystem without leaving a copy elsewhere. (See "Where the assumptions break" below for what this means.)

### Putting it together

Let `t` be any moment in time after `M` was written. At time `t`, on every machine that has ever received a sync containing `M`, the file `M` lives at one of:

- `<recipient>/inbox/M.md`, OR
- `<recipient>/archive/M.md` (placed there by an explicit `st archive`), OR
- nowhere, *only if* `st archive trim` has been explicitly run on that machine after `M` reached the archive.

Because the sweep can only delete an inbox copy when an archive copy exists locally, no two paths can both be empty unless trim has run.

So the system is monotonic with respect to `M`'s existence: once `M` is in archive on machine `K`, the only way to lose it from `K` is an explicit trim. The only way for `M` to vanish from *every* machine is for trim to run on all of them after each one has received `M` via sync. That is a deliberate, user-driven action.

Conclusion: **no silent loss of `M`.**

## Where the assumptions break

The argument is only as strong as its assumptions. Here are the failure modes a reviewer should watch for:

### Filename collisions

Two writers produce the same `<unix-ms>-<rand6>.md` name. With ms-resolution and a 32⁶ rand6 space, a single-machine sender hitting the same ms twice has a roughly 1-in-10⁹ chance of also hitting the same rand6. Multi-writer collision (different machines writing the same millisecond) is bounded by the same per-file probability times the number of concurrent writers.

If a collision occurs, rsync sees two different files claiming the same name. The behavior depends on which side is source and which is destination (rsync `-a` will overwrite the destination if the source's mtime is newer). One copy survives; the other is silently lost.

This is the only way the system loses data without an explicit deletion. To bound the risk:

- Higher-volume use cases should consider a longer rand suffix.
- A future signing layer (see walkthrough.md) would let us detect collisions, since two different writers' signatures would diverge.

### Non-atomic writes

If an implementation writes the file in two steps (open, write some bytes, write some more bytes) without an atomic-rename intermediary, a reader might see a partially-written file. Rsync would then propagate the partial state; if the writer crashes before completing, the partial state is permanent.

The bash reference implementation uses `set -C` (`noclobber`) plus `cat > path` which redirects atomically per-fd, and `mv` from a tempfile for some operations. Both are atomic on POSIX filesystems. A re-implementation in another language must preserve this property.

### Manual filesystem tampering

If a user `rm`s a message file by hand outside `st archive trim`, that file is gone from that machine. If sync from a peer that still has it hasn't run yet, the file may still survive elsewhere. If every machine has been hand-deleted, the file is lost.

This is not a flaw in the algorithm; it's a consequence of the trust model. Cooperating peers don't hand-delete each other's data.

### Clock skew

Filenames embed the writer's local clock. If a writer's clock is very wrong (years ahead or behind), filenames will sort wrong and the `<since UNIX_MS>` filter on `st watch` and `st ls` will behave unintuitively. The `<rand6>` suffix still makes filenames unique, so no data is lost — only ordering is off.

A wildly-wrong clock that produces filenames in the future would cause readers using `--since-now` (which compares against the local now) to skip those messages until the local clock catches up to the writer's bad value. This is observable but not data-losing.

### Trim convergence (the deferred bug)

Trim is local-only. If alice trims `M` and bob hasn't, bob's next sync to alice resurrects `M` on alice's side. The system tends to *over-preserve* under trim disagreement, not under-preserve. Walkthrough §"Trim coordination" describes the operational workaround.

### `--delete` accidentally enabled

If an implementer or operator enables `rsync --delete` (or its equivalent in another sync transport), all bets are off. `--delete` propagates "I don't have this file" as "delete it from the destination." A machine that hasn't yet received `M` would, on its first sync as a source, propagate the absence and erase `M` from peers.

The reference implementation does not pass `--delete`. Any documentation suggesting it should be flagged loudly.

## What this proof does not cover

- **Order-of-arrival.** We do not claim messages are seen in any particular order on any particular machine. Sync is asynchronous; you may see B's reply to A before A. Threading via `in-reply-to` lets readers reconstruct the order regardless.
- **Liveness.** We do not claim messages are delivered within any time bound. Sync runs on whatever cadence the operator configures. A peer that's offline for a year still gets the messages when it comes back.
- **Privacy.** Anyone with read access to any participating machine's `$ST_ROOT` can read every message there. Encryption is out of scope for v0.
- **Authenticity.** A peer can write `from: alice` without being alice. Out of scope for v0; signing is the intended layer.

## Asks of a reviewer

If you can:

1. Find a sequence of operations under the assumptions above that loses a message.
2. Find an unstated assumption the argument relies on.
3. Find a real-world failure mode (network partition, partial sync, kernel crash mid-rsync, NFS, …) that violates one of the assumptions in a way we should care about.

…we'd want to know. The folder convention is small enough that adversarial review is cheap; please push on it.
