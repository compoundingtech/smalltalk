# Model-free proof: 2026-08-28

- Result: Pass.
- Runtime: st3 version 2.
- Binaries: st3 0.1.0 at `6321c87`, and PTY `0.12.0+500eab2`.
- Wall time: 2.10 seconds.
- Model tokens: 0.
- KDL SHA-256: `d8c3969fe897b7741e590ada49f8fcba3975bf0c4d73641b980ab9e655759817`.
- Run subject: `mission-run/0bed58729879fc74e0e2d598e832ac48`.
- Observed flow: Two nested APIs kept separate PTY and message subjects. The judge found both isolation markers. The cleanup emptied the run scope.
