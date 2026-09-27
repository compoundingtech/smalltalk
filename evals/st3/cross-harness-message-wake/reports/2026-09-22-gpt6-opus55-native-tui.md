# Native TUI canary — 2026-09-22

- Eval: `cross-harness-message-wake`, disposable Codex/Claude lane
- Runtime: isolated st3 daemon and immutable copied binaries
- Run ID: `mission-run/native-tui-gpt6-opus55-final-20260922`
- Candidate: current working tree based on `90a1317`
- Staged KDL SHA-256: `5e832dfc382ea3c13ca43f9d86c6056b04cbe6969b26c0174b67c1b18ef5b60f`
- Result: `pass`

## Scope

This run selected Codex `gpt-6-sol` and Claude Code's `opus` alias. Claude's native usage record
identified the concrete provider model as `claude-opus-5-5` with a 1,000,000-token context window.
Pi and OMP were omitted from this disposable copy because their executables were not installed on
the host; the committed four-harness fixture remains intact.

## Evidence

- The mission completed in 102.325 seconds, including held-out gates and cleanup.
- Startup messages were sent at `23:06:39.175Z`; both native delivery receipts existed by
  `23:07:01.538Z` and both independent consensus results existed by `23:07:33.118Z`.
- Both harnesses then reached exactly `idle` at store index 176. Idle-phase messages were sent at
  `23:07:41.710Z`; both native delivery receipts existed by `23:07:48.857Z` and both independent
  consensus results existed by `23:08:15.371Z`.
- The coordination gate passed exact fact, agreement, result, ordering, and lifecycle checks in
  both phases.
- The executable PTY audit recorded zero events and both agent histories contained zero
  `terminal.input.requested` claims.
- Cleanup stopped both owned PTY agents and the finite mission reached `completed`.

The live run first exposed and drove fixes for two eval/product regressions: controllers still read
the retired direct agent JSON shape, and typed Claude seats did not register lifecycle hooks against
their private st3 catalog. The passing run used the repaired client envelope parsing and dedicated
st3 Claude lifecycle settings.
