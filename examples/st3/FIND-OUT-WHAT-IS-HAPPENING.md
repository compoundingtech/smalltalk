# Find out what is happening

Suppose the `agent/example/worker` seat looks quiet and a message appears to be missing.

## Failure first: reading is a recipient lifecycle action

The sender retains the message ID:

```sh
sent_message="$(st conversations send agent/example/worker \
  --from person/operator \
  --subject 'Catalog status' \
  --body 'Please report the active catalog run.')"
```

This is the wrong way for that sender to inspect it:

```sh
st conversations read "$sent_message" --as person/operator
```

`conversations read` is recipient-only because it records delivery and read lifecycle for the
recipient. A refusal here does not mean the sent message vanished, and it is not evidence that the
read command was removed.

Use a non-mutating view for a message you sent:

```sh
st conversations thread "$sent_message"
st subject show "$sent_message"
```

The recipient uses `conversations read --as` for its own inbox and archives the message after its
related action is complete.

## Supported recovery: move from overview to exact subject

These five views answer different questions in useful order:

```sh
st now --as person/operator
st agents tree --status running --enrich
st agents queue agent/example/worker
st work ls --as agent/example/worker
st missions show mission-run/example/catalog/first
```

- `now` summarizes what needs action for the selected person and the current fleet.
- `agents tree` shows whether the seat's runtime is present and reachable.
- `agents queue` shows the step the seat holds, its next work, and every run waiting for it, in
  order. A quiet seat is often holding one step while the others wait behind it, or has only
  runs whose steps are waiting on a gate or a dependency.
- `work ls --as` shows ready or owned steps for the exact agent, in seat-queue order, including
  blockers.
- `missions show` explains the exact run's goals, phase, generation, and step states.

A seat receives work as mission steps, not as messages. If the question is why a seat has not
started some work, look for the step and its blockers rather than for a message.

If an ID from those views needs deeper inspection, pass that exact typed subject to
`st subject show`. Run `st COMMAND SUBCOMMAND --help` before concluding that an operation no
longer exists; the help for the exact subcommand is the authoritative command shape.
