# Run generation revision

This paid st3 eval proves one protected planning revision of a live mission run.

The controller completes one stable step before it starts a targeted Codex launch.
The planner submits a replacement, and the requester approves the exact preview hash.
st3 creates one successor generation and keeps the prior generation unchanged.

The gates prove these results:

- The mission run keeps its initial revision and selects the successor revision.
- The prior generation becomes `superseded` and remains readable.
- The successor names the prior generation as its predecessor.
- The stable completed step carries to the successor.
- The changed step does not carry completion.
- A new mechanical gate receives `ST_RUN_GENERATION` without an authored `env` mapping.
- The inner mission reaches completion after the proof captures both generations.

This eval uses one Codex planning run.

Validate its graph contract with `cargo test -p st3 --test examples`; live orchestration uses the
repository's internal eval controller, not the public CLI.
