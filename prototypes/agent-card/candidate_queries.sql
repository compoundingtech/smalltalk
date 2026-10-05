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

-- A point read seeks the immutable subject AVL root chosen at the cut, then
-- follows one branch per level. A canonical epoch fork reuses the old root for
-- unchanged agents; it does not copy their version intervals.
WITH RECURSIVE seek(id) AS (
    SELECT point_root_id FROM (
        SELECT point_root_id FROM agent_card_roots
        WHERE epoch=?1 AND history=?2 AND status='*' AND store_index<=?3
        ORDER BY store_index DESC LIMIT 1
    )
), walk(id) AS (
    SELECT id FROM seek
    UNION ALL
    SELECT CASE WHEN ?4<node.agent THEN node.left_id ELSE node.right_id END
    FROM walk JOIN agent_card_subject_nodes AS node ON node.id=walk.id
    WHERE node.agent<>?4
)
SELECT card.card_json FROM walk
JOIN agent_card_subject_nodes AS node ON node.id=walk.id
JOIN agent_card_versions AS card
  ON card.agent=node.agent AND card.version=node.card_version
WHERE node.agent=?4 LIMIT 1;

-- A history/status ordered page chooses one immutable predecessor root.
SELECT root_id FROM agent_card_roots
WHERE epoch=?1 AND history=?2 AND status=?3 AND store_index<=?4
ORDER BY store_index DESC LIMIT 1;

-- The public page uses the final presentation root. Continuations refer to
-- its exact root ID, while a first page chooses the latest source/local cut.
SELECT time_root_id, store_index, local_generation
FROM agent_card_presentation_roots
WHERE epoch=?1 AND history=?2 AND store_index<=?3
ORDER BY store_index DESC, local_generation DESC LIMIT 1;
