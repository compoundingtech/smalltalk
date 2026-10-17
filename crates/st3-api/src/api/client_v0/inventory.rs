//! Screen inventories use the same authorized reads as HTTP, on the collection snapshot.
use super::*;
use std::path::PathBuf;

pub fn is_inventory(collection: &str) -> bool {
    matches!(
        collection,
        "clients" | "usage" | "machines" | "devices" | "sessions" | "terminals"
    )
}

pub fn validate(request: &CollectionSubscribe) -> Result<(), ApiError> {
    if request.collection != "usage" && (request.since_ms.is_some() || request.until_ms.is_some()) {
        return Err(validation("period boundaries are supported for usage only"));
    }
    if matches!(
        request.collection.as_str(),
        "clients" | "usage" | "machines" | "sessions" | "terminals"
    ) && (request.person.is_some() || request.actor.is_some())
    {
        return Err(validation(
            "this inventory does not accept person or actor filters",
        ));
    }
    if request.collection == "devices" && request.actor.is_some() {
        return Err(validation("devices select a person, not an actor"));
    }
    if request
        .since_ms
        .zip(Some(request.until_ms.unwrap_or(client_now_ms() as u64)))
        .is_some_and(|(since, until)| since > until)
    {
        return Err(validation("usage start must be before its end"));
    }
    Ok(())
}

/// Shared serialization keeps the HTTP and followed usage reports identical.
pub fn usage_value(store: &Store, since_ms: u64, until_ms: u64) -> anyhow::Result<Value> {
    let (mut rows, agent_messages) = store.usage_period_report(since_ms, until_ms)?;
    for row in &mut rows {
        if let Some(fields) = row.as_object_mut() {
            fields.retain(|_, value| value.as_str() != Some(""));
        }
    }
    let limits = store
        .account_limits()?
        .into_iter()
        .map(|limit| {
            let mut value = serde_json::to_value(limit).expect("account limit is serializable");
            if let Some(fields) = value.as_object_mut() {
                fields.retain(|_, value| !value.is_null());
            }
            value
        })
        .collect::<Vec<_>>();
    Ok(
        json!({"since_ms":since_ms, "until_ms":until_ms, "rows":rows, "limits":limits, "agent_messages":agent_messages}),
    )
}

pub fn items(
    state: &AppState,
    session: &ClientSession,
    request: &CollectionSubscribe,
    snapshot: &ClientSnapshot,
) -> anyhow::Result<(Vec<Value>, bool)> {
    let limit = request.limit.unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS);
    if let Some(deadline) = &request.inventory_deadline {
        *deadline.lock().expect("inventory deadline") = session.grant_expires_at;
    }
    let mut items = match request.collection.as_str() {
        "clients" => {
            let mut rows = client_connection_items(state, snapshot)
                .map_err(|error| anyhow::anyhow!(error.message))?;
            if let Some(deadline) = &request.inventory_deadline {
                let expiry = super::super::client_presence::next_expiry(&rows);
                let mut deadline = deadline.lock().expect("inventory deadline");
                *deadline = deadline.iter().copied().chain(expiry).min();
            }
            for row in &mut rows {
                // The grouping tuple is stable across connect/disconnect and has no ambiguous separators.
                let identity =
                    json!([row["member"], row["actor"], row["client"], row["via"]]).to_string();
                row["id"] = json!(format!(
                    "connected-client/{}",
                    hex::encode(Sha256::digest(identity.as_bytes()))
                ));
                row["kind"] = json!("connected-client");
                row["updated_at"] = row["last_seen"].clone();
                row["revision"] = json!(hex::encode(Sha256::digest(row.to_string().as_bytes())));
            }
            rows
        }
        "usage" => {
            // The graph clock can predate this period on an idle or empty member.
            // Open periods end at wall time, just as the authorized HTTP read does.
            let until = request.until_ms.unwrap_or(client_now_ms() as u64);
            let since = request.since_ms.unwrap_or(until.saturating_sub(86_400_000));
            anyhow::ensure!(since <= until, "usage start must be before its end");
            let mut value = usage_value(&state.store, since, until)?;
            let rows = value["rows"].as_array_mut().expect("usage rows");
            let has_more = rows.len() > limit;
            rows.truncate(limit);
            let revision = hex::encode(Sha256::digest(value.to_string().as_bytes()));
            return Ok((
                vec![
                    json!({"id":"usage/current", "kind":"usage", "revision":revision,
                "updated_at":snapshot.created_at, "value":value}),
                ],
                has_more,
            ));
        }
        "machines" => machine_resources(state, false, snapshot, session)?,
        "devices" => {
            let person = person_filter(session, request.person.as_deref())
                .map_err(|error| anyhow::anyhow!(error.message))?
                .filter(|person| person.starts_with("person/"))
                .ok_or_else(|| {
                    anyhow::anyhow!("device inventory requires an explicitly authenticated person")
                })?;
            let mut rows = device_resources(state, snapshot, &person)
                .map_err(|error| anyhow::anyhow!(error.message))?;
            rows.retain(|row| row["state"] == "active");
            rows
        }
        "sessions" => {
            let managed = managed_native_session_ids(&state.store, snapshot.store_index)?;
            let discovery = crate::external_sessions::discover_fresh(
                state.native_session_home.as_deref(),
                false,
            )?;
            let mut rows = discovery
                .sessions
                .into_iter()
                .filter(|session| !managed.contains(&session.native_id))
                .map(external_session_resource)
                .collect::<Vec<_>>();
            rows.extend(
                discovery
                    .unresolved_processes
                    .into_iter()
                    .map(|process| unresolved_session_resource(process, &snapshot.created_at)),
            );
            rows.sort_by(|left, right| {
                right["updated_at"]
                    .as_str()
                    .cmp(&left["updated_at"].as_str())
                    .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
            });
            rows
        }
        "terminals" => {
            let mut rows = terminal_resources_for_owner(state, false, snapshot, session, None)?;
            rows.retain(|row| row["terminal_id"].is_string());
            rows
        }
        _ => unreachable!(),
    };
    if request.collection == "devices"
        && let Some(deadline) = &request.inventory_deadline
    {
        let expiry = items
            .iter()
            .filter_map(|row| row["expires_at"].as_str())
            .filter_map(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.timestamp_millis().max(0) as u128)
            .min();
        let mut deadline = deadline.lock().expect("inventory deadline");
        *deadline = deadline.iter().copied().chain(expiry).min();
    }
    if let Some(processes) = &request.native_processes {
        *processes.lock().expect("native process coverage") = items
            .iter()
            .filter_map(|row| row.pointer("/process/pid").and_then(Value::as_u64))
            .map(|pid| pid as u32)
            .collect();
    }
    let has_more = items.len() > limit;
    items.truncate(limit);
    Ok((items, has_more))
}

/// Schedule actual membership expirations, without a timer that rereads idle inventories.
pub fn next_expiry(subscription: &CollectionSubscription) -> Option<u128> {
    subscription
        .request
        .inventory_deadline
        .as_ref()
        .and_then(|deadline| *deadline.lock().expect("inventory deadline"))
}

/// Watch native inventory files while a session list is held. Missing roots are watched at
/// their nearest existing ancestor, without recursively watching an unrelated home tree.
pub struct NativeWatch {
    watcher: notify::RecommendedWatcher,
    pub processes_in_read: Arc<Mutex<Vec<u32>>>,
    #[cfg(target_os = "linux")]
    processes: BTreeMap<u32, tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>,
    home: PathBuf,
    paths: BTreeMap<PathBuf, notify::RecursiveMode>,
    events: tokio::sync::watch::Receiver<Result<(), String>>,
}

impl NativeWatch {
    pub fn open(home: &Path) -> anyhow::Result<Self> {
        let (sender, events) = tokio::sync::watch::channel(Ok(()));
        let roots = native_roots(home);
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            match event {
                Ok(event) => {
                    if matches!(event.kind, notify::EventKind::Access(_)) {
                        return;
                    }
                    // Ancestors admit newly created provider roots; other home files do not wake us.
                    if !event.paths.iter().any(|path| {
                        roots
                            .iter()
                            .any(|root| path.starts_with(root) || root.starts_with(path))
                    }) {
                        return;
                    }
                    // A later event must not erase a source failure before the socket sees it.
                    sender.send_if_modified(|held| held.is_ok());
                }
                Err(error) => {
                    let _ = sender.send_replace(Err(error.to_string()));
                }
            }
        })?;
        let mut watch = Self {
            watcher,
            processes_in_read: Arc::new(Mutex::new(Vec::new())),
            #[cfg(target_os = "linux")]
            processes: BTreeMap::new(),
            home: home.to_owned(),
            paths: BTreeMap::new(),
            events,
        };
        watch.refresh()?;
        Ok(watch)
    }

    fn refresh(&mut self) -> anyhow::Result<()> {
        use notify::Watcher as _;
        let mut paths = BTreeMap::new();
        for root in self.roots() {
            let mut path = root.clone();
            while !path.is_dir() {
                path = path
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("native session home has no existing ancestor"))?
                    .to_owned();
            }
            let mode = if path == root {
                notify::RecursiveMode::Recursive
            } else {
                notify::RecursiveMode::NonRecursive
            };
            paths
                .entry(path)
                .and_modify(|held| {
                    if mode == notify::RecursiveMode::Recursive {
                        *held = mode;
                    }
                })
                .or_insert(mode);
        }
        for (path, mode) in &paths {
            if self.paths.get(path) != Some(mode) {
                self.watcher.watch(path, *mode)?;
            }
        }
        for path in self.paths.keys().filter(|path| !paths.contains_key(*path)) {
            self.watcher.unwatch(path)?;
        }
        self.paths = paths;
        Ok(())
    }

    pub fn observe_processes(&mut self) -> anyhow::Result<bool> {
        let mut exited = false;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd as _;
            let pids = self
                .processes_in_read
                .lock()
                .expect("native process coverage")
                .iter()
                .copied()
                .collect::<BTreeSet<_>>();
            self.processes.retain(|pid, _| pids.contains(pid));
            for pid in pids {
                if self.processes.contains_key(&pid) {
                    continue;
                }
                let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
                if fd < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::ESRCH) {
                        exited = true;
                        continue;
                    }
                    return Err(error.into());
                }
                let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
                self.processes
                    .insert(pid, tokio::io::unix::AsyncFd::new(fd)?);
            }
        }
        Ok(exited)
    }

    pub async fn changed(&mut self) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        let mut exits = self
            .processes
            .iter()
            .map(|(pid, fd)| async move { fd.readable().await.map(|_| *pid) })
            .collect::<futures_util::stream::FuturesUnordered<_>>();
        #[cfg(target_os = "linux")]
        let has_exits = !exits.is_empty();
        #[cfg(not(target_os = "linux"))]
        let (mut exits, has_exits) = (
            futures_util::stream::pending::<std::io::Result<u32>>(),
            false,
        );
        let exit = tokio::select! {
            event = self.events.changed() => {
                event?;
                self.events.borrow_and_update().clone().map_err(anyhow::Error::msg)?;
                None
            }
            pid = exits.next(), if has_exits => Some(pid.expect("process exit watcher")?),
        };
        drop(exits);
        #[cfg(target_os = "linux")]
        if let Some(pid) = exit {
            self.processes.remove(&pid);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = exit;
        self.refresh()
    }

    fn roots(&self) -> Vec<PathBuf> {
        native_roots(&self.home)
    }
}

fn native_roots(home: &Path) -> Vec<PathBuf> {
    [
        ".codex/sessions",
        ".claude/projects",
        ".claude/sessions",
        ".pi/agent/sessions",
        ".omp/agent/sessions",
        ".oh-omp/agent/sessions",
        ".local/share/opencode",
    ]
    .iter()
    .map(|suffix| home.join(suffix))
    .collect()
}
