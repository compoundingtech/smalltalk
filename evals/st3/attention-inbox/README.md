# Human attention inbox eval

This model-free eval proves the complete human attention inbox.

The controller creates one current item for each review kind, plus a message that stays in conversations: messages and faults never enter a person's attention. It checks the global list and the selected person list.

The planning workspace is a file, not a directory. Its render fails, which prevents the temporary Codex planner from starting. The controller submits the fixed candidate directly as that planner and creates no model request.

The controller approves or closes each item. It then proves that the selected person's inbox is empty.

Validate its graph contract with `cargo test -p st3 --test integration examples::`; live orchestration uses the
repository's internal eval controller, not the public CLI.
