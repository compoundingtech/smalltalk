# Model-free proof: 2026-08-28

- Result: Pass.
- Runtime: st2 version 1.
- Binaries: st2 0.1.0 at `6321c87`, and PTY `0.12.0+500eab2`.
- Wall time: 3.50 seconds.
- Model tokens: 0.
- KDL SHA-256: `9a2e1ad7d47646b04d765e689abb4f96b5dc9472b3fc6fe5552139f3fcb280f4`.
- Observed flow: The pre-send screen had no acknowledgement. The post-send screen contained the exact acknowledgement.
- Cleanup: The run stopped the synthetic PTY and left the live registry unchanged.
