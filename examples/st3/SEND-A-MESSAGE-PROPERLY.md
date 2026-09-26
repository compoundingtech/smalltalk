# Send a message properly

This example sends a question about an invented catalog project.

## Failure first: a delivered message can still lose its context

This command delivers a body, but it supplies neither a visible subject nor a thread parent:

```sh
st3 conversations send agent/example/catalog/standing/owner \
  --from person/operator \
  --body 'Which catalog edition should I use?'
```

The missing title was not removed in transit; none was authored. Sending a later response without
`--in-reply-to` creates another root message, so it cannot appear as a reply in the first thread.

There is a second trap: `--body` is a shell argument. Unquoted command substitutions, dollar signs,
and backticks in prose can be expanded before st3 sees them.

## Supported recovery: author the subject and parent explicitly

Use a single-quoted heredoc delimiter so the shell preserves the body literally, then retain the
canonical `message/ID` printed by `send`:

```sh
message_body="$(/bin/cat <<'BODY'
Which catalog edition should the importer use?
Treat `$edition` and `$(edition-command)` as literal examples.
BODY
)"

request_message="$(st3 conversations send \
  agent/example/catalog/standing/owner \
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

st3 conversations send person/operator \
  --from agent/example/catalog/standing/owner \
  --subject 'Re: Choose the catalog edition' \
  --in-reply-to "$request_message" \
  --body "$reply_body"
```

`st3 conversations reply "$request_message" ...` is the shorter supported route when the sender
has the original message: it derives the recipient and `in-reply-to`, and preserves or replaces the
subject deliberately.

## Bodies larger than 4 KiB

Inline message content cannot exceed 4 KiB. Store a larger UTF-8 body as an immutable document,
then send its exact `doc/NAME@HASH` reference as the body:

```sh
document_ref="$(st3 documents put catalog-context.md --as doc/example/catalog-context)"
st3 conversations send agent/example/catalog/standing/owner \
  --from person/operator \
  --subject 'Catalog import context' \
  --body "$document_ref"
```

One stored document can be at most 1 MiB. The receiver resolves the pinned reference; replacing the
local file later cannot change the delivered bytes.
