# st3 schema registry

This file is generated from `st3-schema`.

Schema: `st3.v1`
Digest: `d49966b40954fabd7d348e51e8ba50c93242743f7ce40e683abca7e84dc07def`

## Subject families

| Family | Pattern | Client writable | Description |
|---|---|---:|---|
| `account` | `account/NAME` | no | An external provider account identity. |
| `agent` | `agent/RUN/LOCAL_ID` | no | A mission-run agent runtime. |
| `attention` | `attention/ID` | yes | An explicit request for human attention. |
| `custom` | `custom/NAMESPACE/NAME` | yes | An extension subject. |
| `daemon` | `daemon/NODE` | no | An st3 daemon. |
| `doc` | `doc/NAME` | no | A named immutable document lineage. |
| `exec` | `exec/RUN/LOCAL_ID` | no | A mission-run exec runtime. |
| `file` | `file/HOST:/ABSOLUTE_PATH` | no | A read-only file gate target. |
| `gate-operation` | `gate-operation/IDENTITY` | no | One gate evaluation attempt. |
| `host` | `host/NAME` | no | A graph host. |
| `loop-run` | `loop-run/GENERATION/PATH` | no | One bounded loop execution. |
| `message` | `message/ID` | yes | A Small Talk message. |
| `mission` | `mission/ID` | no | An immutable mission revision lineage. |
| `mission-run` | `mission-run/ID` | no | A mission execution. |
| `observer` | `observer/RUN/LOCAL_ID` | no | A mission-run resource observer. |
| `person` | `person/IDENTITY` | no | A human actor. |
| `planning-session` | `planning-session/ID` | no | A durable planning session. |
| `pty` | `pty/RUN/LOCAL_ID` | no | A mission-run terminal runtime. |
| `repair` | `repair/ID` | yes | An immutable receipt for a bounded graph or replication repair. |
| `resource` | `resource/NAME` | yes | An observed external or durable fact bag. |
| `revision-proposal` | `revision-proposal/ID` | no | A mission revision proposal. |
| `run-generation` | `run-generation/ID` | no | An immutable mission-run generation. |
| `schedule` | `schedule/RUN/LOCAL_ID` | no | A mission-run schedule. |
| `step-run` | `step-run/GENERATION/PATH` | no | One step attempt lineage. |
| `subscription` | `subscription/RUN/LOCAL_ID` | no | A mission-run observer subscription. |

Custom subjects use `custom/NAMESPACE/NAME`. Custom claims use `custom.NAMESPACE.NAME`.

## Resource kinds

| Kind | Facts | Description |
|---|---|---|
| `ci.run` | `commit:subject-reference`, `completed_at:string`, `conclusion:string`, `external_id:string`, `name:string`, `provider:string`, `pull_request:subject-reference`, `repository:subject-reference`, `started_at:string`, `status:string`, `url:string` | A continuous integration run. |
| `filesystem.file` | `content_hash:string`, `mode:integer`, `path:string immutable`, `reason:string`, `size:integer`, `status:string` | A file observed through an explicit local path. |
| `harness.session-file` | `agent:subject-reference`, `harness:string immutable`, `incarnation_id:string`, `modified_at:string`, `path:string`, `session_id:string`, `status:string` | A harness session file that can outlive one runtime incarnation. |
| `human.review` | `decision:string`, `document:string`, `reason:string`, `reviewer:subject-reference`, `submitted_at:string`, `target:subject-reference` | A human review of another graph subject. |
| `vcs.commit` | `author:string`, `committed_at:string`, `committer:string`, `message:string`, `parents:array`, `repository:subject-reference immutable`, `sha:string immutable`, `state:string`, `tree:string immutable`, `url:string` | An immutable version control commit. |
| `vcs.issue` | `author:string`, `created_at:string`, `labels:array`, `number:integer`, `repository:subject-reference`, `state:string`, `title:string`, `updated_at:string`, `url:string` | A version control issue. |
| `vcs.pull-request` | `author:string`, `base:subject-reference`, `checks:array`, `created_at:string`, `draft:boolean`, `head:subject-reference`, `merged:boolean`, `number:integer`, `repository:subject-reference`, `reviews:array`, `state:string`, `title:string`, `updated_at:string`, `url:string` | A version control pull request. |
| `vcs.ref` | `ancestors:array`, `head:string`, `name:string immutable`, `ref_type:string`, `repository:subject-reference immutable`, `target:subject-reference`, `url:string` | A named version control reference. |
| `vcs.repository` | `default_ref:subject-reference`, `head:subject-reference`, `issues:array`, `pull_requests:array`, `state:string`, `url:string`, `vcs:string` | A version control repository. |
| `custom.NAMESPACE.NAME` | open fact bag | A namespaced custom resource. |

## Claim kinds

| Kind | Subjects | Write policy | Cardinality | Fields | KDL source |
|---|---|---|---|---|---|
| `agent.account` | `agent` | `same-subject-actor` | `state-transition` | `account!:subject-reference(account)` |  |
| `agent.presence` | `agent` | `same-subject-actor` | `append` | `presence!:string`, `reachability:string`, `reason:string` |  |
| `attention.requested` | `attention` | `authorized-participant` | `once` | `reason!:string`, `reviewer!:subject-reference(person)`, `severity!:string`, `targets:array`, `title!:string` |  |
| `attention.resolved` | `attention` | `authorized-participant` | `once` | `outcome!:string`, `reason:string`, `request!:string` |  |
| `daemon.diagnostic` | `daemon` | `system-only` | `append` | `code!:string`, `reason!:string`, `severity!:string`, `status:string` |  |
| `daemon.started` | `daemon` | `system-only` | `append` | `pid:integer`, `schema:string`, `schema_digest:string`, `status!:string`, `version:string` | `reset` |
| `doc.bound` | `doc` | `authorized-requester` | `append` | `executable:boolean`, `hash:string`, `name:string`, `size:integer` | `doc` |
| `eval.verdict` | `mission-run` | `system-only` | `once` | `reason:string`, `residue:array`, `verdict!:string` |  |
| `file.observed` | `file` | `system-only` | `append` | `blob_hash:string`, `content:string`, `content_hash:string`, `mode:integer`, `path!:string`, `reason:string`, `status!:string` | `gate` |
| `gate.requested` | `gate-operation` | `system-only` | `once` | `attempt:integer`, `baseline:boolean`, `capability_expires_at:string`, `capability_hash:string`, `decisions:array`, `gate:string`, `mission_revision:string`, `model:string`, `operation:subject-reference`, `owner:subject-reference`, `question:string`, `review_targets:array`, `reviewer:subject-reference`, `runner:string`, `status:string`, `step_definition:string`, `token_budget:integer`, `tools:array` | `gate` |
| `gate.result` | `gate-operation` | `capability-holder` | `append` | `baseline:boolean`, `field:string`, `gate:string`, `operation:subject-reference`, `reason:string`, `request:string`, `stage:string`, `token_usage:integer`, `value:any`, `verdict!:string` | `gate` |
| `harness.context-clear.requested` | `agent` | `authorized-requester` | `append` | `context_epoch:string`, `incarnation_id:string`, `operation_status:string`, `runtime_id:string` |  |
| `harness.context-clear.result` | `agent` | `system-only` | `once` | `context_epoch:string`, `incarnation_id:string`, `reason:string`, `result!:string`, `runtime_id:string` |  |
| `harness.diagnostic` | `agent` | `same-subject-actor` | `append` | `code:string`, `incarnation_id:string`, `observed_since_ms:integer`, `reason:string`, `retry_after_unix_ms:integer`, `retry_attempt:integer`, `severity:string`, `status:string`, `step_run:subject-reference(step-run)`, `wake_attempts:integer` |  |
| `harness.observed` | `agent` | `same-subject-actor` | `append` | `ask:string`, `blocked_on:string`, `driver:string`, `evidence_incarnation:string`, `exit:string`, `incarnation_id:string`, `input_buffer:string`, `observed_at_ms:integer`, `observed_since_ms:integer`, `ownership_sequence:integer`, `reason:string`, `state!:string`, `transition_sequence:integer`, `transport:string` |  |
| `harness.session-file` | `agent` | `authorized-requester` | `append` | `agent:subject-reference(agent)`, `discovery_revision:string`, `harness!:string`, `incarnation_id:string`, `modified_at:string`, `path:string`, `session_id!:string`, `source_session:string`, `status:string` |  |
| `harness.timeline` | `agent` | `same-subject-actor` | `append` | `body!:object`, `driver!:string`, `entry_id!:string`, `entry_type!:string`, `final!:boolean`, `incarnation_id!:string`, `observed_at_unix_ms:integer`, `operation!:string`, `revision!:integer`, `role!:string`, `sequence:integer` |  |
| `harness.usage` | `agent` | `same-subject-actor` | `append` | `cached_tokens:integer`, `compactions:integer`, `context_used_percent:number`, `context_used_tokens:integer`, `context_window_tokens:integer`, `cost:number`, `currency:string`, `driver!:string`, `incarnation_id!:string`, `input_tokens:integer`, `last_compaction_ms:integer`, `last_compaction_trigger:string`, `model:string`, `output_tokens:integer`, `semantics!:string`, `total_tokens:integer` |  |
| `intent.desired` | `*` | `authorized-requester` | `state-transition` | `desired:object`, `kind:string`, `revision:string` | `account`, `agent`, `doc`, `exec`, `host`, `message`, `observer`, `mission`, `mission-run`, `planning-session`, `pty`, `resource`, `schedule`, `step`, `stop`, `subscription` |
| `loop.round-dispatch` | `loop-run` | `system-only` | `append` | `candidate:integer`, `dispatch!:integer`, `item_id:string`, `mission_run!:subject-reference(mission-run)`, `reason!:string`, `round!:integer`, `status!:string` | `loop`, `round` |
| `loop.round-result` | `loop-run` | `system-only` | `append` | `candidate:integer`, `feedback:subject-reference(doc)`, `item:any`, `metrics:object`, `mission_run!:subject-reference(mission-run)`, `reason:string`, `round!:integer`, `status!:string`, `token_usage:integer` | `loop`, `round` |
| `loop.state` | `loop-run` | `system-only` | `state-transition` | `best_metrics:object`, `best_round:integer`, `feedback:subject-reference(doc)`, `items:array`, `reason:string`, `round:integer`, `status!:string`, `winner:integer` | `loop` |
| `message.closed` | `message` | `authorized-participant` | `once-per-actor` | `status!:string` |  |
| `message.delivered` | `message` | `system-only` | `once-per-actor` | `recipient:subject-reference`, `runtime_id:string`, `status!:string`, `transport:string` | `message` |
| `message.read` | `message` | `authorized-participant` | `once-per-actor` | `status!:string` |  |
| `message.sent` | `message` | `ordinary-client` | `once` | `content:string`, `from:subject-reference`, `in_reply_to:subject-reference`, `session_id:string`, `status!:string`, `tags:array`, `title:string`, `to:subject-reference` | `message` |
| `message.staged` | `message` | `system-only` | `once-per-actor` | `recipient:subject-reference`, `runtime_id:string`, `status!:string`, `transport:string` | `message` |
| `mission-run.created` | `mission-run` | `system-only` | `once` | `current_generation:subject-reference`, `deadline_at_unix_ms:integer`, `default_selector:object`, `generation:subject-reference`, `initial_revision:string`, `inputs:object`, `mission:subject-reference`, `mode:string`, `parent_step_run:subject-reference`, `requester:subject-reference`, `revision:string`, `root_mission_run:subject-reference`, `root_revision:string`, `status:string`, `timeout_ms:integer`, `workspace:string` | `mission-run` |
| `mission-run.state` | `mission-run` | `system-only` | `state-transition` | `completion:string`, `finally:string`, `phase:string`, `previous_phase:string`, `reason:string`, `status:string` | `mission-run`, `completion`, `finally`, `cancellation` |
| `mission.produced` | `mission`, `step-run` | `capability-holder` | `append` | `attempt:integer`, `mission:subject-reference`, `name:string`, `revision:string`, `step_definition:string` | `produces` |
| `mission.published` | `mission` | `authorized-requester` | `append` | `body:object`, `revision:string`, `state:string` | `mission` |
| `observer.observed` | `observer` | `system-only` | `append` | `attempt:string`, `changed:boolean`, `changed_fields:array`, `cursor:string`, `locator:string`, `next_check_unix_ms:string`, `observation:subject-reference`, `provider:string`, `resource:subject-reference`, `revision:string`, `status:string` | `observer` |
| `observer.refresh-requested` | `observer` | `system-only` | `append` | `attempt!:string`, `revision!:string` | `refresh` |
| `observer.state` | `observer` | `system-only` | `state-transition` | `attempt:string`, `next_check_unix_ms:string`, `reason:string`, `revision:string`, `state!:string` | `observer` |
| `planning-session.approved` | `planning-session` | `authorized-requester` | `once` | `candidate_revision:integer`, `kdl:subject-reference`, `markdown:subject-reference`, `mission_revision:string`, `preview_hash:string`, `preview_token:string`, `requester:subject-reference`, `variant:string` |  |
| `planning-session.cancelled` | `planning-session` | `authorized-requester` | `once` | `reason:string`, `requester:subject-reference` | `cancellation` |
| `planning-session.candidate-submitted` | `planning-session` | `authorized-participant` | `append` | `candidate_revision:integer`, `kdl:subject-reference`, `markdown:subject-reference`, `mission_revision:string`, `revision:integer`, `variant:string` |  |
| `planning-session.previewed` | `planning-session` | `system-only` | `append` | `candidate_revision:integer`, `diff:string`, `graph:string`, `mission:object`, `preview_hash:string`, `store_index:integer`, `variant:string` |  |
| `planning-session.question-answered` | `planning-session` | `authorized-requester` | `append` | `decision_id:string`, `expected_revision:integer`, `explanation:string`, `requester:subject-reference`, `response:object` |  |
| `planning-session.question-requested` | `planning-session` | `authorized-participant` | `append` | `decision_id:string`, `decision_type!:string`, `options:array`, `planner:subject-reference`, `question:string`, `requester:subject-reference`, `revision:integer` |  |
| `planning-session.revision-requested` | `planning-session` | `authorized-requester` | `append` | `candidate_revision:integer`, `feedback:subject-reference`, `requester:subject-reference`, `variant:string` | `feedback` |
| `planning-session.started` | `planning-session` | `authorized-requester` | `once` | `mission:subject-reference`, `planner:subject-reference`, `planner_config:object`, `request:subject-reference`, `requester:subject-reference`, `target_generation:subject-reference`, `target_run:subject-reference`, `workspace:string` | `planning-session` |
| `publication.operation` | `*` | `system-only` | `append` | `action:string`, `operation:string`, `status!:string` | `revision`, `reset`, `cancellation`, `refresh`, `feedback` |
| `record.repaired` | `repair` | `ordinary-client` | `once` | `reason!:string`, `record!:string`, `replacement!:string` | `repair` |
| `render.applied` | `agent`, `exec`, `pty` | `system-only` | `append` | `writes:array` |  |
| `repair.applied` | `repair` | `system-only` | `once` | `affected_subjects:array`, `item_count:integer`, `reason!:string`, `token!:string` |  |
| `resource.observed` | `resource` | `ordinary-client` | `append` | `kind:string`, `observed_at:integer`, `state:any` | `resource` |
| `revision-proposal.applied` | `revision-proposal` | `system-only` | `once` | `reason:string`, `status:string`, `successor_generation:subject-reference` |  |
| `revision-proposal.approved` | `revision-proposal` | `authorized-requester` | `once-per-actor` | `all_approved:boolean`, `preview_hash:string`, `reviewer:subject-reference` |  |
| `revision-proposal.cancelled` | `revision-proposal` | `authorized-requester` | `once` | `reason:string`, `status:string` |  |
| `revision-proposal.created` | `revision-proposal` | `authorized-requester` | `once` | `candidate_revision:string`, `compatible_steps:array`, `cutover:string`, `preview_hash:string`, `reason:string`, `reviewers:array`, `run:subject-reference`, `source_generation:subject-reference`, `status:string` |  |
| `run-generation.created` | `run-generation` | `system-only` | `once` | `compatible_steps:array`, `predecessor:subject-reference`, `reason:string`, `revision:string`, `run:subject-reference`, `status:string` | `mission-run`, `revision` |
| `run-generation.state` | `run-generation` | `system-only` | `state-transition` | `phase:string`, `previous_phase:string`, `reason:string`, `status:string`, `successor:subject-reference` | `mission-run`, `step`, `completion`, `finally`, `revision`, `cancellation` |
| `run-generation.superseded` | `run-generation` | `system-only` | `once` | `phase:string`, `previous_phase:string`, `reason:string`, `status:string`, `successor:subject-reference` | `revision` |
| `runtime.action.deadline-reached` | `agent`, `exec`, `pty`, `gate-operation` | `system-only` | `append` | `action:string`, `deadline_key:string`, `desired_token:string`, `incarnation_id:string`, `operation:string`, `operation_status:string`, `reason:string`, `runtime_id:string`, `signal:string`, `terminal:boolean` | `stop`, `gate` |
| `runtime.action.failed` | `agent`, `exec`, `pty`, `gate-operation` | `system-only` | `append` | `action:string`, `deadline_key:string`, `desired_token:string`, `incarnation_id:string`, `operation:string`, `operation_status:string`, `reason:string`, `runtime_id:string`, `signal:string`, `terminal:boolean` | `stop`, `gate` |
| `runtime.action.requested` | `agent`, `exec`, `pty`, `gate-operation` | `authorized-requester` | `append` | `action:string`, `deadline_unix_ms:string`, `incarnation_id:string`, `operation:string`, `runtime_id:string`, `signal:string`, `terminal:boolean` | `stop`, `gate` |
| `runtime.action.succeeded` | `agent`, `exec`, `pty`, `gate-operation` | `system-only` | `append` | `action:string`, `deadline_key:string`, `desired_token:string`, `incarnation_id:string`, `operation:string`, `operation_status:string`, `reason:string`, `runtime_id:string`, `signal:string`, `terminal:boolean` | `stop`, `gate` |
| `runtime.observed` | `agent`, `exec`, `pty`, `gate-operation` | `same-subject-actor` | `append` | `adopted:boolean`, `driver:string`, `exit_code:integer`, `exit_signal:integer`, `host:string`, `incarnation_id:string`, `reachability:string`, `reason:string`, `runtime_id:string`, `shutdown_timeout_ms:integer`, `status:string`, `terminal:boolean` |  |
| `runtime.readiness-deadline-reached` | `agent` | `system-only` | `append` | `deadline_unix_ms!:string`, `driver!:string`, `incarnation_id!:string`, `reason!:string`, `runtime_id!:string` |  |
| `runtime.reconcile-decision` | `agent`, `exec`, `pty`, `schedule` | `system-only` | `append` | `decision:string`, `gate:string`, `input_number:integer`, `key:string`, `reachability:string`, `reason:string`, `restart_at_unix_ms:string` |  |
| `runtime.restart-window-reset` | `agent`, `exec`, `pty` | `system-only` | `append` | `desired_token:string`, `incarnation_id!:string`, `reason!:string` | `reset` |
| `schedule.occurrence-cancelled` | `schedule` | `system-only` | `append` | `occurrence:integer`, `reason:string`, `revision:string` | `schedule` |
| `schedule.occurrence-reached` | `schedule` | `system-only` | `append` | `at_unix_ms:integer`, `occurrence:integer`, `revision:string`, `scheduled:subject-reference`, `scheduled_at_unix_ms:string` | `schedule` |
| `schedule.occurrence-scheduled` | `schedule` | `system-only` | `append` | `at_unix_ms:integer`, `occurrence:integer`, `revision:string`, `scheduled_at_unix_ms:string` | `schedule` |
| `schedule.work-requested` | `schedule` | `system-only` | `append` | `inputs!:object`, `mission!:subject-reference(mission)`, `mission_revision!:string`, `occurrence!:integer`, `revision!:string`, `workspace!:string` | `schedule` |
| `schedule.work-started` | `schedule` | `system-only` | `append` | `mission_run!:subject-reference(mission-run)`, `request!:string` | `schedule` |
| `step-run.carried` | `step-run` | `system-only` | `once` | `attempt:integer`, `definition_hash:string`, `source:subject-reference`, `source_generation:subject-reference`, `source_step_run:subject-reference`, `status:string`, `worker_reported:boolean` | `step` |
| `step-run.retried` | `step-run` | `system-only` | `append` | `attempt:integer`, `not_before_unix_ms:integer`, `reason:string`, `status:string` | `step` |
| `step-run.state` | `step-run` | `system-only` | `state-transition` | `attempt:integer`, `readiness_epoch:integer`, `reason:string`, `status:string` | `step` |
| `subscription.mission-requested` | `subscription` | `system-only` | `append` | `discovery!:string`, `mission!:subject-reference(mission)`, `mission_revision!:string`, `requester:subject-reference(agent|person)`, `resource!:subject-reference(resource)`, `resource_input!:string`, `workspace!:string` | `subscription` |
| `subscription.mission-started` | `subscription` | `system-only` | `append` | `mission_run!:subject-reference(mission-run)`, `request!:string` | `subscription` |
| `subscription.state` | `subscription` | `system-only` | `state-transition` | `fields:array`, `observer:subject-reference`, `reason:string`, `state!:string`, `to:subject-reference` | `subscription` |
| `terminal.input.requested` | `agent`, `pty` | `authorized-requester` | `append` | `byte_count:integer`, `incarnation_id:string`, `intent:string`, `mode:string`, `runtime_id:string`, `sequence:integer`, `sha256:string` |  |
| `terminal.input.result` | `agent`, `pty` | `system-only` | `append` | `incarnation_id:string`, `reason:string`, `result!:string`, `runtime_id:string`, `sequence:integer` |  |
| `transport.observed` | `host` | `system-only` | `append` | `last_success_at:integer`, `protocol:string`, `reason:string`, `remote_heads:object`, `status!:string` |  |
| `work.claimed` | `step-run` | `authorized-participant` | `state-transition` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.failed` | `step-run` | `authorized-participant` | `once-per-attempt` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.progress` | `step-run` | `authorized-participant` | `append` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.released` | `step-run` | `authorized-participant` | `append` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.renewed` | `step-run` | `authorized-participant` | `append` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.submitted` | `step-run` | `authorized-participant` | `once-per-attempt` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |

`resource.observed` validates facts against the resource kind. Custom resource facts remain open.
