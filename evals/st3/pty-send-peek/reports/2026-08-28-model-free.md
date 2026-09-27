# Model-free proof: 2026-08-28

- Result: Pass.
- Runtime: st3 version 2.
- Binaries: st3 0.1.0 at `6321c87`, and PTY `0.12.0+500eab2`.
- Wall time: 1.60 seconds.
- Model tokens: 0.
- KDL SHA-256: `275e80591696444426d27acd5211f0e3970c05c78fd1e57e093ed135b15a3e76`.
- Run subject: `mission-run/a96f9c36b9fc9d812025d843c5df3fd4`.
- Observed flow: The input request and result kept the PTY lifecycle status. The judge found the acknowledgement. The cleanup emptied the run scope.
