# ghost-bug-codex — the ghost-bug debug loop, run by codex seats

This st3 eval asks two native Codex agents to find a shared-default mutation bug in `labelkit`.
This teaches a bounded delegate-debug-verify loop where a shallow patch or a test that never failed
cannot pass.

The KDL records delegation, diagnosis, regression-first repair, publication, verification, and products as graph work.

Validate its graph contract with `cargo test -p st3 --test examples`; paid live orchestration uses
the repository's internal eval controller, not the public CLI.

Held-out gates (identical logic to ghost-bug): isolation (author-gated to `gbx.fix`), suite-green,
root-cause (two blind probes), **regression mutation-valid** (RED on the buggy BASE src — the integrity
bar, ported verbatim), coordination.

Fixture `worker/` reuses ghost-bug's labelkit (owner-pinned `gbx.fix`); `worker/AGENTS.md` +
`sup/AGENTS.md` are intentionally pre-seeded complete Codex personas. The frozen `worker/_git`
snapshot rehydrates as `.git` only inside the temporary eval root. The KDL uses native `codex {}` blocks.

The `receipts/` directory contains the passing proof.
