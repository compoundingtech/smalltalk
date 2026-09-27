# Eval run report — YYYY-MM-DD

- Eval: `<name>`
- Runtime: `st2|st3`
- Run ID: `<id>`
- Candidate commit: `<sha>`
- Eval KDL SHA-256: `<sha256>`
- Result: `pass|fail|stopped`

## Timing

- Started: `<ISO-8601>`
- Ended: `<ISO-8601>`
- Duration: `<seconds>`

## Model usage

| Role | Harness | Model | Tokens |
| --- | --- | --- | ---: |
| `<role>` | `<provider>` | `<model>` | `<count>` |

- Agent tokens: `<count>`
- LLM judge tokens: `<count>`
- Total model tokens: `<count>`
- Count source: `<runtime receipt, provider output, or unavailable reason>`

## Observable graph transitions

| Time | Subject or step | Transition | Evidence |
| --- | --- | --- | --- |
| `<time>` | `<subject>` | `<old> -> <new>` | `<store index, event, or log>` |

## Small Talk

- Runtime work messages: `<count>`
- Direct agent messages: `<count>`
- Required sequence: `<concise actor sequence>`
- Unexpected or duplicate messages: `<none or description>`

## Judges

| Judge | Result | Duration | Evidence |
| --- | --- | ---: | --- |
| `<name>` | `<pass|fail>` | `<seconds>` | `<concise evidence>` |

## Result details

- Products or commits: `<identifiers>`
- Cleanup: `<complete|incomplete>`
- Notable behavior: `<concise notes>`
- Follow-up: `<none or action>`
