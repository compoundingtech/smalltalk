# st3 schema registry

This file is generated from `st3-schema`.

Schema: `st3.v1`
Digest: `ab9dc249b1e6679d1510a0ab122265ef17fc02ce6d0da95e31e28199f8589651`
Storage version: `18`
Storage digest: `d11a3e5db57db0a0bd6a9b27474cdd93958a401b3a6482b62aea3595530dbcef`

## Subject families

| Family | Pattern | Client writable | Description |
|---|---|---:|---|
| `account` | `account/NAME` | no | A model account: its provider, owner, plan and where its login lives. |
| `agent` | `agent/RUN/LOCAL_ID` | no | A mission-run agent runtime. |
| `arrangement` | `arrangement/person/NAME/UUIDv7` | no | A permanently person-owned shared folder arrangement. |
| `attention` | `attention/ID` | yes | An explicit request for human attention. |
| `checkpoint` | `checkpoint/DAY` | no | A checkpoint that trims replicated history dated before a UTC day. |
| `checkpoint-excusal` | `checkpoint-excusal/ID` | no | A person's excusal of an unreachable writer from checkpoints. |
| `custom` | `custom/NAMESPACE/NAME` | yes | An extension subject. |
| `daemon` | `daemon/NODE` | no | An st3 daemon. |
| `doc` | `doc/NAME` | no | A named immutable document lineage. |
| `exec` | `exec/RUN/LOCAL_ID` | no | A mission-run exec runtime. |
| `external` | `external/PROVIDER/KIND/IDENTITY` | no | An external account or actor, distinct from a native person. |
| `file` | `file/HOST:/ABSOLUTE_PATH` | no | A read-only file gate target. |
| `fleet-invite` | `fleet-invite/ID` | no | A single-use fleet join invite. |
| `gate-operation` | `gate-operation/IDENTITY` | no | One gate evaluation attempt. |
| `github-post` | `github-post/OWNER/REPO/KIND/ID` | no | A GitHub comment or review an agent seat posted, by its GitHub ID. |
| `glass` | `glass/person/NAME/UUID` | no | A private person workspace. |
| `host` | `host/NAME` | no | A graph host. |
| `lane` | `lane/RUN/LOCAL_ID` | no | A mission-run lane: an ordered line of entries its run works through front first. |
| `loop-run` | `loop-run/GENERATION/PATH` | no | One bounded loop execution. |
| `message` | `message/ID` | yes | A Small Talk message. |
| `mission` | `mission/ID` | no | An immutable mission revision lineage. |
| `mission-run` | `mission-run/ID` | no | A mission execution. |
| `observer` | `observer/RUN/LOCAL_ID` | no | A mission-run resource observer. |
| `owned-set` | `owned-set/NAME` | no | A graph-owned set of declarations with source ordering and omission retirement. |
| `person` | `person/IDENTITY` | no | A human actor. |
| `planning-session` | `planning-session/ID` | no | A durable planning session. |
| `pty` | `pty/RUN/LOCAL_ID` | no | A mission-run terminal runtime. |
| `repair` | `repair/ID` | yes | An immutable receipt for a bounded graph or replication repair. |
| `resource` | `resource/NAME` | yes | An observed external or durable fact bag. |
| `revision-proposal` | `revision-proposal/ID` | no | A mission revision proposal. |
| `rule` | `rule/NAME` | no | A permission rule, its mode, and the writes it audited. |
| `run-generation` | `run-generation/ID` | no | An immutable mission-run generation. |
| `schedule` | `schedule/RUN/LOCAL_ID` | no | A mission-run schedule. |
| `sekret` | `sekret/HOST or sekret/HOST/OWNER/NAME` | no | A host's sekrets gateway, or one of its profiles: calls run with a credential the caller never reads. |
| `step-run` | `step-run/GENERATION/PATH` | no | One step attempt lineage. |
| `subscription` | `subscription/RUN/LOCAL_ID` | no | A mission-run observer subscription. |

Custom subjects use `custom/NAMESPACE/NAME`. Custom claims use `custom.NAMESPACE.NAME`.

## Resource kinds

| Kind | Facts | Description |
|---|---|---|
| `arrangement` | `body:object`, `owner!:subject-reference(person) immutable` | A person-owned per-register arrangement. |
| `ci.run` | `commit:subject-reference`, `completed_at:string`, `conclusion:string`, `external_id:string`, `name:string`, `provider:string`, `pull_request:subject-reference`, `repository:subject-reference`, `started_at:string`, `status:string`, `url:string` | A continuous integration run. |
| `filesystem.file` | `content_hash:string`, `mode:integer`, `path:string immutable`, `reason:string`, `size:integer`, `status:string` | A file observed through an explicit local path. |
| `harness.session-file` | `agent:subject-reference`, `harness:string immutable`, `incarnation_id:string`, `modified_at:string`, `path:string`, `session_id:string`, `status:string` | A harness session file that can outlive one runtime incarnation. |
| `human.review` | `decision:string`, `document:string`, `reason:string`, `reviewer:subject-reference`, `submitted_at:string`, `target:subject-reference` | A human review of another graph subject. |
| `vcs.commit` | `author:string`, `committed_at:string`, `committer:string`, `message:string`, `parents:array`, `repository:subject-reference immutable`, `sha:string immutable`, `state:string`, `tree:string immutable`, `url:string` | An immutable version control commit. |
| `vcs.issue` | `author:string`, `closed_by:string`, `closed_by_resource:subject-reference`, `comments:integer`, `created_at:string`, `labels:array`, `last_comment:object`, `mentions:array`, `moved_to:subject-reference`, `node_id:string`, `number:integer`, `opened_by:subject-reference`, `opened_by_run:subject-reference`, `reactions:object`, `recent_comments:array`, `repository:subject-reference`, `state:string`, `state_reason:string`, `title:string`, `updated_at:string`, `url:string` | A version control issue. |
| `vcs.pull-request` | `author:string`, `base:subject-reference`, `base_branch:string`, `branch:string`, `checks:array`, `checks_state:string`, `closed_by_resource:subject-reference`, `comments:integer`, `created_at:string`, `draft:boolean`, `head:subject-reference`, `head_sha:string`, `last_comment:object`, `mentions:array`, `merge_commit_sha:string`, `merge_queue:object`, `merged:boolean`, `merged_at:string`, `merged_by:string`, `moved_to:subject-reference`, `node_id:string`, `number:integer`, `opened_by:subject-reference`, `opened_by_run:subject-reference`, `reactions:object`, `recent_comments:array`, `repository:subject-reference`, `required_checks:object`, `review_decision:string`, `reviews:array`, `state:string`, `state_reason:string`, `title:string`, `updated_at:string`, `url:string` | A version control pull request. |
| `vcs.ref` | `ancestors:array`, `head:string`, `name:string immutable`, `ref_type:string`, `repository:subject-reference immutable`, `target:subject-reference`, `url:string` | A named version control reference. |
| `vcs.repository` | `default_ref:subject-reference`, `github_http_requests_since_start:integer`, `head:subject-reference`, `issues:array`, `main_performance_failures:array`, `moved_to:subject-reference`, `node_id:string`, `pull_requests:array`, `repository_id:integer`, `state:string`, `url:string`, `vcs:string` | A version control repository. |
| `custom.NAMESPACE.NAME` | open fact bag | A namespaced custom resource. |

## Claim kinds

| Kind | Subjects | Write policy | Cardinality | Retention | Fields | KDL source |
|---|---|---|---|---|---|---|
| `agent.account` | `agent` | `same-subject-actor` | `state-transition` | `durable` | `account!:subject-reference(account)` |  |
| `agent.placement.source-offline` | `agent` | `authorized-requester` | `append` | `durable` | `desired_token!:string`, `destination!:string`, `sources!:array` |  |
| `agent.presence` | `agent` | `same-subject-actor` | `append` | `durable` | `presence!:string`, `reachability:string`, `reason:string` |  |
| `agent.queue.moved` | `agent` | `authorized-requester` | `append` | `durable` | `anchor:subject-reference(mission-run)`, `placement!:string`, `reason:string`, `run!:subject-reference(mission-run)` |  |
| `arrangement.edited` | `arrangement` | `ordinary-client` | `append` | `durable` | `action_digest:string`, `action_id:string`, `operations!:array`, `owner!:subject-reference(person)` |  |
| `attention.requested` | `attention` | `authorized-participant` | `once` | `durable` | `closed_by:string`, `reason!:string`, `reviewer!:subject-reference(person)`, `severity!:string`, `step:subject-reference(step-run)`, `step_attempt:integer`, `targets:array`, `title!:string`, `until:string` |  |
| `attention.resolved` | `attention` | `authorized-participant` | `once` | `durable` | `outcome!:string`, `reason:string`, `request!:string` |  |
| `checkpoint.excused` | `checkpoint-excusal` | `system-only` | `append` | `durable` | `reason!:string`, `writer!:string` |  |
| `checkpoint.sealed` | `checkpoint` | `system-only` | `append` | `durable` | `build:string`, `checkpoint_protocol!:integer`, `cut_unix_ms!:integer`, `participants:array`, `rules_digest!:string`, `sealed_count!:integer`, `sealed_digest!:string` |  |
| `checkpoint.verified` | `checkpoint` | `system-only` | `append` | `durable` | `build:string`, `checkpoint_protocol!:integer`, `cut_unix_ms!:integer`, `drop_digest!:string`, `dropped_claims!:integer`, `dropped_envelopes!:integer`, `graph_digest!:string`, `participants:array`, `reader_digest!:string`, `retained_digest!:string`, `rules_digest!:string`, `sealed_digest!:string` |  |
| `daemon.diagnostic` | `daemon` | `system-only` | `append` | `durable` | `code!:string`, `reason!:string`, `severity!:string`, `status:string` |  |
| `daemon.started` | `daemon` | `system-only` | `append` | `durable` | `features:object`, `pid:integer`, `schema:string`, `schema_digest:string`, `status!:string`, `version:string` | `reset` |
| `delivery.hold` | `agent` | `authorized-requester` | `state-transition` | `durable` | `held!:boolean`, `legacy_adoption:boolean`, `reason!:string`, `until_unix_ms!:integer` |  |
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
| `gate.result` | `gate-operation` | `capability-holder` | `append` | `durable` | `acted_for:subject-reference(person)`, `baseline:boolean`, `decision:string`, `delegation:object`, `field:string`, `gate:string`, `operation:subject-reference`, `reason:string`, `request:string`, `stage:string`, `token_usage:integer`, `value:any`, `verdict!:string` | `gate` |
| `github.posted` | `github-post` | `system-only` | `once` | `durable` | `agent!:subject-reference(agent)`, `id!:integer`, `item!:integer`, `kind!:string`, `login:string`, `repository!:string`, `url:string` |  |
| `glass.deleted` | `glass` | `authorized-requester` | `append` | `durable` | `base_revision:string`, `replaced_revision:string` |  |
| `glass.upserted` | `glass` | `authorized-requester` | `append` | `durable` | `base_revision:string`, `body:object`, `replaced_revision:string` |  |
| `harness.context-clear.requested` | `agent` | `authorized-requester` | `append` | `durable` | `context_epoch:string`, `incarnation_id:string`, `operation_status:string`, `runtime_id:string` |  |
| `harness.context-clear.result` | `agent` | `system-only` | `once` | `durable` | `context_epoch:string`, `incarnation_id:string`, `reason:string`, `result!:string`, `runtime_id:string` |  |
| `harness.diagnostic` | `agent` | `same-subject-actor` | `append` | `durable` | `attempt:integer`, `auth_attention_key:string`, `code:string`, `driver:string`, `incarnation_id:string`, `matched_line:string`, `observed_since_ms:integer`, `ownership_sequence:integer`, `provider_auth_sequence:integer`, `readiness_epoch:integer`, `reason:string`, `retry_after_unix_ms:integer`, `retry_attempt:integer`, `selector_scope:string`, `session_id:string`, `severity:string`, `status:string`, `step_run:subject-reference(step-run)`, `wake_attempts:integer` |  |
| `harness.limits` | `agent` | `same-subject-actor` | `append` | `durable` | `account:string`, `account_ref:string`, `driver!:string`, `five_hour_percent:number`, `five_hour_resets_at_unix_ms:integer`, `incarnation_id:string`, `measured_at_unix_ms!:integer`, `plan:string`, `weekly_percent:number`, `weekly_resets_at_unix_ms:integer` |  |
| `harness.observed` | `agent` | `same-subject-actor` | `append` | `latest` | `ask:string`, `background_jobs:integer`, `blocked_on:string`, `blocking:array`, `driver:string`, `evidence_incarnation:string`, `exit:string`, `incarnation_id:string`, `input_buffer:string`, `observed_at_ms:integer`, `observed_since_ms:integer`, `ownership_sequence:integer`, `provider_auth:boolean`, `provider_auth_sequence:integer`, `quiescent:boolean`, `reason:string`, `rollout_operation:string`, `state!:string`, `status_transition:boolean`, `transition_sequence:integer`, `transport:string` |  |
| `harness.session-file` | `agent` | `authorized-requester` | `append` | `durable` | `account_ref:string`, `agent:subject-reference(agent)`, `discovery_revision:string`, `harness!:string`, `incarnation_id:string`, `modified_at:string`, `path:string`, `session_id!:string`, `source_session:string`, `status:string` |  |
| `harness.telemetry` | `agent` | `same-subject-actor` | `append` | `local` | `driver!:string`, `incarnation_id!:string`, `signals!:object`, `unit!:string` |  |
| `harness.timeline` | `agent` | `same-subject-actor` | `append` | `local` | `body!:object`, `driver!:string`, `entry_id!:string`, `entry_type!:string`, `final!:boolean`, `incarnation_id!:string`, `observed_at_unix_ms:integer`, `operation!:string`, `revision!:integer`, `role!:string`, `sequence:integer`, `source_id:string` |  |
| `harness.todo.observed` | `agent` | `same-subject-actor` | `append` | `latest` | `harness!:string`, `incarnation_id!:string`, `observed_at!:string`, `phases!:array`, `session_id!:string`, `source_op!:string`, `totals!:object`, `truncated!:boolean` |  |
| `harness.usage` | `agent` | `same-subject-actor` | `append` | `latest` | `account:string`, `cache_write_1h_tokens:integer`, `cache_write_tokens:integer`, `cached_tokens:integer`, `compactions:integer`, `context_used_percent:number`, `context_used_tokens:integer`, `context_window_tokens:integer`, `cost:number`, `cost_microusd:integer`, `currency:string`, `driver!:string`, `host:string`, `incarnation_id!:string`, `input_tokens:integer`, `last_compaction_ms:integer`, `last_compaction_trigger:string`, `model:string`, `native_session_id:string`, `observed_at_unix_ms:integer`, `output_tokens:integer`, `owner_run:string`, `owner_step:string`, `pricing:string`, `pricing_provenance:array`, `reported_cost_microusd:integer`, `semantics!:string`, `total_tokens:integer`, `unpriced_tokens:integer` |  |
| `intent.desired` | `*` | `authorized-requester` | `state-transition` | `durable` | `desired:object`, `kind:string`, `revision:string` | `account`, `agent`, `doc`, `exec`, `host`, `lane`, `message`, `observer`, `mission`, `mission-run`, `planning-session`, `pty`, `resource`, `schedule`, `step`, `stop`, `subscription` |
| `lane.approved` | `lane` | `authorized-participant` | `append` | `durable` | `entry!:subject-reference`, `reason:string` |  |
| `lane.joined` | `lane` | `authorized-participant` | `append` | `durable` | `entry!:subject-reference`, `reason:string` |  |
| `lane.left` | `lane` | `authorized-participant` | `append` | `durable` | `entry!:subject-reference`, `outcome!:string`, `reason:string` |  |
| `lane.marked` | `lane` | `authorized-participant` | `append` | `durable` | `detail:string`, `entry!:subject-reference`, `head:string`, `state!:string` |  |
| `lane.moved` | `lane` | `authorized-participant` | `append` | `durable` | `anchor:subject-reference`, `entry!:subject-reference`, `placement!:string`, `reason:string` |  |
| `loop.round-dispatch` | `loop-run` | `system-only` | `append` | `durable` | `candidate:integer`, `dispatch!:integer`, `item_id:string`, `mission_run!:subject-reference(mission-run)`, `reason!:string`, `round!:integer`, `status!:string` | `loop`, `round` |
| `loop.round-result` | `loop-run` | `system-only` | `append` | `durable` | `candidate:integer`, `feedback:subject-reference(doc)`, `item:any`, `metrics:object`, `mission_run!:subject-reference(mission-run)`, `reason:string`, `round!:integer`, `status!:string`, `token_usage:integer` | `loop`, `round` |
| `loop.state` | `loop-run` | `system-only` | `state-transition` | `durable` | `best_metrics:object`, `best_round:integer`, `feedback:subject-reference(doc)`, `items:array`, `reason:string`, `round:integer`, `status!:string`, `winner:integer` | `loop` |
| `message.closed` | `message` | `authorized-participant` | `once-per-actor` | `durable` | `acted_for:subject-reference(person)`, `delegation:object`, `status!:string` |  |
| `message.delivered` | `message` | `system-only` | `once-per-actor` | `durable` | `recipient:subject-reference`, `runtime_id:string`, `status!:string`, `transport:string` | `message` |
| `message.read` | `message` | `authorized-participant` | `once-per-actor` | `durable` | `status!:string` |  |
| `message.sent` | `message` | `ordinary-client` | `once` | `durable` | `attachments:array`, `content:string`, `from:subject-reference`, `in_reply_to:subject-reference`, `session_id:string`, `status!:string`, `tags:array`, `title:string`, `to:subject-reference` | `message` |
| `message.staged` | `message` | `system-only` | `once-per-actor` | `durable` | `recipient:subject-reference`, `runtime_id:string`, `status!:string`, `transport:string` | `message` |
| `mission-run.created` | `mission-run` | `system-only` | `once` | `durable` | `ad_hoc_title:string`, `after:subject-reference`, `current_generation:subject-reference`, `deadline_at_unix_ms:integer`, `default_selector:object`, `generation:subject-reference`, `initial_revision:string`, `inputs:object`, `mission:subject-reference`, `mission_spec:object`, `mode:string`, `parent_step_run:subject-reference`, `report_completed:boolean`, `report_to:subject-reference`, `requester:subject-reference`, `revision:string`, `root_mission_run:subject-reference`, `root_revision:string`, `stalled_after_ms:integer`, `status:string`, `timeout_ms:integer`, `workspace:string` | `mission-run` |
| `mission-run.report-to` | `mission-run` | `system-only` | `state-transition` | `durable` | `report_completed:boolean`, `report_to:subject-reference`, `stalled_after_ms:integer` |  |
| `mission-run.state` | `mission-run` | `system-only` | `state-transition` | `durable` | `completion:string`, `finally:string`, `phase:string`, `previous_phase:string`, `reason:string`, `status:string` | `mission-run`, `completion`, `finally`, `cancellation` |
| `mission.produced` | `mission`, `step-run` | `capability-holder` | `append` | `durable` | `attempt:integer`, `mission:subject-reference`, `name:string`, `revision:string`, `step_definition:string` | `produces` |
| `mission.provenance` | `mission` | `system-only` | `once` | `durable` | `mission!:subject-reference(mission)`, `provenance!:object`, `revision!:string` | `provenance` |
| `mission.published` | `mission` | `authorized-requester` | `append` | `durable` | `body:object`, `revision:string`, `state:string` | `mission` |
| `observer.observed` | `observer` | `system-only` | `append` | `durable` | `attempt:string`, `changed:boolean`, `changed_fields:array`, `cursor:string`, `locator:string`, `next_check_unix_ms:string`, `observation:subject-reference`, `provider:string`, `resource:subject-reference`, `revision:string`, `status:string` | `observer` |
| `observer.refresh-requested` | `observer` | `system-only` | `append` | `durable` | `attempt!:string`, `revision!:string` | `refresh` |
| `observer.state` | `observer` | `system-only` | `state-transition` | `durable` | `attempt:string`, `error_code:string`, `next_check_unix_ms:string`, `reason:string`, `revision:string`, `state!:string` | `observer` |
| `operational.failure` | `agent`, `exec`, `pty`, `observer`, `subscription`, `schedule`, `daemon`, `machine`, `step-run`, `mission-run`, `loop-run`, `resource`, `checkpoint` | `system-only` | `append` | `durable` | `condition:string`, `episode:string`, `incarnation:string`, `reason:string`, `reviewer:subject-reference`, `severity:string`, `source_revision:string`, `targets:array`, `title:string` |  |
| `operational.recovered` | `agent`, `exec`, `pty`, `observer`, `subscription`, `schedule`, `daemon`, `machine`, `step-run`, `mission-run`, `loop-run`, `resource`, `checkpoint` | `system-only` | `append` | `durable` | `episode:string`, `failure:string`, `reason:string` |  |
| `owned-set.revised` | `owned-set` | `system-only` | `append` | `durable` | `body!:object`, `revision!:string` |  |
| `person.delegation-set` | `person` | `same-subject-actor` | `state-transition` | `durable` | `actions!:array` |  |
| `person.directive-note-set` | `person` | `same-subject-actor` | `state-transition` | `durable` | `expires_at:string`, `text!:string or null` |  |
| `planning-session.approved` | `planning-session` | `authorized-requester` | `once` | `durable` | `candidate_revision:integer`, `kdl:subject-reference`, `markdown:subject-reference`, `mission_revision:string`, `preview_hash:string`, `preview_token:string`, `requester:subject-reference`, `variant:string` |  |
| `planning-session.cancelled` | `planning-session` | `authorized-requester` | `once` | `durable` | `reason:string`, `requester:subject-reference` | `cancellation` |
| `planning-session.candidate-submitted` | `planning-session` | `authorized-participant` | `append` | `durable` | `candidate_revision:integer`, `kdl:subject-reference`, `markdown:subject-reference`, `mission_revision:string`, `revision:integer`, `variant:string` |  |
| `planning-session.previewed` | `planning-session` | `system-only` | `append` | `durable` | `candidate_revision:integer`, `diff:string`, `graph:string`, `mission:object`, `preview_hash:string`, `store_index:integer`, `variant:string` |  |
| `planning-session.question-answered` | `planning-session` | `authorized-requester` | `append` | `durable` | `decision_id:string`, `expected_revision:integer`, `explanation:string`, `requester:subject-reference`, `response:object` |  |
| `planning-session.question-requested` | `planning-session` | `authorized-participant` | `append` | `durable` | `decision_id:string`, `decision_type!:string`, `options:array`, `planner:subject-reference`, `question:string`, `requester:subject-reference`, `revision:integer` |  |
| `planning-session.revision-requested` | `planning-session` | `authorized-requester` | `append` | `durable` | `candidate_revision:integer`, `feedback:subject-reference`, `requester:subject-reference`, `variant:string` | `feedback` |
| `planning-session.started` | `planning-session` | `authorized-requester` | `once` | `durable` | `mission:subject-reference`, `planner:subject-reference`, `planner_config:object`, `request:subject-reference`, `requester:subject-reference`, `target_generation:subject-reference`, `target_run:subject-reference`, `workspace:string` | `planning-session` |
| `principal.key-granted` | `person`, `agent` | `system-only` | `append` | `durable` | `issuer!:string`, `issuer_key!:string`, `key!:string`, `label:string`, `role!:string` |  |
| `principal.key-revoked` | `person`, `agent` | `system-only` | `append` | `durable` | `key!:string`, `reason:string` |  |
| `publication.operation` | `*` | `system-only` | `append` | `durable` | `action:string`, `operation:string`, `status!:string` | `revision`, `reset`, `cancellation`, `refresh`, `feedback` |
| `reconcile.fault` | `daemon`, `mission-run`, `observer`, `schedule`, `step-run`, `subscription` | `system-only` | `append` | `durable` | `reason:string`, `scope!:string`, `status!:string` |  |
| `record.repaired` | `repair` | `ordinary-client` | `once` | `durable` | `reason!:string`, `record!:string`, `replacement!:string` | `repair` |
| `render.applied` | `agent`, `exec`, `pty` | `system-only` | `append` | `local` | `warnings:array`, `writes:array` |  |
| `repair.applied` | `repair` | `system-only` | `once` | `durable` | `affected_subjects:array`, `item_count:integer`, `reason!:string`, `token!:string` |  |
| `resource.observed` | `resource` | `ordinary-client` | `append` | `durable` | `attribution_only:boolean`, `kind:string`, `observed_at:integer`, `state:any` | `resource` |
| `revision-proposal.applied` | `revision-proposal` | `system-only` | `once` | `durable` | `reason:string`, `status:string`, `successor_generation:subject-reference` |  |
| `revision-proposal.approved` | `revision-proposal` | `authorized-requester` | `once-per-actor` | `durable` | `all_approved:boolean`, `preview_hash:string`, `reviewer:subject-reference` |  |
| `revision-proposal.cancelled` | `revision-proposal` | `authorized-requester` | `once` | `durable` | `reason:string`, `status:string` |  |
| `revision-proposal.created` | `revision-proposal` | `authorized-requester` | `once` | `durable` | `candidate_revision:string`, `compatible_steps:array`, `cutover:string`, `preview_hash:string`, `reason:string`, `reviewers:array`, `run:subject-reference`, `source_generation:subject-reference`, `status:string` |  |
| `rule.audited` | `rule` | `system-only` | `append` | `durable` | `action!:string`, `actor!:string`, `rule!:string`, `target!:string` |  |
| `rule.set` | `rule` | `system-only` | `append` | `durable` | `actors:array`, `description:string`, `except:array`, `kinds:array`, `mode!:string`, `subjects:array`, `unless_subjects:array` |  |
| `run-generation.created` | `run-generation` | `system-only` | `once` | `durable` | `compatible_steps:array`, `predecessor:subject-reference`, `reason:string`, `revision:string`, `run:subject-reference`, `status:string` | `mission-run`, `revision` |
| `run-generation.state` | `run-generation` | `system-only` | `state-transition` | `durable` | `phase:string`, `previous_phase:string`, `reason:string`, `status:string`, `successor:subject-reference` | `mission-run`, `step`, `completion`, `finally`, `revision`, `cancellation` |
| `run-generation.superseded` | `run-generation` | `system-only` | `once` | `durable` | `phase:string`, `previous_phase:string`, `reason:string`, `status:string`, `successor:subject-reference` | `revision` |
| `runtime.action.deadline-reached` | `agent`, `exec`, `pty`, `gate-operation` | `system-only` | `append` | `system-local` | `action:string`, `blocking:array`, `code:string`, `deadline_key:string`, `desired_token:string`, `harness:string`, `incarnation_id:string`, `native_session_id:string`, `operation:string`, `operation_status:string`, `reason:string`, `rollout:object`, `rollout_operation:string`, `runtime_id:string`, `signal:string`, `source_host:string`, `terminal:boolean` | `stop`, `gate` |
| `runtime.action.failed` | `agent`, `exec`, `pty`, `gate-operation` | `system-only` | `append` | `system-local` | `action:string`, `blocking:array`, `code:string`, `deadline_key:string`, `desired_token:string`, `harness:string`, `incarnation_id:string`, `native_session_id:string`, `operation:string`, `operation_status:string`, `reason:string`, `rollout:object`, `rollout_operation:string`, `runtime_id:string`, `signal:string`, `source_host:string`, `terminal:boolean` | `stop`, `gate` |
| `runtime.action.requested` | `agent`, `exec`, `pty`, `gate-operation` | `authorized-requester` | `append` | `system-local` | `action:string`, `deadline_unix_ms:string`, `host:string`, `incarnation_id:string`, `operation:string`, `reason:string`, `rollout:object`, `runtime_id:string`, `signal:string`, `source_host:string`, `terminal:boolean` | `stop`, `gate` |
| `runtime.action.succeeded` | `agent`, `exec`, `pty`, `gate-operation` | `system-only` | `append` | `system-local` | `action:string`, `blocking:array`, `code:string`, `deadline_key:string`, `desired_token:string`, `harness:string`, `incarnation_id:string`, `native_session_id:string`, `operation:string`, `operation_status:string`, `reason:string`, `rollout:object`, `rollout_operation:string`, `runtime_id:string`, `signal:string`, `source_host:string`, `terminal:boolean` | `stop`, `gate` |
| `runtime.observed` | `agent`, `exec`, `pty`, `gate-operation` | `same-subject-actor` | `append` | `durable` | `adopted:boolean`, `driver:string`, `exit_code:integer`, `exit_signal:integer`, `host:string`, `incarnation_id:string`, `reachability:string`, `reason:string`, `runtime_id:string`, `shutdown_timeout_ms:integer`, `status:string`, `terminal:boolean` |  |
| `runtime.readiness-deadline-reached` | `agent` | `system-only` | `append` | `local` | `deadline_unix_ms!:string`, `driver!:string`, `incarnation_id!:string`, `reason!:string`, `runtime_id!:string` |  |
| `runtime.reconcile-decision` | `agent`, `exec`, `pty`, `schedule` | `system-only` | `append` | `durable` | `decision:string`, `gate:string`, `input_number:integer`, `key:string`, `reachability:string`, `reason:string`, `restart_at_unix_ms:string` |  |
| `runtime.restart-window-reset` | `agent`, `exec`, `pty` | `system-only` | `append` | `durable` | `desired_token:string`, `incarnation_id!:string`, `reason!:string` | `reset` |
| `schedule.occurrence-cancelled` | `schedule` | `system-only` | `append` | `durable` | `occurrence:integer`, `reason:string`, `revision:string` | `schedule` |
| `schedule.occurrence-reached` | `schedule` | `system-only` | `append` | `durable` | `at_unix_ms:integer`, `occurrence:integer`, `revision:string`, `scheduled:subject-reference`, `scheduled_at_unix_ms:string` | `schedule` |
| `schedule.occurrence-scheduled` | `schedule` | `system-only` | `append` | `durable` | `at_unix_ms:integer`, `occurrence:integer`, `revision:string`, `scheduled_at_unix_ms:string` | `schedule` |
| `schedule.work-failed` | `schedule` | `system-only` | `append` | `durable` | `code!:string`, `reason!:string`, `request!:string` | `schedule` |
| `schedule.work-requested` | `schedule` | `system-only` | `append` | `durable` | `inputs!:object`, `mission!:subject-reference(mission)`, `mission_revision!:string`, `occurrence!:integer`, `revision!:string`, `workspace!:string` | `schedule` |
| `schedule.work-started` | `schedule` | `system-only` | `append` | `durable` | `mission_run!:subject-reference(mission-run)`, `request!:string` | `schedule` |
| `sekret.called` | `sekret` | `system-only` | `append` | `local` | `argv:array`, `at_unix_ms!:integer`, `caller!:subject-reference(agent|person)`, `cwd:string`, `grant:string`, `login:boolean`, `person!:subject-reference(person)`, `profile!:string`, `seq!:integer`, `tty:boolean` |  |
| `sekret.changed` | `sekret` | `system-only` | `append` | `local` | `at_unix_ms!:integer`, `caller:subject-reference(agent|person)`, `change!:string`, `detail:object`, `person!:subject-reference(person)`, `profile:string`, `seq!:integer` |  |
| `sekret.exited` | `sekret` | `system-only` | `append` | `local` | `at_unix_ms!:integer`, `call!:integer`, `caller:subject-reference(agent|person)`, `error:string`, `exit_code:integer`, `person!:subject-reference(person)`, `profile:string`, `seq!:integer`, `signal:integer` |  |
| `sekret.refused` | `sekret` | `system-only` | `append` | `local` | `argv:array`, `at_unix_ms!:integer`, `caller:subject-reference(agent|person)`, `person!:subject-reference(person)`, `profile:string`, `reason!:string`, `seq!:integer` |  |
| `step-run.carried` | `step-run` | `system-only` | `once` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `definition_hash:string`, `source:subject-reference`, `source_generation:subject-reference`, `source_step_run:subject-reference`, `status:string`, `worker_reported:boolean` | `step` |
| `step-run.retried` | `step-run` | `system-only` | `append` | `durable` | `attempt:integer`, `goals:array`, `not_before_unix_ms:integer`, `reason:string`, `status:string` | `step` |
| `step-run.state` | `step-run` | `system-only` | `state-transition` | `durable` | `attempt:integer`, `readiness_epoch:integer`, `reason:string`, `status:string` | `step` |
| `subagent.appeared` | `agent` | `same-subject-actor` | `append` | `durable` | `description:string`, `driver!:string`, `incarnation_id!:string`, `lease_expires_at_unix_ms!:integer`, `session_id:string`, `started_at_unix_ms:integer`, `step_run:subject-reference(step-run)`, `subagent_id!:string`, `subagent_type:string` |  |
| `subagent.ended` | `agent` | `same-subject-actor` | `append` | `durable` | `cache_write_tokens:integer`, `cached_tokens:integer`, `duration_ms:integer`, `ended_at_unix_ms:integer`, `input_tokens:integer`, `outcome!:string`, `output_tokens:integer`, `reason:string`, `subagent_id!:string`, `total_tokens:integer` |  |
| `subagent.renewed` | `agent` | `same-subject-actor` | `append` | `durable` | `incarnation_id:string`, `lease_expires_at_unix_ms!:integer`, `subagent_id!:string` |  |
| `subscription.batch-sent` | `subscription` | `system-only` | `append` | `durable` | `entries!:integer`, `message!:subject-reference(message)`, `through!:string` | `subscription` |
| `subscription.batched` | `subscription` | `system-only` | `append` | `durable` | `delivery_key!:string`, `entries!:array` | `subscription` |
| `subscription.mission-deferred` | `subscription` | `system-only` | `append` | `durable` | `not_before_unix_ms!:integer`, `request!:string` | `subscription` |
| `subscription.mission-failed` | `subscription` | `system-only` | `append` | `durable` | `code!:string`, `reason!:string`, `request!:string` | `subscription` |
| `subscription.mission-request-cancelled` | `subscription` | `authorized-participant` | `append` | `durable` | `reason:string`, `request!:string` | `subscription` |
| `subscription.mission-request-released` | `subscription` | `authorized-participant` | `append` | `durable` | `reason:string`, `request!:string` | `subscription` |
| `subscription.mission-requested` | `subscription` | `system-only` | `append` | `durable` | `delivery_key:string`, `discovery!:string`, `held:boolean`, `mission!:subject-reference(mission)`, `mission_revision:string`, `requester:subject-reference(agent|person)`, `resource!:subject-reference(resource)`, `resource_input!:string`, `text:string`, `text_input:string`, `workspace!:string` | `subscription` |
| `subscription.mission-started` | `subscription` | `system-only` | `append` | `durable` | `mission_run!:subject-reference(mission-run)`, `request!:string` | `subscription` |
| `subscription.state` | `subscription` | `system-only` | `state-transition` | `durable` | `fields:array`, `observer:subject-reference`, `reason:string`, `state!:string`, `to:subject-reference` | `subscription` |
| `subscription.watch-ended` | `subscription` | `system-only` | `append` | `durable` | `message:subject-reference(message)`, `reason!:string`, `since_unix_ms!:string` | `subscription` |
| `terminal.input.requested` | `agent`, `pty` | `authorized-requester` | `append` | `durable` | `byte_count:integer`, `incarnation_id:string`, `intent:string`, `mode:string`, `runtime_id:string`, `sequence:integer`, `sha256:string` |  |
| `terminal.input.result` | `agent`, `pty` | `system-only` | `append` | `durable` | `incarnation_id:string`, `reason:string`, `result!:string`, `runtime_id:string`, `sequence:integer` |  |
| `terminal.launch-geometry` | `person` | `same-subject-actor` | `append` | `durable` | `columns!:integer`, `rows!:integer` |  |
| `transport.observed` | `host` | `system-only` | `append` | `durable` | `last_success_at:integer`, `protocol:string`, `reason:string`, `remote_heads:object`, `status!:string` |  |
| `work.claimed` | `step-run` | `authorized-participant` | `state-transition` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `extend_ms:integer`, `handoff_acknowledged:subject-reference`, `handoff_key:string`, `handoff_message:subject-reference`, `handoff_request:object`, `handoff_to:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.extended` | `step-run` | `authorized-participant` | `append` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `extend_ms:integer`, `handoff_acknowledged:subject-reference`, `handoff_key:string`, `handoff_message:subject-reference`, `handoff_request:object`, `handoff_to:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.failed` | `step-run` | `authorized-participant` | `once-per-attempt` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `extend_ms:integer`, `handoff_acknowledged:subject-reference`, `handoff_key:string`, `handoff_message:subject-reference`, `handoff_request:object`, `handoff_to:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.nudged` | `step-run` | `system-only` | `append` | `durable` | `agent:subject-reference`, `attempt:integer`, `idle_since_unix_ms:string`, `message:subject-reference`, `reason:string`, `waits:array` |  |
| `work.person-asked` | `step-run` | `authorized-participant` | `append` | `durable` | `attempt:integer`, `generation:subject-reference`, `key:string`, `legacy_request:string`, `mission_spec:object`, `origin_attempt:integer`, `origin_step:subject-reference`, `owner_generation:subject-reference`, `owner_run:subject-reference`, `person:subject-reference`, `reason:string`, `request:object`, `requester_declaration:string`, `run:subject-reference`, `status:string`, `title:string`, `waiting_since:string` |  |
| `work.person-cancelled` | `step-run` | `authorized-participant` | `append` | `durable` | `answer:object`, `attempt:integer`, `episode:string`, `key:string`, `status:string`, `summary:string` |  |
| `work.person-done` | `step-run` | `authorized-participant` | `append` | `durable` | `acted_for:subject-reference(person)`, `answer:object`, `attempt:integer`, `delegation:object`, `episode:string`, `key:string`, `status:string`, `summary:string` |  |
| `work.progress` | `step-run` | `authorized-participant` | `append` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `extend_ms:integer`, `handoff_acknowledged:subject-reference`, `handoff_key:string`, `handoff_message:subject-reference`, `handoff_request:object`, `handoff_to:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.released` | `step-run` | `authorized-participant` | `append` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `extend_ms:integer`, `handoff_acknowledged:subject-reference`, `handoff_key:string`, `handoff_message:subject-reference`, `handoff_request:object`, `handoff_to:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.renewed` | `step-run` | `authorized-participant` | `append` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `extend_ms:integer`, `handoff_acknowledged:subject-reference`, `handoff_key:string`, `handoff_message:subject-reference`, `handoff_request:object`, `handoff_to:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `work.submitted` | `step-run` | `authorized-participant` | `once-per-attempt` | `durable` | `attempt:integer`, `claim_expires_at_unix_ms:integer`, `claim_incarnation:string`, `claimant:subject-reference`, `extend_ms:integer`, `handoff_acknowledged:subject-reference`, `handoff_key:string`, `handoff_message:subject-reference`, `handoff_request:object`, `handoff_to:subject-reference`, `readiness_epoch:integer`, `reason:string`, `status:string`, `summary:string`, `worker_reported:boolean` |  |
| `workspace.observed` | `agent` | `system-only` | `append` | `latest` | `host!:string`, `repository:string`, `workspace!:string` |  |

`resource.observed` validates facts against the resource kind. Custom resource facts remain open.

A `durable` claim is a fact in the replicated claim log. A `local` claim is an observation kept only in the local observation log of the node that made it, trimmed after that node's retention window. A `latest` claim is an observation kept in that log whose replicated claims are written only when its state changes; each one replaces the previous one for its subject. A `system-local` claim is `local` when the system records it without an actor and replicates when a person or agent writes it as its actor.

## Current person directive notes

`person.directive-note-set` records the subject person's current context on `person/NAME`. Only that exact person may set or clear it, including replicated admission. Required `text` is null to clear, or nonblank text of at most 4096 UTF-8 bytes; optional `expires_at` is a valid RFC 3339 UTC timestamp. The canonical latest claim selects the current revision, with its actor as author and accepted time as creation time. A clear or expired revision hides every earlier note.

Notes inform and never grant approval, gate verdicts, delegation, or person-ask answers. A person reads only their own note. An agent reads the union of known account owners, declaration authorship chains and mission-run requesters it works for; missing or cyclic ownership does not select a global operator. Ownership walks stop at 16 hops, and oversized relation sets fail closed. Exact person-key lookups read a current projection that retains clear and expired tombstones; immutable claims remain authoritative. Projection creation does not backfill existing history during migration.

Publication requires anchored membership and `features.person_directive_note=1` on the latest own-origin `daemon.started` for every active member. Known unfenced legacy writers also prevent publication. Advertisements cannot prove discovery of every legacy peer: fence legacy peers from replication before enabling notes.

## Harness todo snapshots

`harness.todo.observed` replaces the entire seat todo list. Session and incarnation identify its source; `observed_at` is source timestamp provenance, not an ordering clock. Keep the last snapshot until replaced, and expose stale provenance rather than presenting an old binding as current. Missing means unobserved; `phases: []`, zero totals and `truncated: false` means known empty.

Each phase has `name` and `tasks`; each task has `content`, `status` (`pending`, `in_progress`, `completed`, `blocked`) and optional string `blocker`. The shared phase/task shape can also represent a future plan with one unnamed phase. Bounds are 16 phases, 100 tasks total, 128 UTF-8 bytes per phase name and 512 per content/blocker. Producers shorten at UTF-8 boundaries and omit trailing tasks/phases in source order to keep serialized claim fields within 64 KiB (including JSON escaping). Bound-driven shortening or omission sets `truncated`. `totals` contains nonnegative integer counts for all four statuses from the full source: counts equal the visible list when not truncated and cannot be less than visible counts when truncated. Unknown nested fields, invalid statuses, null blockers and oversized fields are rejected.

OMP's native `abandoned` tasks are omitted from phase tasks rather than relabeled as completed. Their enclosing phase is preserved when it fits. `totals.abandoned` counts these dropped tasks separately; it is optional on the wire and defaults to zero when absent. Totals for the four task statuses count the full source snapshot and exclude abandoned tasks from active progress. Dropping an abandoned task does not set `truncated`; that flag describes text/list/serialized-size bounds only. The OMP producer always emits the abandoned count and reserves 4 KiB of the serialized-fields budget for authenticated provenance.

## Local checkpoint capture storage

SQLite `user_version` is 18. The local checkpoint guard uses trigger version 3, independently of the unchanged claim/wire registry digest. Initialization adds the singleton, any missing guard columns, and its mutation triggers; it does not scan history, rewrite claim bodies, rebuild projections, or force replay.

```sql
CREATE TABLE IF NOT EXISTS checkpoint_capture_epoch (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    value INTEGER NOT NULL,
    envelope_frontier INTEGER NOT NULL DEFAULT 0,
    cut_unix_ms INTEGER NOT NULL DEFAULT 0,
    trigger_version INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO checkpoint_capture_epoch(id, value, envelope_frontier) VALUES (1, 0, 0);
```

Before the first page, a short read snapshot chooses the sealed envelope rowid and checks the persisted guard bounds. If either bound needs to grow, one atomic autocommit writer statement registers `MAX(envelope_frontier, seal_rowid)` and `MAX(cut_unix_ms, requested_cut)`. Already-covered bounds require no writer loan or durable update. Each attempt reads `SELECT value FROM checkpoint_capture_epoch WHERE id=1`; all retries retain the original seal and cut without registering them again. Every metadata, body, protection, and tombstone page checks that value in its own short snapshot. A mismatch discards the whole attempt; three invalidations fail closed. The registered bounds only grow across concurrent captures and reconnects; registering them does not increment the epoch. A zero cut is inactive.

An envelope is relevant when its rowid is at/below the registered frontier and its accepted time is below the registered cut. A protected or canonical-order target additionally requires the claim's own accepted time below the cut. Mutations audit both OLD and NEW identities and references. These retained highwaters can conservatively invalidate a smaller later capture, but ordinary above-cut history does not invalidate it merely because that history is already within the row frontier.

Claim-to-envelope relevance seeks batch membership through `replica_envelopes_batch` and record membership through `replica_records_claim`, then performs an exact envelope-identity lookup. The two relationships use separate EXISTS branches; record membership is driven from records with CROSS JOIN, not from the envelope frontier. Mutation work therefore depends on the target's relationships rather than unrelated retained history. Guard version 3 replaces version 2's envelope-side OR predicate, which scanned the frontier for each deleted claim during trim, without changing invalidation semantics or adding indexes.

| Captured source | Mutations that increment the epoch |
|---|---|
| `claims` | Relevant envelope-associated body, identity, canonical-key and membership changes, body backfills, and deletions. Late claims in a relevant envelope remain exclusion witnesses: changing their accepted time, identity or batch, or deleting them, can change whether that envelope qualifies. |
| `batches` | Backfills, canonical writer/sequence changes, identity/rowid changes and deletions for claims associated with a relevant envelope. Hash and batch-time bookkeeping alone is not captured. |
| `replica_records` | Backfills and captured identity, position, admission-state, claim-reference or replacement-reference changes in a relevant envelope. A newer duplicate naming a captured below-cut claim is also fenced on INSERT, UPDATE and DELETE: canonical `MIN(position)` reads every copy, not only the sealed envelope prefix. |
| `replica_envelopes` | Relevant identity, accepted-time or rowid changes and deletions. Any successful below-cut INSERT conservatively invalidates, including explicit rowid backfill and identical REPLACE relocating an old identity beyond the frontier. Receipt state, relay, validation errors and received-time bookkeeping alone is not captured. |
| `desired`, `mission_definitions`, `mission_revisions`, `documents` | INSERT/DELETE referencing a captured below-cut claim, and UPDATE moving either OLD or NEW reference to/from such a claim (or moving its scan rowid). Unchanged references and references confined to above-cut claims are ignored. Captured projection rebuilds and direct desired deletion remain fenced. |
| Repair references | `record.repaired` receipt changes referencing a captured replacement or a record in a relevant envelope; `repair.applied` receipt changes referencing captured predecessor claims; record replacement references into captured below-cut claims. Repairs only concerning newer history are ignored. |
| `checkpoint_claims`, `checkpoint_envelopes` | Below-cut tombstone INSERT/DELETE and captured metadata, identity, accepted-time or rowid changes; both sides of a cut crossing are checked. The checkpoint-label field alone is not captured. |
| Identity collisions on INSERT | BEFORE INSERT compares the colliding existing row's capture-relevant content, covering REPLACE with recursive triggers disabled, including removal of captured protection or tombstones. Identical ignored duplicates do not invalidate. Successful relevant inserts still run their AFTER fence. |

Capture does not read envelope signatures/holds, projection-health rows, peer/cursor state, blob bytes, operation caches, or checkpoint status as metadata, so mutations confined to those tables need no capture fence. Blob-backed document bindings and operation identity in claim bodies/tombstones are covered by their captured tables. Any future captured source must extend the trigger audit before pages may read it. Accepted-time parsing remains a prerequisite owned by #2106, not a separate compatibility change here.

Triggers persist and increment the epoch in the mutation's own transaction on every connection, without connection-local hooks or SQL functions. Rollback rolls back the increment too. A trigger-version migration atomically drops/recreates the old predicates, increments `value`, and stores `trigger_version=3`; captures using the old guard must restart. Earlier version-18 PR-head stores receive `cut_unix_ms` and `trigger_version` columns with default zero while preserving their epoch and frontier. Version-2 stores retain their active cut/frontier through the version-3 predicate migration. Ordinary reopens preserve all bounds and do not repeat the migration.

Upgrade requires the normal process restart, not a history migration; its restart duration has not been measured. Stable older binaries reject storage version 18. Earlier version-18 PR-head binaries are not rollback targets: guard version 1 cannot safely operate the cut-aware schema, and guard version 2 rejects trigger version 3 on open. Their copied stores can migrate forward only. Binary rollback requires restoring a pre-upgrade database snapshot; otherwise roll forward. No replicated claim, checkpoint rule, wire protocol, or response shape changes.
