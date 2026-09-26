# Model-free proof: 2026-08-28

- Result: Pass.
- Runtime: st3 version 2.
- Binary: st3 0.1.0 from source revision `6321c87`.
- Wall time: 1.10 seconds.
- Model tokens: 0.
- KDL SHA-256: `717b2841d38fd2050833b846461bd0fc6aadfc071aae749dc6a6b04793e46633`.
- Run subject: `mission-run/7cb2522278b39c31d0464b18883a1c64`.
- Observed flow: The contract restarted a nested daemon twice. The judge found the durable context and resource. The cleanup emptied the run scope.
