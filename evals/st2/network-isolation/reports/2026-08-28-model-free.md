# Model-free proof: 2026-08-28

- Result: Pass.
- Runtime: st2 version 1.
- Binaries: st2 0.1.0 at `6321c87`, and PTY `0.12.0+500eab2`.
- Wall time: 2.10 seconds.
- Model tokens: 0.
- KDL SHA-256: `4f2b995adae87348dd2d0498cd4f2defc0eb2f2d5fb3baaeffe486fc1cd0cc4b`.
- Observed flow: Two networks held separate PTY and message subjects. Neither network exposed the other network's secret.
- Cleanup: The run stopped both networks and their sessions.
