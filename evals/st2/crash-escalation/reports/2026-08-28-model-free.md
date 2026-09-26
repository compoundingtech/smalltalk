# Model-free proof: 2026-08-28

- Result: Pass.
- Runtime: st2 version 1.
- Binary: st2 0.1.0 from source revision `6321c87`.
- Wall time: 20.21 seconds.
- Model tokens: 0.
- KDL SHA-256: `7dc94dcaf5932cd164686eaecd20ae2cde87160c9ce9a3f3bf251859679d3146`.
- Observed flow: One task crashed, supervision retried it, and escalation reached the supervisor. The clean exit produced no crash notice.
- Cleanup: The st2 eval runner stopped both tasks and removed the temporary catalog.
