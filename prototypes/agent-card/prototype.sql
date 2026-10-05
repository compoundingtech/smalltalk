-- Source-only schema sketch. Migration, canonical source cuts, and writer hooks
-- require integration review before this is used by the daemon.
CREATE TABLE agent_card_nodes (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    agent TEXT NOT NULL,
    card_version INTEGER NOT NULL,
    priority BLOB NOT NULL,
    left_id INTEGER REFERENCES agent_card_nodes(id),
    right_id INTEGER REFERENCES agent_card_nodes(id)
);
CREATE TABLE agent_card_versions (
    agent TEXT NOT NULL,
    version INTEGER NOT NULL,
    card_json TEXT NOT NULL CHECK (json_valid(card_json)),
    PRIMARY KEY(agent, version)
);
CREATE TABLE agent_card_roots (
    store_index INTEGER NOT NULL,
    history INTEGER NOT NULL CHECK (history IN (0, 1)),
    status TEXT NOT NULL,
    root_id INTEGER REFERENCES agent_card_nodes(id),
    PRIMARY KEY(history, status, store_index)
);
-- Reverse seek by canonical cut: one predecessor root, no roster scan.
CREATE INDEX agent_card_root_cut_index
    ON agent_card_roots(history, status, store_index DESC);

CREATE TABLE agent_card_local_nodes (
    id INTEGER PRIMARY KEY,
    agent TEXT NOT NULL,
    fact_version INTEGER NOT NULL,
    next_deadline_ms INTEGER,
    priority BLOB NOT NULL,
    left_id INTEGER REFERENCES agent_card_local_nodes(id),
    right_id INTEGER REFERENCES agent_card_local_nodes(id)
);
CREATE TABLE agent_card_local_roots (
    generation INTEGER PRIMARY KEY,
    root_id INTEGER REFERENCES agent_card_local_nodes(id),
    created_ms INTEGER NOT NULL
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
    accepted_at_ms INTEGER NOT NULL,
    priority BLOB NOT NULL,
    left_id INTEGER REFERENCES agent_harness_state_nodes(id),
    right_id INTEGER REFERENCES agent_harness_state_nodes(id),
    summary_json TEXT NOT NULL CHECK (json_valid(summary_json))
);
CREATE TABLE agent_harness_state_roots (
    agent TEXT NOT NULL,
    incarnation TEXT NOT NULL,
    store_index INTEGER NOT NULL,
    root_id INTEGER REFERENCES agent_harness_state_nodes(id),
    PRIMARY KEY(agent, incarnation, store_index)
);
