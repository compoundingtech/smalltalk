# Find out what is happening

Suppose the invented catalog owner looks quiet and a message appears to be missing.

## Failure first: reading is a recipient lifecycle action

The sender retains the message ID:

```sh
sent_message="$(st3 conversations send agent/example/catalog/standing/owner \
  --from person/operator \
  --subject 'Catalog status' \
  --body 'Please report the active catalog run.')"
```

This is the wrong way for that sender to inspect it:

```sh
st3 conversations read "$sent_message" --as person/operator
```

`conversations read` is recipient-only because it records delivery and read lifecycle for the
recipient. A refusal here does not mean the sent message vanished, and it is not evidence that the
read command was removed.

Use a non-mutating view for a message you sent:

```sh
st3 conversations thread "$sent_message"
st3 subject show "$sent_message"
```

The recipient uses `conversations read --as` for its own inbox and archives the message after its
related action is complete.

## Supported recovery: move from overview to exact subject

These four views answer different questions in useful order:

```sh
st3 now --as person/operator
st3 agents tree --status running --enrich
st3 work ls --as agent/example/catalog/standing/owner
st3 missions show mission-run/example/catalog/first
```

- `now` summarizes what needs action for the selected person and the current fleet.
- `agents tree` shows whether the owner runtime is present, reachable, and attached to the expected run.
- `work ls` shows ready or owned steps for the exact agent, including blockers.
- `missions show` explains the exact run's goals, phase, generation, and step states.

If an ID from those views needs deeper inspection, pass that exact typed subject to
`st3 subject show`. Run `st3 COMMAND SUBCOMMAND --help` before concluding that an operation no
longer exists; the help for the exact subcommand is the authoritative command shape.
