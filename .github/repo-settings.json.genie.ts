import { githubRepoSettings, githubRuleset } from '../repos/effect-utils/genie/external.ts'

// Landing goes through GitHub's merge queue. Apply the fourth required check only after
// typescript-client has passed on main; the live ruleset keeps its three existing checks until then.
// Every required job must run on merge_group, or the queue never receives its checks and merges freeze.
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
          // The merge queue tests every entry on top of the current main and the entries ahead of it,
          // so a pull request no longer has to be rebased onto the latest main before it can be queued.
          strict_required_status_checks_policy: false,
          do_not_enforce_on_create: false,
          required_status_checks: [
            { context: 'linux-gate', integration_id: 15368 },
            { context: 'isolation-vm', integration_id: 15368 },
            { context: 'genie-freshness', integration_id: 15368 },
            { context: 'typescript-client', integration_id: 15368 },
          ],
        },
      },
      {
        type: 'merge_queue',
        parameters: {
          // The repository merges with merge commits today (the st train did).
          merge_method: 'MERGE',
          grouping_strategy: 'ALLGREEN',
          // Namespace's measured Linux limit is 320 vCPU / 640 GiB. Each full Workspace CI group
          // initially requests 40 vCPU / 80 GiB, so five groups fit; PR and main jobs share capacity.
          max_entries_to_build: 5,
          max_entries_to_merge: 5,
          min_entries_to_merge: 1,
          min_entries_to_merge_wait_minutes: 5,
          // A required check that never reports (a lost Namespace job) fails the entry after this
          // long instead of blocking the queue for the default hour.
          check_response_timeout_minutes: 30,
        },
      },
    ],
  })],
})
