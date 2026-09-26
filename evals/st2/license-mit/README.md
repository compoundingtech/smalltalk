# license-mit — the smallest real team loop

> **Authority:** This st2 repository copy is authoritative.
> It was copied on 2026-08-26 from `compoundingtech/evals` commit
> `3db48ab56d40ce27dfd94f89d2db9b692d93836a`.
> The remaining `compoundingtech/evals` copy is a read-only fossil.

**What it evaluates.** One request in — *"the `widget` library's license should be MIT"* — and a
coordinated **delegate → execute → verify → confirm** loop out, with isolation held. A supervisor
(`mix.sup`, coordinate-only, owns no repo) delegates to a specialist (`mix.worker`, owns the `widget`
repo); the worker relicenses + commits; the supervisor verifies read-only and confirms back to the
`requester`. It is the smallest end-to-end proof that the team loop works.

**Run it:** `st2 eval ./evals/st2/license-mit/`

`st2 eval` copies the fixture into a fresh catalog, boots the team + a judge agent, delivers `task.md`
to `mix.sup`, runs to the supervisor's confirmation (or `max-timeout`), then runs the judges → verdict.

## The folder

| path | what it is |
| --- | --- |
| `license-mit.kdl` | the whole eval: the `mix` team (sup + worker) + the `eval {}` block (copy, kickoff, judges) |
| `task.md` | the kickoff message delivered to `mix.sup` |
| `fixture/` | the pre-built starting world, copied 1:1: `worker/` (the widget repo, **proprietary** license, base commit + owner-pinned author, git db stored as `worker/_git` → `.git` on copy) and `sup/` (coordinate-only, no repo). Each holds an `st2`-native persona. |
| `judges/` | the held-out bash judges (below) |

## What makes it pass (all judges must pass — the team never sees these)

- **isolation** (`judges/isolation.sh`) — the supervisor's dir is not a git repo (structurally cannot
  commit); the change stays on the license surface. (Commit author is reported but not the gate — the
  repo identity is owner-pinned, so isolation is proven structurally + by the coordination judge.)
- **coordination** (`judges/coordination.sh`) — the full loop is visible on the bus: a `mix.sup → mix.worker`
  delegation, a `mix.worker → mix.sup` report, and a `mix.sup → requester` confirmation that **post-dates**
  the worker's report (a *verified* confirmation, not the initial "on it" ack). Also reports the autonomy
  signal (post-kick rescues).
- **LICENSE is real MIT** (declarative) — `worker/LICENSE` has the MIT permission grant + the AS-IS
  warranty and lacks the proprietary text.
- **package.json declares MIT** (declarative) — the SPDX `license` field is `MIT` (a job that flips
  LICENSE but leaves `package.json` proprietary is still proprietary to tooling).
- **committed + clean** (`judges/committed.sh`) — the LICENSE change is in `base..HEAD` and the worktree
  is clean.
- **confirmation is verified, not a rubber-stamp** (ask-agent) — a cross-family judge reads the
  supervisor's confirmation and passes only if it cites the real commit + the verification run.
