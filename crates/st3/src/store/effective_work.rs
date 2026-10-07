//! One effective-state calculation for Store readers and certified keyed consumers.
//! Input providers own source coverage. An absent owner preserves the existing Store
//! behavior; it is never a certificate that a public work row exists or is authorized.
use super::*;

pub(super) struct Owner {
    pub run_status: String,
    pub run_phase: String,
    pub current_generation: String,
    pub generation_status: String,
    pub root_status: String,
    pub root_phase: String,
}

/// Providers are called lazily in the established Store order, at one captured time.
/// Keyed implementations must certify owner/generation/root, person-request and blocker
/// dependencies before calling apply; unknown coverage must be an error, not None/empty.
pub(super) trait Inputs {
    fn owner(&mut self, view: &StepRunView) -> rusqlite::Result<Option<Owner>>;
    fn person_request_live(
        &mut self,
        view: &StepRunView,
        at: u128,
    ) -> rusqlite::Result<Option<bool>>;
    fn active_blockers(&mut self, view: &StepRunView, at: u128) -> rusqlite::Result<Vec<String>>;
}

pub(super) struct SqlInputs<'a>(pub &'a Connection);
impl Inputs for SqlInputs<'_> {
    fn owner(&mut self, view: &StepRunView) -> rusqlite::Result<Option<Owner>> {
        self.0
            .query_row(
                "SELECT mission_runs.status, mission_runs.phase,
                    mission_runs.current_generation_id, run_generations.status,
                    root_runs.status, root_runs.phase
             FROM mission_runs JOIN run_generations
               ON run_generations.id=?2 AND run_generations.run_id=mission_runs.id
             JOIN mission_runs root_runs ON root_runs.id=mission_runs.root_run_id
             WHERE mission_runs.id=?1",
                params![
                    view.run.strip_prefix("mission-run/").unwrap_or(&view.run),
                    generation_id_from_subject(&view.generation),
                ],
                |row| {
                    Ok(Owner {
                        run_status: row.get(0)?,
                        run_phase: row.get(1)?,
                        current_generation: row.get(2)?,
                        generation_status: row.get(3)?,
                        root_status: row.get(4)?,
                        root_phase: row.get(5)?,
                    })
                },
            )
            .optional()
    }
    fn person_request_live(
        &mut self,
        view: &StepRunView,
        at: u128,
    ) -> rusqlite::Result<Option<bool>> {
        let ask = person_work::request(self.0, &view.subject)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
        ask.map(|ask| {
            if matches!(view.status.as_str(), "ready" | "pending") {
                person_work::current(self.0, &ask, at)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))
            } else {
                // The original reader loads the ask in every person state, but checks
                // its current ownership only for ready/pending steps.
                Ok(true)
            }
        })
        .transpose()
    }
    fn active_blockers(&mut self, view: &StepRunView, at: u128) -> rusqlite::Result<Vec<String>> {
        active_step_blockers_tx(self.0, &view.subject, at)
    }
}

pub(super) fn apply(
    view: &mut StepRunView,
    snapshot_unix_ms: u128,
    inputs: &mut impl Inputs,
) -> rusqlite::Result<()> {
    let Some(owner) = inputs.owner(view)? else {
        return Ok(());
    };
    if view
        .assigned_to
        .as_deref()
        .is_some_and(|person| person.starts_with("person/"))
    {
        if let Some(request_live) = inputs.person_request_live(view, snapshot_unix_ms)? {
            if matches!(view.status.as_str(), "ready" | "pending") && !request_live {
                view.status = "cancelled".into();
                view.blocked_reason =
                    Some("the requester, originating attempt or owning run ended".into());
            }
        }
    }
    let generation_id = generation_id_from_subject(&view.generation);
    if is_terminal_run_state(&owner.run_status)
        || owner.run_phase == "terminal"
        || is_terminal_generation_state(&owner.generation_status)
        || generation_id != owner.current_generation
        || is_terminal_run_state(&owner.root_status)
        || owner.root_phase == "terminal"
    {
        if !matches!(view.status.as_str(), "completed" | "failed" | "cancelled") {
            view.status = "cancelled".into();
            view.blocked_reason = Some("the owning mission run or generation is terminal".into());
        }
        view.claimant = None;
        view.claim_incarnation = None;
        view.claim_expires_at_unix_ms = None;
        return Ok(());
    }
    if matches!(
        view.status.as_str(),
        "claimed" | "working" | "verifying" | "blocked"
    ) && view
        .claim_expires_at_unix_ms
        .is_some_and(|expiry| expiry <= snapshot_unix_ms)
    {
        view.status = "ready".into();
        view.blocked_reason = Some("the worker lease expired".into());
        // Expiry is a new readiness episode even before a repair writes it. Old
        // consumed wakes must not acknowledge this newly unclaimed work. A claim
        // persists this effective epoch, so later expiries advance it once more.
        view.readiness_epoch = view.readiness_epoch.saturating_add(1);
        view.claimant = None;
        view.claim_incarnation = None;
        view.claim_expires_at_unix_ms = None;
    }
    if view.status == "waiting-person" {
        view.blockers = inputs.active_blockers(view, snapshot_unix_ms)?;
        if view.blockers.is_empty() {
            view.status = "ready".into();
            view.blocked_reason = None;
            view.readiness_epoch += 1;
        }
    }
    if view.status == "ready" && view.blocked_reason.is_some() {
        view.blockers = inputs.active_blockers(view, snapshot_unix_ms)?;
        if view.blockers.is_empty() {
            view.blocked_reason = None;
        } else {
            view.status = "blocked".into();
            view.claimant = None;
            view.claim_incarnation = None;
            view.claim_expires_at_unix_ms = None;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Recorded {
        owner: Option<Owner>,
        person: Option<Option<bool>>,
        blockers: Option<Vec<String>>,
        calls: Vec<&'static str>,
    }
    impl Inputs for Recorded {
        fn owner(&mut self, _view: &StepRunView) -> rusqlite::Result<Option<Owner>> {
            self.calls.push("owner");
            Ok(self.owner.take())
        }
        fn person_request_live(
            &mut self,
            _view: &StepRunView,
            _at: u128,
        ) -> rusqlite::Result<Option<bool>> {
            self.calls.push("person");
            self.person.ok_or(rusqlite::Error::InvalidQuery)
        }
        fn active_blockers(
            &mut self,
            _view: &StepRunView,
            _at: u128,
        ) -> rusqlite::Result<Vec<String>> {
            self.calls.push("blockers");
            self.blockers.clone().ok_or(rusqlite::Error::InvalidQuery)
        }
    }
    fn fixture() -> (Store, StepRunView) {
        let store = Store::open_memory("birch").unwrap();
        let intent=crate::parse_intent("version 2\nmission \"orchard\" state=\"ready\" { goal \"Prepare a sample.\"; step \"build\" { goal \"Build a sample.\"; } }\n",store.origin()).unwrap();
        store.apply_internal(&intent, "publish").unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "orchard".into(),
                revision: None,
                workspace: "/example/project".into(),
                requester: Some("person/avery".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "start".into(),
            })
            .unwrap();
        (store, run.steps[0].clone())
    }
    fn inputs(connection: &Connection, view: &StepRunView) -> Recorded {
        Recorded {
            owner: SqlInputs(connection).owner(view).unwrap(),
            person: Some(None),
            blockers: Some(Vec::new()),
            calls: Vec::new(),
        }
    }

    #[test]
    fn alternate_inputs_match_sql_at_lease_boundary_and_refuse_unknown_blockers() {
        let (store, mut step) = fixture();
        step.status = "working".into();
        step.claimant = Some("agent/sample/worker".into());
        step.claim_incarnation = Some("sample-incarnation".into());
        step.claim_expires_at_unix_ms = Some(10);
        step.readiness_epoch = u32::MAX;
        let connection = store.readers.get();
        for at in [9, 10, 11] {
            let mut alternate = step.clone();
            let mut sql = step.clone();
            let mut inputs = inputs(&connection, &step);
            apply(&mut alternate, at, &mut inputs).unwrap();
            apply(&mut sql, at, &mut SqlInputs(&connection)).unwrap();
            assert_eq!(
                serde_json::to_value(&alternate).unwrap(),
                serde_json::to_value(&sql).unwrap()
            );
            assert_eq!(alternate.claimant.is_some(), at < 10);
            assert_eq!(alternate.readiness_epoch, u32::MAX);
            assert_eq!(
                inputs.calls,
                if at < 10 {
                    vec!["owner"]
                } else {
                    vec!["owner", "blockers"]
                }
            );
        }
        let mut uncertified = inputs(&connection, &step);
        uncertified.blockers = None;
        assert!(
            apply(&mut step, 10, &mut uncertified).is_err(),
            "unknown blocker coverage cannot become empty"
        );
    }

    #[test]
    fn terminal_and_missing_owner_preserve_lazy_dependency_order() {
        let (store, mut step) = fixture();
        let connection = store.readers.get();
        step.status = "waiting-person".into();
        step.blocked_reason = Some("step-run/sample/ask".into());
        let mut terminal = inputs(&connection, &step);
        terminal.owner.as_mut().unwrap().root_phase = "terminal".into();
        terminal.blockers = None;
        apply(&mut step, 10, &mut terminal).unwrap();
        assert_eq!(step.status, "cancelled");
        assert_eq!(terminal.calls, ["owner"]);

        step.assigned_to = Some("person/avery".into());
        step.status = "ready".into();
        let mut terminal = inputs(&connection, &step);
        terminal.owner.as_mut().unwrap().run_phase = "terminal".into();
        terminal.person = Some(Some(false));
        terminal.blockers = None;
        apply(&mut step, 10, &mut terminal).unwrap();
        assert_eq!(terminal.calls, ["owner", "person"]);
        assert_eq!(
            step.blocked_reason.as_deref(),
            Some("the requester, originating attempt or owning run ended")
        );

        let before = serde_json::to_value(&step).unwrap();
        let mut missing = Recorded {
            owner: None,
            person: None,
            blockers: None,
            calls: Vec::new(),
        };
        apply(&mut step, 10, &mut missing).unwrap();
        assert_eq!(serde_json::to_value(&step).unwrap(), before);
        assert_eq!(missing.calls, ["owner"]);
    }
}
