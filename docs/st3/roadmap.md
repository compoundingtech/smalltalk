# st product roadmap

This file records accepted future work that is not built yet. Active fleet work belongs in st missions.

## Agent continuity

- [ ] Suspend an idle seat automatically at a long graph wait or human gate, and resume it when the
  graph makes its work ready again. Manual [suspend and resume](suspend.md) exist today.

## User interfaces

- [ ] Hold a product design session for `st` and the Fractal mobile application.

## Runtime containment

- [ ] Add optional CPU, memory, process, and disk limits for each mission-owned runtime.
- [ ] Evaluate the macOS sandbox and resource-limit surfaces before selecting an enforcement contract.

## Harness accounts

- [ ] Extend [model accounts](accounts.md) to OMP seats.
- [ ] Offer round-robin selection within a pool, without moving active conversations.
- [ ] Evaluate [Comradex](https://github.com/nicosuave/comradex) as a reference for Codex account routing.
