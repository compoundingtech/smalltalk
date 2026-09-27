# Model-free proof: 2026-08-28

- Result: Pass.
- Runtime: st2 version 1.
- Binaries: st2 0.1.0 at `6321c87`, and PTY `0.12.0+500eab2`.
- Wall time: 2.60 seconds.
- Model tokens: 0.
- KDL SHA-256: `0497e7ccc5483fe4e0ecfba6facb4a9b695360dde8b82cf937cb839e882ee469`.
- Observed flow: The attach stream sent snapshots across a forced reconnect. The final frame kept the machine-readable output contract.
- Cleanup: The run stopped the proxy, remote server, and synthetic PTY.
