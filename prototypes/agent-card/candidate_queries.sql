-- Run each query twice in the same writer transaction: once before changing
-- desired/step/current-dependency rows and once after. Union the agent IDs.
-- The outer caller also includes direct agent subjects, old/new message.sent
-- from/to agents and old/new work.progress/submitted actors from claim payloads.

-- Changed run or generation. This primary-key seek returns only linked agents.
SELECT agent FROM agent_card_dependency_current
WHERE relation = ?1 AND source = ?2 ORDER BY agent;

-- The relation table is populated from these indexed projection lookups.
-- `desired_owner_run_index` already covers owner_run+subject.
SELECT subject FROM desired
WHERE kind='agent' AND owner_run=?1 ORDER BY subject;

-- `agent_card_desired_owner_generation` covers owner_generation+subject.
SELECT subject FROM desired
WHERE kind='agent' AND owner_generation=?1 ORDER BY subject;

-- `step_runs_run_index` covers run_id+generation_id. A change of generation
-- collects old/new assignee and lease owner of the affected run's step rows.
SELECT assignee AS agent FROM step_runs
WHERE run_id=?1 AND generation_id=?2 AND assignee IS NOT NULL
UNION
SELECT lease_owner AS agent FROM step_runs
WHERE run_id=?1 AND generation_id=?2 AND lease_owner IS NOT NULL;

-- A changed step can be located by its subject primary key. available_to is
-- passed to the queue selection for these agents, never made into a new key.
SELECT assignee, lease_owner, available_to FROM step_runs WHERE subject=?1;

-- A point card read uses this key, not the ordered fleet root.
SELECT card_json FROM agent_card_versions
WHERE agent=?1 AND version<=?2 ORDER BY version DESC LIMIT 1;

-- A history/status ordered page chooses one immutable predecessor root.
SELECT root_id FROM agent_card_roots
WHERE history=?1 AND status=?2 AND store_index<=?3
ORDER BY store_index DESC LIMIT 1;

-- The public page uses the final presentation root. Continuations refer to
-- its exact root ID, while a first page chooses the latest source/local cut.
SELECT root_id, store_index, local_generation
FROM agent_card_presentation_roots
WHERE history=?1 AND status=?2 AND store_index<=?3
ORDER BY store_index DESC, local_generation DESC LIMIT 1;
