# Model-free proof: 2026-08-28

- Result: Pass.
- Runtime: st2 version 1.
- Binary: st2 0.1.0 from source revision `6321c87`.
- Wall time: 1.30 seconds.
- Model tokens: 0.
- KDL SHA-256: `65490575e0cd2d7e0aa2b3e5d1198d4a62fa57585814c79753c8b0dbc61d37a5`.
- Observed flow: The run wrote context and a resource. Two daemon restarts kept both values available.
- Cleanup: The nested daemon stopped, and the st2 eval runner removed the temporary catalog.
