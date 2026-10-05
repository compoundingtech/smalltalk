-- Source-only schema sketch. Migration, canonical source cuts, and writer hooks
-- require integration review before this is used by the daemon.
CREATE TABLE agent_card_nodes (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    agent TEXT NOT NULL,
    card_version INTEGER NOT NULL,
    height INTEGER NOT NULL CHECK (height BETWEEN 1 AND 63),
    left_id INTEGER REFERENCES agent_card_nodes(id),
    right_id INTEGER REFERENCES agent_card_nodes(id)
);
CREATE TABLE agent_card_versions (
    agent TEXT NOT NULL,
    version INTEGER NOT NULL,
    card_json TEXT NOT NULL CHECK (json_valid(card_json)),
    PRIMARY KEY(agent, version)
);
-- `version` is an opaque immutable row ID, not a store index. A second
-- path-copied AVL tree maps agent -> card version at each source root. It
-- preserves unchanged agents across an epoch fork without fleet copying.
CREATE TABLE agent_card_subject_nodes (
    id INTEGER PRIMARY KEY,
    agent TEXT NOT NULL,
    card_version INTEGER NOT NULL,
    height INTEGER NOT NULL CHECK (height BETWEEN 1 AND 63),
    left_id INTEGER REFERENCES agent_card_subject_nodes(id),
    right_id INTEGER REFERENCES agent_card_subject_nodes(id)
);
-- A canonical repair closes the old epoch at the first changed store index.
-- Cursor cuts before that fence remain valid; cuts at/after it return a gap.
CREATE TABLE agent_card_epochs (
    epoch INTEGER PRIMARY KEY,
    valid_through_store_index INTEGER,
    source_digest TEXT NOT NULL
);
CREATE TABLE agent_card_roots (
    epoch INTEGER NOT NULL REFERENCES agent_card_epochs(epoch),
    store_index INTEGER NOT NULL,
    history INTEGER NOT NULL CHECK (history IN (0, 1)),
    status TEXT NOT NULL,
    root_id INTEGER REFERENCES agent_card_nodes(id),
    point_root_id INTEGER REFERENCES agent_card_subject_nodes(id),
    retired_at_ms INTEGER,
    PRIMARY KEY(epoch, history, status, store_index)
);
-- Reverse seek by canonical cut: one predecessor root, no roster scan.
CREATE INDEX agent_card_root_cut_index
    ON agent_card_roots(epoch, history, status, store_index DESC);

-- Prior versions are immutable because retained presentation roots name them.
CREATE TABLE agent_card_local_fact_versions (
    agent TEXT NOT NULL,
    version INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    facts_json TEXT NOT NULL CHECK (json_valid(facts_json)),
    PRIMARY KEY(agent, version)
);
CREATE INDEX agent_card_fact_at_generation
    ON agent_card_local_fact_versions(agent, generation DESC);
CREATE TABLE agent_card_local_current (
    agent TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    next_deadline_ms BLOB CHECK (next_deadline_ms IS NULL OR length(next_deadline_ms)=16),
    FOREIGN KEY(agent, version) REFERENCES agent_card_local_fact_versions(agent, version)
);
CREATE INDEX agent_card_local_deadline
    ON agent_card_local_current(next_deadline_ms, agent)
    WHERE next_deadline_ms IS NOT NULL;
CREATE TABLE agent_card_presentation_nodes (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    agent TEXT NOT NULL,
    card_version INTEGER NOT NULL,
    local_fact_version INTEGER NOT NULL,
    final_status TEXT NOT NULL,
    height INTEGER NOT NULL CHECK (height BETWEEN 1 AND 63),
    left_id INTEGER REFERENCES agent_card_presentation_nodes(id),
    right_id INTEGER REFERENCES agent_card_presentation_nodes(id)
);
CREATE TABLE agent_card_presentation_roots (
    epoch INTEGER NOT NULL REFERENCES agent_card_epochs(epoch),
    store_index INTEGER NOT NULL,
    local_generation INTEGER NOT NULL,
    history INTEGER NOT NULL CHECK (history IN (0, 1)),
    time_root_id INTEGER REFERENCES agent_card_time_nodes(id),
    created_ms INTEGER NOT NULL,
    retired_at_ms INTEGER,
    PRIMARY KEY(epoch, store_index, local_generation, history)
);
CREATE INDEX agent_card_presentation_latest
    ON agent_card_presentation_roots(epoch, history, store_index DESC, local_generation DESC);

-- Fixed-depth binary interval tree over the full u128 millisecond domain.
-- An agent's exact final-status interval [start,end) is inserted into at most
-- 256 canonical time nodes. At a frozen time, visit the 128-node path and
-- merge their ordered per-status AVL roots. No expiry catch-up runs on read.
CREATE TABLE agent_card_time_nodes (
    id INTEGER PRIMARY KEY,
    depth INTEGER NOT NULL CHECK (depth BETWEEN 0 AND 128),
    left_id INTEGER REFERENCES agent_card_time_nodes(id),
    right_id INTEGER REFERENCES agent_card_time_nodes(id),
    -- An immutable node is fetched and copied as one row. GC must walk each
    -- encoded ordered-root ID because JSON cannot carry foreign keys.
    status_roots_json TEXT NOT NULL CHECK (json_valid(status_roots_json))
);

-- Reverse owner-generation seek is missing from the current #1409 schema.
CREATE INDEX agent_card_desired_owner_generation
    ON desired(owner_generation, subject) WHERE kind='agent';
-- Existing desired_owner_run_index, step_runs_run_index,
-- step_runs_assignee_index and step_runs_lease_index are reused.

-- Current reverse edges are read before and after the writer mutation. Historical
-- membership lives in immutable roots above; no history-wide dependency scan
-- is needed for the ordinary current writer path.
CREATE TABLE agent_card_dependency_current (
    relation TEXT NOT NULL,
    source TEXT NOT NULL,
    agent TEXT NOT NULL,
    PRIMARY KEY(relation, source, agent)
);
CREATE INDEX agent_card_dependency_agent
    ON agent_card_dependency_current(agent, relation, source);

-- A per-agent/incarnation balanced canonical event tree stores a WorkingRun
-- summary at every node. The content and rotations are described in
-- working_run.rs; path-copy is required for retained historical roots.
CREATE TABLE agent_harness_state_nodes (
    id INTEGER PRIMARY KEY,
    agent TEXT NOT NULL,
    incarnation TEXT NOT NULL,
    canonical_key BLOB NOT NULL,
    state TEXT NOT NULL,
    accepted_at_ms TEXT NOT NULL,
    height INTEGER NOT NULL CHECK (height BETWEEN 1 AND 63),
    left_id INTEGER REFERENCES agent_harness_state_nodes(id),
    right_id INTEGER REFERENCES agent_harness_state_nodes(id),
    summary_json TEXT NOT NULL CHECK (json_valid(summary_json))
);
CREATE TABLE agent_harness_state_roots (
    agent TEXT NOT NULL,
    incarnation TEXT NOT NULL,
    epoch INTEGER NOT NULL REFERENCES agent_card_epochs(epoch),
    store_index INTEGER NOT NULL,
    root_id INTEGER REFERENCES agent_harness_state_nodes(id),
    PRIMARY KEY(agent, incarnation, epoch, store_index)
);
