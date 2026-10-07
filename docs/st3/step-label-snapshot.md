# Selected labels in a captured snapshot

`store::step_labels::rows(connection, subjects)` exposes the existing selected
step-label SQL and decoder on the caller's `Connection`. It accepts at most
`501 * AGENT_WORK_PREVIEW_LIMIT * 2` subjects (currently 5010): a limit-plus-one
agent window with both held and upcoming previews. It executes one indexed
selected-step/owning-run join. Empty input returns an empty map; duplicates
collapse and absent subjects are omitted. The existing public `Store::step_labels`
shares that query and decoder and retains its previous accepted batch sizes.

The fields remain run, mission, path, optional title, first goal, raw status and
updated time. There is no generation restriction or effective-state adjustment:
those would change the existing label semantics. Parse failures remain errors.
No new source schema, registration, source installation or view ID is introduced.

Card adapters should use this accessor inside the same authorized snapshot as
queue selection and other card sources. A label query does not certify source
coverage, existence authority or an agent card's readiness. The shared source
owner must certify the projection and the adapter's complete dependency closure.
Invalidation includes selected step removal and changes to subject, run, path,
title, goals, raw status and updated time, plus old/new owning run mission ID and
run removal. Dependency fanout must remain bounded writer-side work; labels do
not provide a GET repair or a whole-agent scan.

The controls verify the existing public fields and actual SQL query-plan primary
key seeks, selected batch limits/duplicates/absence, and a real file-backed Store
snapshot retaining old labels across a newer normal state write. A fresh public
reader sees the new state while the existing snapshot still sees its original
cut. These are read-seam controls, not full agent or work IVM qualification.
