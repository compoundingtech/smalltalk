# Send a message properly

This example sends a question about an invented catalog project.

Messages are for conversation: questions, answers, and context. They do not hand out work, and they
are not where results go. To give a seat work, start a mission whose step is assigned to it. To
report a result, complete the step with it or store it as a document and cite the reference. Agents
react to steps, gates, and dependencies, so a result in a message is invisible to the graph.

## Failure first: a delivered message can still lose its context

This command delivers a body, but it supplies neither a visible subject nor a thread parent:

```sh
st conversations send agent/example/worker \
  --from person/operator \
  --body 'Which catalog edition should I use?'
```

The missing title was not removed in transit; none was authored. Sending a later response without
`--in-reply-to` creates another root message, so it cannot appear as a reply in the first thread.

There is a second trap: `--body` is a shell argument. Unquoted command substitutions, dollar signs,
and backticks in prose can be expanded before st sees them.

## Supported recovery: author the subject and parent explicitly

Use a single-quoted heredoc delimiter so the shell preserves the body literally, then retain the
canonical `message/ID` printed by `send`:

```sh
message_body="$(/bin/cat <<'BODY'
Which catalog edition should the importer use?
Treat `$edition` and `$(edition-command)` as literal examples.
BODY
)"

request_message="$(st conversations send \
  agent/example/worker \
  --from person/operator \
  --subject 'Choose the catalog edition' \
  --body "$message_body")"
```

An explicit response has both its own subject and the original canonical parent:

```sh
reply_body="$(/bin/cat <<'BODY'
Use the invented spring edition. I recorded the choice in the active work evidence.
BODY
)"

st conversations send person/operator \
  --from agent/example/worker \
  --subject 'Re: Choose the catalog edition' \
  --in-reply-to "$request_message" \
  --body "$reply_body"
```

`st conversations reply "$request_message" ...` is the shorter supported route when the sender
has the original message: it derives the recipient and `in-reply-to`, and preserves or replaces the
subject deliberately.

## Long bodies

A message body can be up to 256 KiB of UTF-8. Send it as the body, however long:

```sh
st conversations send agent/example/worker \
  --from person/operator \
  --subject 'Catalog import context' \
  --body "$(cat catalog-context.md)"
```

Past 4 KiB, the message holds the first kilobyte as a preview and the whole text stays on the
machine that sent it; other members ask that machine when they read it. `st conversations read`
prints the whole text, and a seat is shown a preview with the instruction to read the rest. If the
sending machine cannot be reached, the reader sees the preview and which machine has the rest. Over
256 KiB the send is refused with `message-too-large`, and nothing is written: shorten the text or
send it as several messages. [Long messages](../../docs/st3/long-messages.md) has the details.

A pinned `doc/NAME@HASH` reference is still a valid body for a document of up to 1 MiB that
already exists:

```sh
document_ref="$(st documents put catalog-context.md --as doc/example/catalog-context)"
st conversations send agent/example/worker --from person/operator --body "$document_ref"
```
