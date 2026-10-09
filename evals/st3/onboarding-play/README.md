# Onboarding play

This eval measures the first-run play with real harnesses: the Smalltalk Assistant greets a person,
shows two agents messaging each other, runs a small mission, and interviews the person about what
they want to do. It checks what the person experiences and what the graph recorded, never only what
the Assistant claims.

## Run it

```sh
# Spends nothing: checks the judge against its fixtures and prints the plan.
evals/st3/onboarding-play/run.sh

# Spends nothing: a throwaway st with no harness, proving the collector works on a live graph.
evals/st3/onboarding-play/run.sh --plumbing --st-bin target/debug/st3

# The paid run. Both variants, at most two runs, on a login you made yourself.
evals/st3/onboarding-play/run.sh --paid --max-runs 2 --harness claude \
  --claude-config-dir /path/to/claude-login --st-bin target/debug/st3
```

`--max-runs` is required for a paid run and caps it. Use `--harness codex --codex-home DIR` for
Codex. Every run is a throwaway install with its own `HOME`, daemon and no service. Nothing is
copied from your real home, no login is created or copied for you, and no API key file is read:
the harness account is exactly the directory you name. The first sign of a login or rate-limit
problem stops the whole eval; a paid run is never retried. Reports go to `reports/run-TIME/`.

## What a run does

| Variant | The person | Expected |
| --- | --- | --- |
| `silent` | Never types | The Assistant plays all three acts and, after its interview question, carries on with the default it stated |
| `skip` | Types `skip` during act one | The Assistant stops the demo, says how to return, and the onboarding run is cancelled |

## What is judged (`judge.sh`)

The judge is pure: it scores one evidence bundle, so it is tested without a harness
(`self-test.sh`, and `cargo test -p st3 --test integration onboarding_eval`).

- **Time to the first agent-to-agent message**, from the graph (default budget 180 s, reported).
- **Acts one and two complete**, from the graph and the demo folder: the second agent is declared,
  messages went both ways, the demo mission completed and left its note.
- **Second person**: at least five uses of "you", no mention of Home, attention items, steps,
  gates, verifying, the graph, `st conversations`, `st ui`, `person/`, "the person" or a sample name.
- **The 30 second default fires**: the person stayed silent, and the Assistant spoke again between
  25 and 150 s after its interview question.
- **Skip cancels** (skip variant): the Assistant answers within 60 s and names `st setup --onboarding`,
  the demo mission is not left running, nothing is sent to the demo agent afterwards, and the
  onboarding run is cancelled.

Budgets can be changed with `FIRST_MESSAGE_BUDGET_S`, `DEFAULT_MIN_S`, `DEFAULT_MAX_S`,
`SKIP_BUDGET_S` and `MIN_YOU`.

## What is not yet proven

`collect.sh` reads message times from each message's delivery age and the Assistant's own words from
its conversation timeline; both were exercised only against a graph with no Assistant. The first
paid run is also the first time it sees a real play, so read its `.json` bundle next to the verdict.
The `skip` trigger waits for the second agent's declaration, then sends `skip` five seconds later.
