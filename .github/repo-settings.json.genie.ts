import { githubRepoSettings, githubRuleset } from '../repos/effect-utils/genie/external.ts'

// Do not apply before the parallel proving runs and the train-driver cutover.
// Keep the live main ruleset's other protections; only replace the CI check API.
export default githubRepoSettings({
  repository: { allow_auto_merge: true, delete_branch_on_merge: true },
  rulesets: [githubRuleset({
    name: 'main',
    target: 'branch',
    enforcement: 'active',
    bypass_actors: [],
    conditions: { ref_name: { include: ['~DEFAULT_BRANCH'], exclude: [] } },
    rules: [
      { type: 'deletion' },
      { type: 'non_fast_forward' },
      {
        type: 'pull_request',
        parameters: {
          required_approving_review_count: 0,
          dismiss_stale_reviews_on_push: false,
          required_reviewers: [],
          require_code_owner_review: false,
          dismissal_restriction: { enabled: false, allowed_actors: [] },
          require_last_push_approval: false,
          required_review_thread_resolution: false,
          require_extra_approval_for_unattributed_changes: true,
          allowed_merge_methods: ['merge', 'squash', 'rebase'],
        },
      },
      {
        type: 'required_status_checks',
        parameters: {
          strict_required_status_checks_policy: true,
          do_not_enforce_on_create: false,
          required_status_checks: [
            { context: 'linux-gate', integration_id: 15368 },
            { context: 'genie-freshness', integration_id: 15368 },
          ],
        },
      },
    ],
  })],
})
