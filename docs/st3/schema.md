# st3 schema registry

This file is generated from `st3-schema`.

Schema: `st3.v1`
Digest: `caebc4e9e8575e9ddac9d74e066e6a62281e980e1ab2540e49a6f7eec6fac2ed`

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
| `fleet-invite` | `fleet-invite/ID` | no | A single-use fleet join invite. |
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
| `vcs.repository` | `default_ref:subject-reference`, `head:subject-reference`, `issues:array`, `pull_requests:array`, `repository_id:integer`, `state:string`, `url:string`, `vcs:string` | A version control repository. |
| `custom.NAMESPACE.NAME` | open fact bag | A namespaced custom resource. |

## Claim kinds

| Kind | Subjects | Write policy | Cardinality | Retention | Fields | KDL source |
|---|---|---|---|---|---|---|
| `agent.account` | `agent` | `same-subject-actor` | `state-transition` | `durable` | `account!:subject-reference(account)` |  |
| `agent.presence` | `agent` | `same-subject-actor` | `append` | `durable` | `presence!:string`, `reachability:string`, `reason:string` |  |
| `agent.queue.moved` | `agent` | `authorized-requester` | `append` | `durable` | `anchor:subject-reference(mission-run)`, `placement!:string`, `reason:string`, `run!:subject-reference(mission-run)` |  |
| `attention.requested` | `attention` | `authorized-participant` | `once` | `durable` | `reason!:string`, `reviewer!:subject-reference(person)`, `severity!:string`, `targets:array`, `title!:string`, `until:string` |  |
| `attention.resolved` | `attention` | `authorized-participant` | `once` | `durable` | `outcome!:string`, `reason:string`, `request!:string` |  |
| `daemon.diagnostic` | `daemon` | `system-only` | `append` | `durable` | `code!:string`, `reason!:string`, `severity!:string`, `status:string` |  |
| `daemon.started` | `daemon` | `system-only` | `append` | `durable` | `pid:integer`, `schema:string`, `schema_digest:string`, `status!:string`, `version:string` | `reset` |
| `doc.bound` | `doc` | `authorized-requester` | `append` | `durable` | `executable:boolean`, `hash:string`, `name:string`, `size:integer` | `doc` |
| `eval.verdict` | `mission-run` | `system-only` | `once` | `durable` | `reason:string`, `residue:array`, `verdict!:string` |  |
| `file.observed` | `file` | `system-only` | `append` | `durable` | `blob_hash:string`, `content:string`, `content_hash:string`, `mode:integer`, `path!:string`, `reason:string`, `status!:string` | `gate` |
| `fleet.invite-created` | `fleet-invite` | `system-only` | `append` | `durable` | `created_by:subject-reference(person)`, `expires_at_unix_ms!:integer`, `name:string`, `sponsor!:subject-reference(host)`, `transports:array` |  |
| `fleet.invite-redeemed` | `fleet-invite` | `system-only` | `append` | `durable` | `member_key!:string`, `name!:string` |  |
| `fleet.invite-revoked` | `fleet-invite` | `system-only` | `append` | `durable` | `reason!:string`, `revoked_by:subject-reference(person)` |  |
| `fleet.member-admitted` | `host` | `system-only` | `append` | `durable` | `admitted_by:subject-reference(person)`, `fleet_id!:string`, `invite:subject-reference(fleet-invite)`, `member_key!:string`, `mode!:string`, `sponsor:subject-reference(host)`, `via!:string`, `writer_floor:integer` |  |
| `fleet.member-endpoints` | `host` | `system-only` | `append` | `durable` | `build:string`, `endpoints:array`, `member_key!:string`, `mode!:string` |  |
| `fleet.member-left` | `host` | `system-only` | `append` | `durable` | `high_water!:integer`, `member_key!:string` |  |
| `fleet.member-removed` | `host` | `system-only` | `append` | `durable` | `high_water!:integer`, `member_key:string`, `reason!:string`, `removed_by:subject-reference(person)` |  |
| `gate.requested` | `gate-operation` | `system-only` | `once` | `durable` | `attempt:integer`, `baseline:boolean`, `capability_expires_at:string`, `capability_hash:string`, `decisions:array`, `gate:string`, `mission_revision:string`, `mode:string`, `model:string`, `operation:subject-reference`, `owner:subject-reference`, `question:string`, `review_targets:array`, `reviewer:subject-reference`, `runner:string`, `status:string`, `step_definition:string`, `token_budget:integer`, `tools:array` | `gate` |
| `gate.result` | `gate-operation` | `capability-holder` | `append` | `durable` | `baseline:boolean`, `decision:string`, `field:string`, `gate:string`, `operation:subject-reference`, `reason:string`, `request:string`, `stage:string`, `token_usage:integer`, `value:any`, `verdict!:string` | `gate` |
| `harness.context-clear.requested` | `agent` | `authorized-requester` | `append` | `durable` | `context_epoch:string`, `incarnation_id:string`, `operation_status:string`, `runtime_id:string` |  |
| `harness.context-clear.result` | `agent` | `system-only` | `once` | `durable` | `context_epoch:string`, `incarnation_id:string`, `reason:string`, `result!:string`, `runtime_id:string` |  |
| `harness.diagnostic` | `agent` | `same-subject-actor` | `append` | `durable` | `attempt:integer`, `code:string`, `incarnation_id:string`, `matched_line:string`, `observed_since_ms:integer`, `readiness_epoch:integer`, `reason:string`, `retry_after_unix_ms:integer`, `retry_attempt:integer`, `severity:string`, `status:string`, `step_run:subject-reference(step-run)`, `wake_attempts:integer` |  |
| `harness.observed` | `agent` | `same-subject-actor` | `append` | `latest` | `ask:string`, `blocked_on:string`, `driver:string`, `evidence_incarnation:string`, `exit:string`, `incarnation_id:string`, `input_buffer:string`, `observed_at_ms:integer`, `observed_since_ms:integer`, `ownership_sequence:integer`, `reason:string`, `state!:string`, `transition_sequence:integer`, `transport:string` |  |
| `harness.session-file` | `agent` | `authorized-requester` | `append` | `durable` | `agent:subject-reference(agent)`, `discovery_revision:string`, `harness!:string`, `incarnation_id:string`, `modified_at:string`, `path:string`, `session_id!:string`, `source_session:string`, `status:string` |  |
| `harness.timeline` | `agent` | `same-subject-actor` | `append` | `local` | `body!:object`, `driver!:string`, `entry_id!:string`, `entry_type!:string`, `final!:boolean`, `incarnation_id!:string`, `observed_at_unix_ms:integer`, `operation!:string`, `revision!:integer`, `role!:string`, `sequence:integer` |  |
| `harness.usage` | `agent` | `same-subject-actor` | `append` | `latest` | `cached_tokens:integer`, `compactions:integer`, `context_used_percent:number`, `context_used_tokens:integer`, `context_window_tokens:integer`, `cost:number`, `currency:string`, `driver!:string`, `incarnation_id!:string`, `input_tokens:integer`, `last_compaction_ms:integer`, `last_compaction_trigger:string`, `model:string`, `output_tokens:integer`, `semantics!:string`, `total_tokens:integer` |  |
| `intent.desired` | `*` | `authorized-requester` | `state-transition` | `durable` | `desired:object`, `kind:string`, `revision:string` | `account`, `agent`, `doc`, `exec`, `host`, `message`, `observer`, `mission`, `mission-run`, `planning-session`, `pty`, `resource`, `schedule`, `step`, `stop`, `subscription` |
| `loop.round-dispatch` | `loop-run` | `system-only` | `append` | `durable` | `candidate:integer`, `dispatch!:integer`, `item_id:string`, `mission_run!:subject-reference(mission-run)`, `reason!:string`, `round!:integer`, `status!:string` | `loop`, `round` |
| `loop.round-result` | `loop-run` | `system-only` | `append` | `durable` | `candidate:integer`, `feedback:subject-reference(doc)`, `item:any`, `metrics:object`, `mission_run!:subject-reference(mission-run)`, `reason:string`, `round!:integer`, `status!:string`, `token_usage:integer` | `loop`, `round` |
| `loop.state` | `loop-run` | `system-only` | `state-transition` | `durable` | `best_metrics:object`, `best_round:integer`, `feedback:subject-reference(doc)`, `items:array`, `reason:string`, `round:integer`, `status!:string`, `winner:integer` | `loop` |
| `message.closed` | `message` | `authorized-participant` | `once-per-actor` | `durable` | `status!:string` |  |
| `message.delivered` | `message` | `system-only` | `once-per-actor` | `durable` | `recipient:subject-reference`, `runtime_id:string`, `status!:string`, `transport:string` | `message` |
| `message.read` | `message` | `authorized-participant` | `once-per-actor` | `durable` | `status!:string` |  |
| `message.sent` | `message` | `ordinary-client` | `once` | `durable` | `content:string`, `from:subject-reference`, `in_reply_to:subject-reference`, `session_id:string`, `status!:string`, `tags:array`, `title:string`, `to:subject-reference` | `message` |
| `message.staged` | `message` | `system-only` | `once-per-actor` | `durable` | `recipient:subject-reference`, `runtime_id:string`, `status!:string`, `transport:string` | `message` |
| `mission-run.created` | `mission-run` | `system-only` | `once` | `durable` | `after:subject-reference`, `current_generation:subject-reference`, `deadline_at_unix_ms:integer`, `default_selector:object`, `generation:subject-reference`, `initial_revision:string`, `inputs:object`, `mission:subject-reference`, `mode:string`, `parent_step_run:subject-reference`, `requester:subject-reference`, `revision:string`, `root_mission_run:subject-reference`, `root_revision:string`, `status:string`, `timeout_ms:integer`, `workspace:string` | `mission-run` |
| `mission-run.state` | `mission-run` | `system-only` | `state-transition` | `durable` | `completion:string`, `finally:string`, `phase:string`, `previous_phase:string`, `reason:string`, `status:string` | `mission-run`, `completion`, `finally`, `cancellation` |
| `mission.produced` | `mission`, `step-run` | `capability-holder` | `append` | `durable` | `attempt:integer`, `mission:subject-reference`, `name:string`, `revision:string`, `step_definition:string` | `produces` |
| `mission.published` | `mission` | `authorized-requester` | `append` | `durable` | `body:object`, `revision:string`, `state:string` | `mission` |
| `observer.observed` | `observer` | `system-only` | `append` | `durable` | `attempt:string`, `changed:boolean`, `changed_fields:array`, `cursor:string`, `locator:string`, `next_check_unix_ms:string`, `observation:subject-reference`, `provider:string`, `resource:subject-reference`, `revision:string`, `status:string` | `observer` |
| `observer.refresh-requested` | `observer` | `system-only` | `append` | `durable` | `attempt!:string`, `revision!:string` | `refresh` |
| `observer.state` | `observer` | `system-only` | `state-transition` | `durable` | `attempt:string`, `error_code:string`, `next_check_unix_ms:string`, `reason:string`, `revision:string`, `state!:string` | `observer` |
| `planning-session.approved` | `planning-session` | `authorized-requester` | `once` | `durable` | `candidate_revision:integer`, `kdl:subject-reference`, `markdown:subject-reference`, `mission_revision:string`, `preview_hash:string`, `preview_token:string`, `requester:subject-reference`, `variant:string` |  |
| `planning-session.cancelled` | `planning-session` | `authorized-requester` | `once` | `durable` | `reason:string`, `requester:subject-reference` | `cancellation` |
| `planning-session.candidate-submitted` | `planning-session` | `authorized-participant` | `append` | `durable` | `candidate_revision:integer`, `kdl:subject-reference`, `markdown:subject-reference`, `mission_revision:string`, `revision:integer`, `variant:string` |  |
| `planning-session.previewed` | `planning-session` | `system-only` | `append` | `durable` | `candidate_revision:integer`, `diff:string`, `graph:string`, `mission:object`, `preview_hash:string`, `store_index:integer`, `variant:string` |  |
| `planning-session.question-answered` | `planning-session` | `authorized-requester` | `append` | `durable` | `decision_id:string`, `expected_revision:integer`, `explanation:string`, `requester:subject-reference`, `response:object` |  |
| `planning-session.question-requested` | `planning-session` | `authorized-participant` | `append` | `durable` | `decision_id:string`, `decision_type!:string`, `options:array`, `planner:subject-reference`, `question:string`, `requester:subject-reference`, `revision:integer` |  |
| `planning-session.revision-requested` | `planning-session` | `authorized-requester` | `append` | `durable` | `candidate_revision:integer`, `feedback:subject-reference`, `requester:subject-reference`, `variant:string` | `feedback` |
| `planning-session.started` | `planning-session` | `authorized-requester` | `once` | `durable` | `mission:subject-reference`, `planner:subject-reference`, `planner_config:object`, `request:subject-reference`, `requester:subject-reference`, `target_generation:subject-reference`, `target_run:subject-reference`, `workspace:string` | `planning-session` |
| `publication.operation` | `*` | `system-only` | `append` | `durable` | `action:string`, `operation:string`, `status!:string` | `revision`, `reset`, `cancellation`, `refresh`, `feedback` |
| `reconcile.fault` | `daemon`, `mission-run`, `observer`, `schedule`, `step-run`, `subscription` | `system-only` | `append` | `durable` | `reason:string`, `scope!:string`, `status!:string` |  |
| `record.repaired` | `repair` | `ordinary-client` | `once` | `durable` | `reason!:string`, `record!:string`, `replacement!:string` | `repair` |
| `render.applied` | `agent`, `exec`, `pty` | `system-only` | `append` | `durable` | `warnings:array`, `writes:array` |  |
| `repair.applied` | `repair` | `system-only` | `once` | `durable` | `affected_subjects:array`, `item_count:integer`, `reason!:string`, `token!:string` |  |
| `resource.observed` | `resource` | `ordinary-client` | `append` | `durable` | `kind:string`, `observed_at:integer`, `state:any` | `resource` |
| `revision-proposal.applied` | `revision-proposal` | `system-only` | `once` | `durable` | `reason:string`, `status:string`, `successor_generation:subject-reference` |  |
| `revision-proposal.approved` | `revision-proposal` | `authorized-requester` | `once-per-actor` | `durable` | `all_approved:boolean`, `preview_hash:string`, `reviewer:subject-reference` |  |
| `revision-proposal.cancelled` | `revision-proposal` | `authorized-requester` | `once` | `durable` | `reason:string`, `status:string` |  |
| `revision-proposal.created` | `revision-proposal` | `authorized-requester` | `once` | `durable` | `candidate_revision:string`, `compatible_steps:array`, `cutover:string`, `preview_hash:string`, `reason:string`, `reviewers:array`, `run:subject-reference`, `source_generation:subject-reference`, `status:string` |  |
| `run-generation.created` | `run-generation` | `system-only` | `once` | `durable` | `compatible_steps:array`, `predecessor:subject-reference`, `reason:string`, `revision:string`, `run:subject-reference`, `status:string` | `mission-run`, `revision` |
| `run-generation.state` | `run-generation` | `system-only` | `state-transition` | `durable` | `phase:string`, `previous_phase:string`, `reason:string`, `status:string`, `successor:subject-reference` | `mission-run`, `step`, `completion`, `finally`, `revision`, `cancellation` |
| `run-generation.superseded` | `run-generation` | `system-only` | `once` | `durable` | `phase:string`, `previous_phase:string`, `reason:string`, `status:string`, `successor:subject-reference` | `revision` |
| `runtime.action.deadline-reached` | `agent`, `exec`, `pty`, `gate-operation` | `system-only` | `append` | `durable` | `action:string`, `deadline_key:string`, `desired_token:string`, `incarnation_id:string`, `operation:string`, `operation_status:string`, `reason:string`, `runtime_id:string`, `signal:string`, `terminal:boolean` | `stop`, `gate` |
| `runtime.action.failed` | `agent`, `exec`, `pty`, `gate-operation` | `system-only` | `append` | `durable` | `action:string`, `deadline_key:string`, `desired_token:string`, `incarnation_id:string`, `operation:string`, `operation_status:string`, `reason:string`, `runtime_id:string`, `signal:string`, `terminal:boolean` | `stop`, `gate` |
| `runtime.action.requested` | `agent`, `exec`, `pty`, `gate-operation` | `authorized-requester` | `append` | `durable` | `action:string`, `deadline_unix_ms:string`, `incarnation_id:string`, `operation:string`, `runtime_id:string`, `signal:string`, `terminal:boolean` | `stop`, `gate` |
| `runtime.action.succeeded` | `agent`, `exec`, `pty`, `gate-operation` | `system-only` | `append` | `durable` | `action:string`, `deadline_key:string`, `desired_token:string`, `incarnation_id:string`, `operation:string`, `operation_status:string`, `reason:string`, `runtime_id:string`, `signal:string`, `terminal:boolean` | `stop`, `gate` |
| `runtime.observed` | `agent`, `exec`, `pty`, `gate-operation` | `same-subject-actor` | `append` | `durable` | `adopted:boolean`, `driver:string`, `exit_code:integer`, `exit_signal:integer`, `host:string`, `incarnation_id:string`, `reachability:string`, `reason:string`, `runtime_id:string`, `shutdown_timeout_ms:integer`, `status:string`, `terminal:boolean` |  |
| `runtime.readiness-deadline-reached` | `agent` | `system-only` | `append` | `durable` | `deadline_unix_ms!:string`, `driver!:string`, `incarnation_id!:string`, `reason!:string`, `runtime_id!:string` |  |
| `runtime.reconcile-decision` | `agent`, `exec`, `pty`, `schedule` | `system-only` | `append` | `durable` | `decision:string`, `gate:string`, `input_number:integer`, `key:string`, `reachability:string`, `reason:string`, `restart_at_unix_ms:string` |  |
| `runtime.restart-window-reset` | `agent`, `exec`, `pty` | `system-only` | `append` | `durable` | `desired_token:string`, `incarnation_id!:string`, `reason!:string` | `reset` |
| `schedule.occurrence-cancelled` | `schedule` | `system-only` | `append` | `durable` | `occurrence:integer`, `reason:string`, `revision:string` | `schedule` |
| `schedule.occurrence-reached` | `schedule` | `system-only` | `append` | `durable` | `at_unix_ms:integer`, `occurrence:integer`, `revision:string`, `scheduled:subject-reference`, `scheduled_at_unix_ms:string` | `schedule` |
| `schedule.occurrence-scheduled` | `schedule` | `system-only` | `append` | `durable` | `at_unix_ms:integer`, `occurrence:integer`, `revision:string`, `scheduled_at_unix_ms:string` | `schedule` |
| `schedule.work-failed` | `schedule` | `system-only` | `append` | `durable` | `code!:string`, `reason!:string`, `request!:string` | `schedule` |
| `schedule.work-requested` | `schedule` | `system-only` | `append` | `durable` | `inputs!:object`, `mission!:subject-reference(mission)`, `mission_revision!:string`, `occurrence!:integer`, `revision!:string`, `workspace!:string` | `schedule` |
| `schedule.work-started` | `schedule` | `system-only` | `append` | `durable` | `mission_run!:subject-reference(mission-run)`, `request!:string` | `schedule` |
| `step-run.carried` | `step-run` | `system-only` | `once` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `definition_hash:string`, `source:subject-reference`, `source_generation:subject-reference`, `source_step_run:subject-reference`, `status:string`, `worker_reported:boolean` | `step` |
| `step-run.retried` | `step-run` | `system-only` | `append` | `durable` | `attempt:integer`, `goals:array`, `not_before_unix_ms:integer`, `reason:string`, `status:string` | `step` |
| `step-run.state` | `step-run` | `system-only` | `state-transition` | `durable` | `attempt:integer`, `readiness_epoch:integer`, `reason:string`, `status:string` | `step` |
| `subscription.mission-deferred` | `subscription` | `system-only` | `append` | `durable` | `not_before_unix_ms!:integer`, `request!:string` | `subscription` |
| `subscription.mission-failed` | `subscription` | `system-only` | `append` | `durable` | `code!:string`, `reason!:string`, `request!:string` | `subscription` |
| `subscription.mission-request-cancelled` | `subscription` | `authorized-participant` | `append` | `durable` | `reason:string`, `request!:string` | `subscription` |
| `subscription.mission-request-released` | `subscription` | `authorized-participant` | `append` | `durable` | `reason:string`, `request!:string` | `subscription` |
| `subscription.mission-requested` | `subscription` | `system-only` | `append` | `durable` | `discovery!:string`, `held:boolean`, `mission!:subject-reference(mission)`, `mission_revision!:string`, `requester:subject-reference(agent|person)`, `resource!:subject-reference(resource)`, `resource_input!:string`, `workspace!:string` | `subscription` |
| `subscription.mission-started` | `subscription` | `system-only` | `append` | `durable` | `mission_run!:subject-reference(mission-run)`, `request!:string` | `subscription` |
| `subscription.state` | `subscription` | `system-only` | `state-transition` | `durable` | `fields:array`, `observer:subject-reference`, `reason:string`, `state!:string`, `to:subject-reference` | `subscription` |
| `terminal.input.requested` | `agent`, `pty` | `authorized-requester` | `append` | `durable` | `byte_count:integer`, `incarnation_id:string`, `intent:string`, `mode:string`, `runtime_id:string`, `sequence:integer`, `sha256:string` |  |
| `terminal.input.result` | `agent`, `pty` | `system-only` | `append` | `durable` | `incarnation_id:string`, `reason:string`, `result!:string`, `runtime_id:string`, `sequence:integer` |  |
| `transport.observed` | `host` | `system-only` | `append` | `durable` | `last_success_at:integer`, `protocol:string`, `reason:string`, `remote_heads:object`, `status!:string` |  |
| `work.claimed` | `step-run` | `authorized-participant` | `state-transition` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.failed` | `step-run` | `authorized-participant` | `once-per-attempt` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.progress` | `step-run` | `authorized-participant` | `append` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.released` | `step-run` | `authorized-participant` | `append` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.renewed` | `step-run` | `authorized-participant` | `append` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.submitted` | `step-run` | `authorized-participant` | `once-per-attempt` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |

`resource.observed` validates facts against the resource kind. Custom resource facts remain open.

A `durable` claim is a fact in the replicated claim log. A `local` claim is an observation kept only in the local observation log of the node that made it, trimmed after that node's retention window. A `latest` claim is an observation kept in that log whose replicated claims are written only when its state changes; each one replaces the previous one for its subject.
