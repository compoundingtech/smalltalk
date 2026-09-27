# License MIT — the smallest team loop

This st3 eval runs the license MIT task with two native Claude Sonnet agents.

- **Task** (`task.md`): one immutable instruction referenced by the graph-owned delegation step.
- **Team/persona mechanism**: the fixture pre-seeds `CLAUDE.md` and `PERSONA.md` in both workspaces.
  The KDL uses native `harness "claude" {}` blocks. `lmc.sup` coordinates and owns no repo;
  `lmc.worker` owns the `widget` repo and makes/commits the change; `lmc.sup` verifies read-only and
  confirms. The KDL records delegation, implementation, verification, and products as nested missions.
  Every eval starts from the frozen `worker/_git` snapshot, rehydrated as `.git` only inside
  the throwaway catalog.
- **Gates** (all held-out): structural isolation (sup owns no repo), the coordination loop on the bus
  (delegate → report → verified-confirm post-dating the report), `LICENSE` is canonical MIT, `package.json`
  declares MIT, the change is committed with a clean worktree, and a Codex gate that the confirmation
  cites real evidence (not a bare "done!").

Validate its graph contract with `cargo test -p st3 --test examples`; paid live orchestration uses
the repository's internal eval controller, not the public CLI.

The `receipts/` directory contains the attempts and the passing proof.
