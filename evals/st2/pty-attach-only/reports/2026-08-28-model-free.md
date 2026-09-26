# Model-free proof: 2026-08-28

- Result: Pass.
- Runtime: st2 version 1.
- Binaries: st2 0.1.0 at `6321c87`, and PTY `0.12.0+500eab2`.
- Wall time: 4.90 seconds.
- Model tokens: 0.
- KDL SHA-256: `a93d3ad5a2eec97f1f1e72d105c6eaa37b5198ab23612f1c95164b8a3c6fc32b`.
- Observed flow: Attach-only connected to a live session and refused a dead session. The legacy control restarted only when requested.
- Cleanup: The run removed all three synthetic PTY sessions.
