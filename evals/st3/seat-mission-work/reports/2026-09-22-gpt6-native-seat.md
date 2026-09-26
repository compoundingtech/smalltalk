# Native seat canary — 2026-09-22

- Eval: `seat-mission-work`
- Runtime: isolated st3 daemon and immutable copied binaries
- Run ID: `mission-run/seat-mission-work-gpt6-20260922`
- Model: `gpt-6-sol`
- Candidate: current working tree based on `90a1317`
- Result: `pass`

The slash-qualified `agent/eval/seat-mission-work/worker` seat was running with
`owner_run_id: null` and had reached exact `idle` before the mission started. The finite mission
then assigned one work item to that existing seat.

The native wake was acknowledged by `claim` on its first attempt. The claim was bound to exact
incarnation `2984423:2026-09-22T23:11:57.161Z`; the seat entered `working`, created
`seat-proof.txt` with the exact required content, submitted completion, and returned to `idle`.
The held-out mechanical gate passed and the mission reached `completed` in 22.695 seconds. The
step's measured execution interval was 10.267 seconds. No terminal-input claim was present.

After evidence collection, the disposable top-level seat was explicitly stopped and the isolated
daemon was shut down.
