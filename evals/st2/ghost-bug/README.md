# ghost-bug — the ghost-bug debug loop, run by Codex seats

The team finds the root cause of a shared-default-mutation bug in `labelkit`.

The eval requires a mutation-valid regression test. A shallow patch or an ineffective test cannot pass.

**Run it:** `st2 eval ./evals/st2/ghost-bug/`

Held-out judges (identical logic to ghost-bug): isolation (author-gated to `gbx.fix`), suite-green,
root-cause (two blind probes), **regression mutation-valid** (RED on the buggy BASE src — the integrity
bar, ported verbatim), coordination.

Fixture `worker/` reuses ghost-bug's labelkit (owner-pinned `gbx.fix`); `worker/AGENTS.md` +
`sup/AGENTS.md` are intentionally pre-seeded complete Codex personas. The frozen `worker/_git`
snapshot rehydrates as `.git` only inside the throwaway catalog. The KDL uses native bare `ding`, with
no authored bus path or compatibility wake command.
