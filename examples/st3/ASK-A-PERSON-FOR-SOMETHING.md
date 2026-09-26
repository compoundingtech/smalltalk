# Ask a person for something

An invented catalog import can validate every input automatically, but its final output format
needs a person to choose between CSV and JSON.

## Failure first: a terminal question is not durable coordination

If the agent prints a blocking prompt in its terminal and waits, the responsible person has no
durable inbox item, the graph does not say what is blocked, and a restarted harness can lose the
question. A push notification would have the same ownership problem: it may attract attention, but
it is not the request or its lifecycle.

Do not stop unrelated work while waiting for an answer.

## Supported recovery: publish one explicit attention request

The agent names what it needs, what the answer unblocks, and what it did instead:

```sh
blocked_step=step-run/GENERATION_ID/publish
st3 attention request \
  --for person/operator \
  --title 'Choose the catalog export format' \
  --reason 'CSV or JSON is needed to unblock the publish step. Input validation is complete, and I am continuing the independent checksum report meanwhile.' \
  --severity warning \
  --target "$blocked_step" \
  --as agent/example/catalog/standing/owner \
  --idempotency-key example-catalog-export-format
```

Replace `GENERATION_ID` with the exact value printed by `st3 work ls`; it is a placeholder, not a
literal graph ID.

The stable idempotency key makes retries return the same attention request instead of creating a
pile of duplicates. The request appears in `person/operator`'s attention inbox; it is not a modal
prompt and it does not automatically block other graph work.

Record the material status and continue everything that does not depend on the choice:

```sh
st3 work progress "$blocked_step" \
  --as agent/example/catalog/standing/owner \
  --summary 'Requested the export-format decision; validation is complete and checksum work continues.'
st3 work ls --as agent/example/catalog/standing/owner
```

When the person resolves the request, the agent reads the durable outcome and resumes only the
dependent publish work. The attention item, its target, and its resolution remain graph history.
