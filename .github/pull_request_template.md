## Change

Describe the concrete problem, resulting behavior, and relevant verification.

## Upgrade impact

- [ ] Commit a `release-notes/NAME.json` fragment, including for changes with no runtime impact.
- [ ] Classify replay, schema, checkpoint rules/fleet coordination, client/protocol/harness compatibility, service interruption, manual steps and recovery as `none`, `changed` or `unknown`.
- [ ] Include affected starting versions/state, all intermediate version transitions, and pinned instructions for manual steps. Do not infer replay from a rules bump or compatibility from an unchanged schema number.
- [ ] Reviewers checked the fragment against the change. Operations can add contextual restart-to-API and restart-to-health observations before publication.

See [release impact authoring](https://github.com/compoundingtech/smalltalk/blob/main/docs/st3/release-impact.md). The fast `upgrade-impact` check requires a fresh valid fragment for PRs numbered above #1661 and feeds the required Linux gate. Publication also checks the complete interval since the previous release.
