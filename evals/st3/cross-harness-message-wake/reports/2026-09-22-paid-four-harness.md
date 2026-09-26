# Eval run report — 2026-09-22

- Eval: `cross-harness-message-wake`
- Runtime: `st3`
- Run ID: `mission-run/paid-cross-harness-20260922-f`
- Candidate commit: `d6206adf5faf39e75188866b53b91c5ac6d565a8` plus the implementation under test
- Eval KDL SHA-256: `038619a562940b4ba44a9552a004fcfa8b30afd5dcd49a45dcd619ff5c13c84f`
- Result: `pass`

## Timing

- Started: `2026-09-22T11:35:03.129Z`
- Ended: `2026-09-22T11:37:21.714Z`
- Duration: `138.585 seconds`
- Native receipt deadline: `120 seconds`
- Independent model-turn deadline: `300 seconds` per stage

## Coverage

The paid run used Codex `gpt-5.6-sol`, Claude `claude-sonnet-5`, Pi with
`openai-codex/gpt-5.6-terra`, and OMP with `openai-codex/gpt-5.6-terra`.

Each harness completed the protocol twice. The startup phase preserved the exact measured native
pre-state for every harness. The second phase began only after all four harnesses were exactly
`idle`. In both phases each paired participant consumed one kickoff, sent one private fact, read
its peer's fact, exchanged the matching agreement, and independently reported consensus.

| Phase | Sent | All native receipts | Facts complete | Agreements complete | Results complete |
| --- | --- | --- | --- | --- | --- |
| startup | `11:35:05.848Z` | `11:35:42.451Z` | `11:36:03.822Z` | `11:36:09.007Z` | `11:36:21.264Z` |
| exact idle | `11:36:36.910Z` | `11:36:45.116Z` | `11:36:58.531Z` | `11:37:07.840Z` | `11:37:17.163Z` |

The controller completed in `132.756 seconds`, the held-out gates in `1.042 seconds`, and cleanup
in `0.659 seconds`.

## Judges

| Judge | Result | Evidence |
| --- | --- | --- |
| Controller | `pass` | All four harnesses completed both paired consensus protocols. |
| Coordination | `pass` | Exact canonical message counts, ordering, receipt bounds, peer facts, agreements, and independent result messages matched. |
| No terminal input | `pass` | The executable audit log was empty and the graph contained zero `terminal.input.requested` claims. |
| Cleanup | `pass` | All four owned agents stopped and the mission reached `completed`. |

## Usage observation

The mission projection reported `usage: null` even though all four providers performed paid model
turns. That is not treated as zero and was not a messaging pass criterion. It is retained as a
separate accounting defect: the native providers completed their work, but the driver usage
records did not reach the step and mission rollups.
