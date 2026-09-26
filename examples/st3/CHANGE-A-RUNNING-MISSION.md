# Change a mission that is already running

This example uses the invented `example/revisable-change` mission and assumes its equally invented
standing owner already exists. Publish and start the initial definition, then allow its ordinary
work to complete. The run pauses at the `release` human gate, keeping it live while the completed
steps have durable state:

```sh
revision_workspace="$(mktemp -d)"
st3 missions publish examples/st3/mission-revision.kdl --as person/operator
st3 missions start example/revisable-change \
  --id example/revisable-change/first \
  --workspace "$revision_workspace" \
  --input 'request=Create the archive index.' \
  --as person/operator
```

## Failure first: publishing a replacement does not revise the run

This publishes a new immutable definition successfully:

```sh
st3 missions publish examples/st3/mission-revision-v2.kdl --as person/operator
st3 missions show mission-run/example/revisable-change/first
```

The second command still shows the revision and generation with which the run started. Publication
does not mutate a run in flight.

An out-of-band request does not mutate it either. If the operator sends the standing owner a
message asking it to perform `summarize-index` now, the correct response is a refusal:

```sh
change_message="$(st3 conversations send \
  agent/example/repository-owner/standing/repository-owner \
  --from person/operator \
  --subject 'Add the archive summary' \
  --body 'Please run summarize-index in the active archive run.')"

st3 conversations reply "$change_message" \
  --from agent/example/repository-owner/standing/repository-owner \
  --body 'The active generation has no summarize-index work. Apply a graph revision before I act.'
```

That refusal protects the durable plan. A message can explain a desired change, but it cannot grant
new work or authority.

## Supported recovery: revise the exact run

Submit the complete replacement through the live-run revision route:

```sh
st3 work revise \
  mission-run/example/revisable-change/first \
  examples/st3/mission-revision-v2.kdl \
  --reason 'Add a summary before final verification.' \
  --as person/operator
```

This mission is protected by `revisions="human-only"`, so submission creates a proposal rather
than silently cutting over. Inspect it, then copy the exact proposal subject and preview hash from
the output into the approval command:

```sh
st3 work revision show mission-run/example/revisable-change/first
st3 work revision approve revision-proposal/PROPOSAL PREVIEW_HASH --as person/operator
```

Its cutover policy is `when-idle`; approval permits the cutover once active work is idle. The
successor definition leaves `implement` unchanged, so completed compatible work carries into the
new generation as completed. The new `summarize-index` step becomes work, and `verify` must run
again because its dependency changed. The dependent `release` gate waits behind it. It is not
correct to redo every completed step, and it is not correct to mark changed or dependent work
complete by analogy.

Inspect both immutable generations to verify that result:

```sh
st3 work revision generations mission-run/example/revisable-change/first
st3 work revision generation run-generation/OLD_GENERATION
st3 work revision generation run-generation/NEW_GENERATION
```

Use the exact generation subjects printed by `generations`; the placeholders above are not literal
IDs.
