# Eval run report — 2026-09-22

- Eval: `work-wake-reliability`
- Runtime: `st3`
- Run IDs: `mission-run/paid-work-wake-20260922-e` and `mission-run/paid-work-wake-20260922-f`
- Candidate commit: `d6206adf5faf39e75188866b53b91c5ac6d565a8` plus the implementation under test
- Eval KDL SHA-256: `81e17578cba75bdee12e074f6adabcc40fbd45519901179c95f0e4995bf05641`
- Result: `pass`, twice consecutively on one isolated node

## Timing

| Run | Started | Ended | Duration | Controller | Held-out gates | Cleanup |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| `e` | `2026-09-22T12:14:20.950Z` | `2026-09-22T12:17:08.462Z` | `167.512s` | `166.013s` | `0.577s` | `0.137s` |
| `f` | `2026-09-22T12:17:31.195Z` | `2026-09-22T12:20:06.938Z` | `155.743s` | `154.059s` | `0.744s` | `0.140s` |

## Coverage per run

Each run kept one real Codex worker across six assigned steps:

1. start and complete fresh mission one;
2. start and complete fresh mission two;
3. start a standing mission and complete its initial step;
4. revise the live mission and complete `revision-one`;
5. revise it again and complete `revision-two`;
6. send the worker a hangup, observe a distinct replacement incarnation, then start and complete a third fresh mission.

The controller then cancelled the standing run and required its agentless finalizer to complete.
Both top-level missions reached `completed`.

Across the two paid runs, all 12 assigned steps recorded exactly one native wake attempt and a
real `work.claimed` acknowledgement. The second run changed from incarnation
`3849475:2026-09-22T12:17:31.371Z` to
`3861215:2026-09-22T12:19:28.856Z`; its post-restart work was claimed by the latter.

## Judges

| Judge | Result | Evidence |
| --- | --- | --- |
| Lifecycle controller | `pass` twice | Three fresh runs, two live revisions, one replacement, and one cancellation finalizer converged per run. |
| Durable graph wake evidence | `pass` twice | Every step had one tagged `message.sent` claim before one `work.claimed` claim and ended `completed`. |
| Historical projection | `pass` | Wake evidence for superseded revision generations remained queryable after the worker incarnation changed. |
| No terminal input | `pass` twice | The executable audit log remained zero bytes and the graph contained no terminal-input request. |

## Defects found and fixed during calibration

- A running PTY registry row can outlive its operating-system process. Reconciliation now verifies
  PID liveness and treats that row as `vanished`, allowing the declared restart policy to act.
- Native driver restart binding previously trusted the stale graph incarnation. It now binds the
  exact current local PTY incarnation.
- A completed step's wake projection was keyed to the worker's current incarnation, so historical
  evidence disappeared after replacement. Projection now resolves the incarnation recorded in
  the durable wake tag.
- The first held-out judge asked the current mission-generation view for superseded steps. It now
  verifies the historical work card plus the underlying wake and claim records directly.
