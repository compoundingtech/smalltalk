//! Filesystem watch filtering shared by the supervisor and DING sidecars.
//!
//! Linux `notify` subscribes to inotify `OPEN` events and reports them as [`EventKind::Access`].
//! Forwarding those events makes a reader self-wake forever: each pass reads its watched tree, that
//! read emits another event, and the next timed wait returns immediately. Only mutations are useful
//! wakeups; timer polling remains the fallback for anything a backend cannot classify.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Watch a directory recursively, forwarding only events that can change reconciled state.
///
/// Recursive registration eagerly walks the tree (one inotify watch per directory), so this is
/// only sound over st2-owned, payload-free directories. Symlink following is disabled: a link
/// must never carry traversal outside the watched boundary.
pub(crate) fn watch_recursive_mutations(
    dir: &Path,
    tx: Sender<()>,
) -> Option<notify::RecommendedWatcher> {
    let mut watcher = recommended_watcher(move |result: notify::Result<Event>| {
        if result.is_ok_and(|event| is_mutation(&event)) {
            let _ = tx.send(());
        }
    })
    .ok()?;
    watcher.watch(dir, RecursiveMode::Recursive).ok()?;
    Some(watcher)
}

/// Watch only the inputs a native delivery pump consumes: the agent's `resources/inbox` subtree
/// and its `status` file. Runtime records written beside them by the pump's own process group —
/// the presence refresh's temp siblings, `harness-state`, `harness-context`, stream state — must
/// never wake delivery, or a writer that observes on every turn boundary pumps itself
/// continuously. The allowlist is what makes that free: a new sibling record is ignored with no
/// production change (HC-R08).
pub(crate) fn watch_delivery_inputs(
    agent_dir: &Path,
    tx: Sender<()>,
) -> Option<notify::RecommendedWatcher> {
    let inbox = agent_dir.join("resources").join("inbox");
    let inbox_for_callback = inbox.clone();
    let status = agent_dir.join("status");
    let mut watcher = recommended_watcher(move |result: notify::Result<Event>| {
        if result.is_ok_and(|event| {
            is_mutation(&event)
                && event
                    .paths
                    .iter()
                    .any(|path| path.starts_with(&inbox_for_callback) || *path == status)
        }) {
            let _ = tx.send(());
        }
    })
    .ok()?;
    // Two shallow subscriptions instead of one recursive walk over the agent dir: Resource
    // payload trees live beside the inbox under `resources/`, and a recursive subscription pays
    // one inotify watch per payload directory before any filtering ever runs. The agent-dir watch
    // stays shallow because everything beside `status` is runtime state; the inbox itself is
    // st2-owned and message-flat, so its recursion is bounded by delivery traffic, not payloads.
    let inbox_for_watch = inbox;
    watcher.watch(agent_dir, RecursiveMode::NonRecursive).ok()?;
    watcher
        .watch(&inbox_for_watch, RecursiveMode::Recursive)
        .ok()?;
    Some(watcher)
}

/// A [`notify::RecommendedWatcher`] that never follows symlinks while walking.
fn recommended_watcher(
    handler: impl FnMut(notify::Result<Event>) + Send + 'static,
) -> notify::Result<notify::RecommendedWatcher> {
    notify::RecommendedWatcher::new(
        handler,
        notify::Config::default().with_follow_symlinks(false),
    )
}

/// A shallow subscription over the directories that can contain declarations.
///
/// `notify` implements a recursive Linux watch by eagerly walking the entire tree and allocating
/// one inotify watch per directory BEFORE the first callback ever runs. A production catalog also
/// contains unbounded Resource payload trees, so a recursive subscription spends all of startup
/// walking data the callback would later ignore — and fails outright once the walk exhausts
/// kernel limits. Keep one non-recursive watch per declaration-space directory instead;
/// [`CatalogDeclarationWatcher::refresh`] runs after each reconcile pass so a newly created
/// directory becomes watched before later edits inside it.
pub(crate) struct CatalogDeclarationWatcher {
    root: PathBuf,
    watcher: RecommendedWatcher,
    watched: Arc<Mutex<BTreeMap<PathBuf, Option<DirIdentity>>>>,
    failed: BTreeSet<PathBuf>,
}

/// Identity of a watched directory. A backend watch attaches to an inode, not a name, so a
/// directory deleted and recreated at the same pathname is a different subscription target.
/// Include ctime as the inode generation discriminator: filesystems may immediately reuse an inode
/// number, but recreating that directory still changes its metadata-change generation.
#[cfg(unix)]
type DirIdentity = (u64, u64, i64, i64);
#[cfg(not(unix))]
type DirIdentity = ();

#[cfg(unix)]
fn dir_identity(path: &Path) -> Option<DirIdentity> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()))
}

#[cfg(not(unix))]
fn dir_identity(path: &Path) -> Option<DirIdentity> {
    fs::metadata(path).ok().map(|_| ())
}

impl CatalogDeclarationWatcher {
    fn new(root: &Path, tx: Sender<()>) -> notify::Result<Self> {
        let callback_root = root.to_path_buf();
        let watched = Arc::new(Mutex::new(BTreeMap::new()));
        let invalidator = Arc::clone(&watched);
        let watcher =
            notify::recommended_watcher(move |result: notify::Result<Event>| match result {
                Ok(event) => {
                    invalidate_removed_dirs(&invalidator, &event);
                    if should_wake_catalog(&callback_root, &event) {
                        let _ = tx.send(());
                    }
                }
                Err(error) => {
                    tracing::warn!("st2: catalog-declaration watcher error: {error}");
                    let _ = tx.send(());
                }
            })?;
        let mut this = Self {
            root: root.to_path_buf(),
            watcher,
            watched,
            failed: BTreeSet::new(),
        };
        this.watcher.watch(root, RecursiveMode::NonRecursive)?;
        let mut watched = this
            .watched
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        watched.insert(root.to_path_buf(), dir_identity(root));
        drop(watched);
        this.refresh();
        Ok(this)
    }

    /// Reconcile the shallow subscriptions with the current declaration namespace.
    pub(crate) fn refresh(&mut self) {
        let desired = declaration_watch_dirs(&self.root);
        let mut watched = self.watched.lock().unwrap_or_else(|p| p.into_inner());

        // An inotify watch dies with its inode. A directory deleted and recreated at the same
        // pathname can even reuse the identity, so `invalidate_removed_dirs` drops removals from
        // this set eagerly; identity comparison here then catches anything the events missed.
        let mut stale = Vec::new();
        watched.retain(|path, identity| match desired.get(path) {
            Some(fresh) if *fresh == *identity => true,
            _ => {
                stale.push(path.clone());
                false
            }
        });

        // Stage additions without publishing them yet. A stale watch's queued removal callback
        // must finish before the same pathname is reserved for its replacement, or that old event
        // can consume the new reservation and make refresh discard a successfully registered watch.
        let additions = desired
            .into_iter()
            .filter(|(path, _)| !watched.contains_key(path))
            .collect::<Vec<_>>();
        drop(watched);

        // notify may synchronously wait for its event loop while registering or unregistering.
        // Never call it while holding the map mutex: the event-loop callback also needs that mutex
        // to invalidate a removed directory, so doing both at once can deadlock watcher teardown.
        for path in &stale {
            // Backends normally discard a watch when its directory disappears. An explicit
            // best-effort unwatch also handles moves that leave the watched inode alive elsewhere.
            // The synchronous backend call is also the ordering boundary after which callbacks for
            // the stale registration have run, so only then may this pathname represent a new watch.
            let _ = self.watcher.unwatch(path);
        }
        for (added, expected_identity) in additions {
            let mut watched = self.watched.lock().unwrap_or_else(|p| p.into_inner());
            if watched.contains_key(&added) {
                continue;
            }
            watched.insert(added.clone(), None);
            drop(watched);
            match self.watcher.watch(&added, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.failed.remove(&added);
                    let fresh_identity = dir_identity(&added);
                    let mut watched = self.watched.lock().unwrap_or_else(|p| p.into_inner());
                    let pending = watched.get(&added).is_some_and(Option::is_none);
                    let registered =
                        pending && fresh_identity.is_some() && fresh_identity == expected_identity;
                    if registered {
                        watched.insert(added.clone(), fresh_identity);
                    } else if pending {
                        watched.remove(&added);
                    }
                    drop(watched);

                    // The directory vanished or was replaced during registration, or its queued
                    // removal callback already consumed the reservation. Discard this backend
                    // watch and let the next refresh retry from a fresh identity.
                    if !registered {
                        let _ = self.watcher.unwatch(&added);
                    }
                }
                Err(error) => {
                    let mut watched = self.watched.lock().unwrap_or_else(|p| p.into_inner());
                    if watched.get(&added).is_some_and(Option::is_none) {
                        watched.remove(&added);
                    }
                    drop(watched);
                    if self.failed.insert(added.clone()) {
                        tracing::warn!(
                            "st2: cannot watch catalog declaration directory '{}': {error}; immediate changes below it are unavailable, continuing with timer polling.",
                            added.display()
                        );
                    }
                }
            }
        }
    }

    #[cfg(test)]
    fn watched_dirs(&self) -> BTreeSet<PathBuf> {
        self.watched
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

/// Independent wake channels for cooperative catalog commits and direct declaration edits.
///
/// The generation channel uses its own backend instance and one shallow control-plane watch. A
/// declaration watcher with many subscriptions can therefore degrade without delaying a completed
/// st2 publication until the timer fallback.
pub(crate) struct CatalogReconcileWatcher {
    root: PathBuf,
    tx: Sender<()>,
    generation: Option<RecommendedWatcher>,
    generation_identity: Option<DirIdentity>,
    declarations: Option<CatalogDeclarationWatcher>,
    generation_install_failed: bool,
    declaration_install_failed: bool,
}

impl CatalogReconcileWatcher {
    fn new(root: &Path, tx: Sender<()>) -> Self {
        let mut this = Self {
            root: root.to_path_buf(),
            tx,
            generation: None,
            generation_identity: None,
            declarations: None,
            generation_install_failed: false,
            declaration_install_failed: false,
        };
        this.refresh();
        this
    }

    pub(crate) fn refresh(&mut self) {
        let control = self.root.join(crate::catalog_lock::CONTROL_DIR);
        if self.generation.is_some() && dir_identity(&control) != self.generation_identity {
            self.generation = None;
            self.generation_identity = None;
            if !self.generation_install_failed {
                tracing::warn!(
                    "st2: catalog control directory was replaced; reinstalling the catalog-generation watcher."
                );
            }
            self.generation_install_failed = true;
        }

        if self.generation.is_none() {
            match watch_catalog_generation(&self.root, self.tx.clone()) {
                Ok(Some((watcher, identity))) => {
                    self.generation = Some(watcher);
                    self.generation_identity = Some(identity);
                    self.generation_install_failed = false;
                }
                Ok(None) => {}
                Err(error) => {
                    if !self.generation_install_failed {
                        tracing::warn!(
                            "st2: cannot watch the catalog generation: {error}; completed catalog transactions depend on declaration watches or timer polling."
                        );
                    }
                    self.generation_install_failed = true;
                }
            }
        }

        match &mut self.declarations {
            Some(watcher) => watcher.refresh(),
            None => match watch_catalog_declarations(&self.root, self.tx.clone()) {
                Ok(watcher) => {
                    self.declarations = Some(watcher);
                    self.declaration_install_failed = false;
                }
                Err(error) => {
                    if !self.declaration_install_failed {
                        tracing::warn!(
                            "st2: cannot watch catalog declarations: {error}; direct declaration changes depend on catalog-generation wakeups or timer polling."
                        );
                    }
                    self.declaration_install_failed = true;
                }
            },
        }
    }
}

fn watch_catalog_generation(
    root: &Path,
    tx: Sender<()>,
) -> notify::Result<Option<(RecommendedWatcher, DirIdentity)>> {
    let control = root.join(crate::catalog_lock::CONTROL_DIR);
    if !control.is_dir() {
        return Ok(None);
    }
    let Some(identity) = dir_identity(&control) else {
        return Ok(None);
    };
    let generation = control.join(crate::catalog_lock::GENERATION_FILE);
    let mut watcher = recommended_watcher(move |result: notify::Result<Event>| match result {
        Ok(event) if is_mutation(&event) && event.paths.iter().any(|path| *path == generation) => {
            let _ = tx.send(());
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!("st2: catalog-generation watcher error: {error}");
            let _ = tx.send(());
        }
    })?;
    watcher.watch(&control, RecursiveMode::NonRecursive)?;
    if dir_identity(&control) != Some(identity) {
        let _ = watcher.unwatch(&control);
        return Ok(None);
    }
    Ok(Some((watcher, identity)))
}

pub(crate) fn watch_catalog_reconcile_inputs(
    root: &Path,
    tx: Sender<()>,
) -> CatalogReconcileWatcher {
    CatalogReconcileWatcher::new(root, tx)
}

/// Invalidate a removed subscription without letting its delayed callback erase a replacement
/// already registered at the same pathname. A live identity equal to the recorded identity proves
/// the event belongs to an older registration; a `None` value is refresh's in-flight reservation,
/// whose before/after identity check owns rollback if the directory changes during registration.
fn invalidate_removed_dirs(watched: &Mutex<BTreeMap<PathBuf, Option<DirIdentity>>>, event: &Event) {
    let torn_down = matches!(
        &event.kind,
        EventKind::Remove(RemoveKind::Folder | RemoveKind::Any | RemoveKind::Other)
            | EventKind::Modify(ModifyKind::Name(RenameMode::From))
    );
    if !torn_down {
        return;
    }
    let mut watched = watched
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for path in &event.paths {
        // Removing or moving away a parent retires every watch beneath it too, except for a
        // replacement that refresh has already registered with a new, still-live identity.
        watched.retain(|tracked, identity| {
            if !tracked.starts_with(path) {
                return true;
            }
            match identity {
                None => true,
                Some(recorded) => dir_identity(tracked).is_some_and(|current| current == *recorded),
            }
        });
    }
}

/// Watch only declaration inputs for the supervisor. Runtime state (PTY registry, bus, logs,
/// locks, inboxes, and generated materializations) must never be traversed or wake reconciliation.
pub(crate) fn watch_catalog_declarations(
    root: &Path,
    tx: Sender<()>,
) -> notify::Result<CatalogDeclarationWatcher> {
    CatalogDeclarationWatcher::new(root, tx)
}

/// Every directory that can hold declarations, discovered WITHOUT descending into Resource
/// payloads: recursion is gated by [`agent_spec::is_catalog_path`], so a declaration parent's
/// `resources`/`archive`/`inbox` children and `.git`/`.st2` control dirs prune the walk.
fn declaration_watch_dirs(root: &Path) -> BTreeMap<PathBuf, Option<DirIdentity>> {
    fn collect(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Option<DirIdentity>>) {
        out.insert(dir.to_path_buf(), dir_identity(dir));
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir())
                && agent_spec::is_catalog_path(root, &path)
            {
                collect(root, &path, out);
            }
        }
    }

    let mut dirs = BTreeMap::new();
    collect(root, root, &mut dirs);
    dirs
}

fn should_wake_catalog(root: &Path, event: &Event) -> bool {
    if !is_mutation(event) {
        return false;
    }
    event.paths.iter().any(|path| {
        is_declaration_path(root, path)
            || (is_directory_topology_mutation(event) && agent_spec::is_catalog_path(root, path))
    })
}

/// Directory-level topology changes must wake even though no `agent.kdl` path exists yet — the
/// created directory may receive one next, and the watcher needs `refresh` anyway. FILE-level
/// create/remove/rename stays silent: a scratch, log, or editor-swap file in declaration space
/// must not wake a full-catalog reconcile. Linux classifies creates/removals by entry type;
/// renames carry no type, so the arrived side is checked against the live tree. A rename AWAY
/// from the catalog has no surviving entry to inspect and stays silent here — an in-catalog
/// rename also emits the arrived side, and a full removal is bounded by the timer plus the
/// next reconciliation's `refresh`.
fn is_directory_topology_mutation(event: &Event) -> bool {
    match &event.kind {
        EventKind::Create(CreateKind::Folder) => true,
        EventKind::Remove(RemoveKind::Folder | RemoveKind::Any | RemoveKind::Other) => true,
        EventKind::Modify(ModifyKind::Name(_)) => event.paths.iter().any(|p| p.is_dir()),
        _ => false,
    }
}

fn is_declaration_path(root: &Path, path: &Path) -> bool {
    if !agent_spec::is_catalog_path(root, path) {
        return false;
    }
    let rel = path.strip_prefix(root).unwrap_or(path);
    if rel == Path::new("catalog.kdl") {
        return true;
    }
    let mut components = rel.components();
    if matches!(
        components.next().and_then(|c| c.as_os_str().to_str()),
        Some("_templates")
    ) {
        return true;
    }
    path.file_name().and_then(|n| n.to_str()) == Some("agent.kdl")
}

fn is_mutation(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode,
    };

    #[test]
    fn only_mutations_wake_watch_loops() {
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            EventKind::Remove(RemoveKind::File),
        ] {
            assert!(is_mutation(&Event::new(kind)), "{kind:?} must wake");
        }

        for kind in [
            EventKind::Access(AccessKind::Open(AccessMode::Read)),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Any,
            EventKind::Other,
        ] {
            assert!(!is_mutation(&Event::new(kind)), "{kind:?} must stay quiet");
        }
    }

    #[test]
    fn declaration_filter_uses_catalog_scoped_namespace_semantics() {
        let catalog = tempfile::tempdir().unwrap();
        let root = catalog.path();
        std::fs::create_dir_all(root.join("agents/h/live")).unwrap();
        std::fs::write(
            root.join("agents/h/live/agent.kdl"),
            r#"agent "live" { host "h"; command "x" }"#,
        )
        .unwrap();

        assert!(is_declaration_path(
            root,
            &root.join("teams/.managed/project/agent.kdl")
        ));
        assert!(is_declaration_path(
            root,
            &root.join("teams/.retired/project/agent.kdl")
        ));
        assert!(is_declaration_path(
            root,
            &root.join("agents/archive/project/agent.kdl")
        ));
        assert!(is_declaration_path(
            root,
            &root.join("agents/resources/project/agent.kdl")
        ));
        assert!(is_declaration_path(root, &root.join("catalog.kdl")));
        assert!(is_declaration_path(root, &root.join("_templates/base.kdl")));
        assert!(!is_declaration_path(
            root,
            &root.join(".git/project/agent.kdl")
        ));
        assert!(!is_declaration_path(
            root,
            &root.join(".st2/project/agent.kdl")
        ));
        assert!(!is_declaration_path(
            root,
            &root.join("organizations/project/.git/nested/agent.kdl")
        ));
        assert!(!is_declaration_path(
            root,
            &root.join("organizations/project/.st2/nested/agent.kdl")
        ));
        assert!(!is_declaration_path(
            root,
            &root.join("pty/project/agent.kdl")
        ));
        for reserved in ["resources", "archive", "inbox"] {
            assert!(!is_declaration_path(
                root,
                &root.join(format!("agents/h/live/{reserved}/project/agent.kdl"))
            ));
        }
        assert!(!is_declaration_path(root, &root.join("team/rendered.kdl")));
    }

    /// One real harness-context write, through the record's own writer: the guard, the lock, the
    /// staging outside the agent subtree, and the rename of the canonical name in.
    #[cfg(target_os = "linux")]
    fn st2_harness_context_write(agent_dir: &Path) {
        use crate::harness_context::{Harness, Reading, Writer};
        Writer::new(agent_dir, "h.worker", Harness::Codex)
            .unwrap()
            .observe(Reading {
                used_tokens: Some(92_283),
                window_tokens: Some(258_400),
                used_percent: Some(33.0),
                ..Reading::default()
            })
            .unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn delivery_watcher_ignores_runtime_records_but_wakes_on_inbox_and_status() {
        use std::sync::mpsc::channel;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agents/h/worker");
        std::fs::create_dir_all(agent_dir.join("resources/inbox")).unwrap();

        let (tx, rx) = channel();
        let _watcher = watch_delivery_inputs(&agent_dir, tx).expect("start inotify watcher");

        // Runtime records the pump's own process group writes must stay silent: the observed
        // harness state and the harness context beside it, their locks and atomic temp siblings,
        // the presence temp sibling, stream state.
        std::fs::write(agent_dir.join("harness-state"), "{}").unwrap();
        std::fs::write(agent_dir.join(".harness-state.tmp-1-0"), "{}").unwrap();
        std::fs::write(agent_dir.join("harness-context"), "{}").unwrap();
        std::fs::write(agent_dir.join(".harness-context.lock"), "").unwrap();
        std::fs::write(agent_dir.join(".status.tmp-1-0"), "available\n").unwrap();
        std::fs::create_dir_all(agent_dir.join("resources/streams/s")).unwrap();
        std::fs::write(agent_dir.join("resources/streams/s/state.json"), "{}").unwrap();
        // …and a real harness-context write through its own writer, staging outside the agent
        // subtree and renaming the record in, which is the shape a producer actually produces.
        st2_harness_context_write(&agent_dir);
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "runtime-record writes must not wake the delivery pump"
        );

        // The two genuine delivery inputs wake it: an inbox arrival…
        std::fs::write(agent_dir.join("resources/inbox/0001-msg.md"), "hi").unwrap();
        rx.recv_timeout(Duration::from_secs(1))
            .expect("inbox write must wake");
        while rx.try_recv().is_ok() {}

        // …and a presence change, including one landing via atomic tmp+rename.
        std::fs::rename(agent_dir.join(".status.tmp-1-0"), agent_dir.join("status")).unwrap();
        rx.recv_timeout(Duration::from_secs(1))
            .expect("status rename must wake");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn delivery_watch_is_bounded_by_inputs_not_payload_depth() {
        use std::sync::mpsc::channel;

        fn inotify_watch_count() -> usize {
            std::fs::read_dir("/proc/self/fdinfo")
                .unwrap()
                .flatten()
                .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
                .filter(|content| content.contains("inotify wd:"))
                .map(|content| content.lines().count())
                .sum()
        }

        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path();
        let payload = agent_dir.join("resources/worktree/node_modules");
        for i in 0..300 {
            std::fs::create_dir_all(payload.join(format!("pkg-{i}/dist/sub"))).unwrap();
        }
        std::fs::create_dir_all(agent_dir.join("resources/inbox")).unwrap();

        let before = inotify_watch_count();
        let (tx, _rx) = channel();
        let _watcher = watch_delivery_inputs(agent_dir, tx).expect("start delivery watcher");
        // The count is host-wide, so a concurrent process releasing watches can make it fall
        // between the two reads. That cannot mean this watcher allocated any, so saturate: an
        // unsigned wrap would panic the assertion below with u64::MAX, blaming the watcher for
        // another process's teardown.
        let delta = inotify_watch_count().saturating_sub(before);
        assert!(
            delta < 32,
            "payload depth must not drive watch allocation: {delta} watches for a \
             900-directory payload tree"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_reads_are_silent_but_real_mutations_wake() {
        use std::sync::mpsc::channel;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("watched");
        std::fs::write(&file, "before").unwrap();

        let (tx, rx) = channel();
        let _watcher = watch_recursive_mutations(dir.path(), tx).expect("start inotify watcher");

        for _ in 0..1_000 {
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "OPEN/read access must not wake a mutation watcher"
        );

        std::fs::write(&file, "after").unwrap();
        rx.recv_timeout(Duration::from_secs(1))
            .expect("content mutation must wake");

        fn assert_wakes(setup: fn(&Path), mutate: fn(&Path)) {
            let dir = tempfile::tempdir().unwrap();
            setup(dir.path());
            let (tx, rx) = channel();
            let _watcher =
                watch_recursive_mutations(dir.path(), tx).expect("start inotify watcher");
            mutate(dir.path());
            rx.recv_timeout(Duration::from_secs(1))
                .expect("filesystem mutation must wake");
        }

        assert_wakes(
            |_| {},
            |dir| std::fs::write(dir.join("created"), "value").unwrap(),
        );
        assert_wakes(
            |dir| std::fs::write(dir.join("renamed-from"), "value").unwrap(),
            |dir| {
                std::fs::rename(dir.join("renamed-from"), dir.join("renamed-to")).unwrap();
            },
        );
        assert_wakes(
            |dir| std::fs::write(dir.join("removed"), "value").unwrap(),
            |dir| std::fs::remove_file(dir.join("removed")).unwrap(),
        );
    }

    #[test]
    fn declaration_watch_tree_stops_before_resource_payloads() {
        let catalog = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = catalog.path();
        let agent = root.join("agents/h/live");
        std::fs::create_dir_all(agent.join("resources/cache/a/b/c/d/e")).unwrap();
        std::fs::create_dir_all(agent.join("inbox/archive/a/b/c")).unwrap();
        std::fs::create_dir_all(outside.path().join("worktree/node_modules/a/b/c/d/e")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.path().join("worktree"),
            agent.join("resources/worktree"),
        )
        .unwrap();
        std::fs::write(
            agent.join("agent.kdl"),
            r#"agent "live" { host "h"; command "x" }"#,
        )
        .unwrap();

        let watched = declaration_watch_dirs(root);
        assert_eq!(
            watched.keys().cloned().collect::<BTreeSet<_>>(),
            [
                root.to_path_buf(),
                root.join("agents"),
                root.join("agents/h"),
                agent,
            ]
            .into_iter()
            .collect()
        );
        assert!(
            watched.keys().all(|path| !path.starts_with(outside.path())),
            "Resource worktree links must not escape the catalog watch boundary"
        );
    }

    #[test]
    fn topology_wakes_are_directory_scoped() {
        use notify::event::{AccessKind, AccessMode};

        let file_create = Event::new(EventKind::Create(CreateKind::File))
            .add_path(PathBuf::from("/cat/agents/h/live/scratch.log"));
        let dir_create = Event::new(EventKind::Create(CreateKind::Folder))
            .add_path(PathBuf::from("/cat/agents/h/newdir"));
        let file_remove = Event::new(EventKind::Remove(RemoveKind::File))
            .add_path(PathBuf::from("/cat/agents/h/live/scratch.log"));
        let dir_remove = Event::new(EventKind::Remove(RemoveKind::Folder))
            .add_path(PathBuf::from("/cat/agents/h/gone"));
        let file_rename_in = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To)))
            .add_path(PathBuf::from("/cat/agents/h/live/renamed.txt"));
        let rename_away = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
            .add_path(PathBuf::from("/cat/agents/h/live/.swap.swp"));

        for quiet in [file_create, file_remove, file_rename_in, rename_away] {
            assert!(
                !is_directory_topology_mutation(&quiet),
                "{quiet:?}: a FILE topology event must not wake the catalog"
            );
        }
        for loud in [dir_create, dir_remove] {
            assert!(
                is_directory_topology_mutation(&loud),
                "{loud:?}: a DIRECTORY-level topology event must wake the catalog"
            );
        }

        // Sanity: access events never wake regardless of classification.
        let read = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Read)))
            .add_path(PathBuf::from("/cat/agents/h/newdir"));
        assert!(!is_directory_topology_mutation(&read));
    }

    #[test]
    fn watcher_installation_preserves_backend_errors() {
        let parent = tempfile::tempdir().unwrap();
        let missing_catalog = parent.path().join("missing");
        let (tx, _rx) = std::sync::mpsc::channel();
        assert!(
            watch_catalog_declarations(&missing_catalog, tx).is_err(),
            "a missing root must return the installation error"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn catalog_watch_is_bounded_and_refreshes_new_declaration_directories() {
        use std::sync::mpsc::channel;
        use std::time::Duration;

        let catalog = tempfile::tempdir().unwrap();
        let root = catalog.path();
        let agent = root.join("agents/h/live");
        std::fs::create_dir_all(agent.join("resources/cache/a/b/c/d/e")).unwrap();
        std::fs::write(
            agent.join("agent.kdl"),
            r#"agent "live" { host "h"; command "x" }"#,
        )
        .unwrap();

        let (tx, rx) = channel();
        let mut watcher = watch_catalog_declarations(root, tx).expect("start catalog watcher");
        assert_eq!(
            watcher.watched_dirs().len(),
            4,
            "Resource depth must not add watches"
        );

        std::fs::write(
            agent.join("agent.kdl"),
            r#"agent "live" { host "h"; command "changed" }"#,
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(1))
            .expect("declaration update must wake");
        while rx.try_recv().is_ok() {}

        std::fs::write(root.join("catalog.kdl"), "catalog {}\n").unwrap();
        rx.recv_timeout(Duration::from_secs(1))
            .expect("root catalog configuration must wake");
        while rx.try_recv().is_ok() {}

        let nested = root.join("teams/new/live");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("agent.kdl"),
            r#"agent "live" { host "new"; command "x" }"#,
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(1))
            .expect("new declaration directory must wake its watched ancestor");
        while rx.try_recv().is_ok() {}
        watcher.refresh();
        assert!(watcher.watched_dirs().contains(&nested));

        std::fs::write(
            nested.join("agent.kdl"),
            r#"agent "live" { host "new"; command "changed" }"#,
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(1))
            .expect("declaration update after refresh must wake");
        while rx.try_recv().is_ok() {}

        std::fs::remove_file(nested.join("agent.kdl")).unwrap();
        rx.recv_timeout(Duration::from_secs(1))
            .expect("declaration removal must wake");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_agent_bundle_publication_wakes_production_shaped_catalog() {
        use std::sync::mpsc::channel;
        use std::time::Duration;

        let catalog = tempfile::tempdir().unwrap();
        let root = catalog.path();
        let host = root.join("agents/h");
        let staged = root.join(".st2/staged");
        for index in 0..750 {
            let agent = host.join(format!("existing-{index}"));
            std::fs::create_dir_all(&agent).unwrap();
            std::fs::write(
                agent.join("agent.kdl"),
                format!(r#"agent "existing-{index}" {{ host "h"; command "x" }}"#),
            )
            .unwrap();
        }
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(
            staged.join("agent.kdl"),
            r#"agent "new" { host "h"; command "x" }"#,
        )
        .unwrap();

        let (tx, rx) = channel();
        let _watcher = watch_catalog_reconcile_inputs(root, tx);

        std::fs::rename(&staged, host.join("new")).unwrap();
        rx.recv_timeout(Duration::from_secs(1))
            .expect("atomic bundle publication must wake");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn catalog_generation_commit_wakes_catalog_watch() {
        use std::sync::mpsc::channel;
        use std::time::Duration;

        let catalog = tempfile::tempdir().unwrap();
        let control = catalog.path().join(crate::catalog_lock::CONTROL_DIR);
        std::fs::create_dir(&control).unwrap();
        std::fs::write(control.join(crate::catalog_lock::GENERATION_FILE), "1\n").unwrap();
        let (tx, rx) = channel();
        let _watcher = watch_catalog_reconcile_inputs(catalog.path(), tx);

        std::fs::write(control.join("next-generation"), "2\n").unwrap();
        std::fs::rename(
            control.join("next-generation"),
            control.join(crate::catalog_lock::GENERATION_FILE),
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(1))
            .expect("catalog generation commit must wake");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn catalog_generation_watch_reinstalls_after_control_directory_replacement() {
        use std::sync::mpsc::channel;

        let catalog = tempfile::tempdir().unwrap();
        let control = catalog.path().join(crate::catalog_lock::CONTROL_DIR);
        std::fs::create_dir(&control).unwrap();
        std::fs::write(control.join(crate::catalog_lock::GENERATION_FILE), "1\n").unwrap();
        let (tx, _rx) = channel();
        let mut watcher = watch_catalog_reconcile_inputs(catalog.path(), tx);
        let original = watcher.generation_identity.unwrap();

        std::fs::remove_dir_all(&control).unwrap();
        std::fs::create_dir(&control).unwrap();
        std::fs::write(control.join(crate::catalog_lock::GENERATION_FILE), "2\n").unwrap();
        watcher.refresh();

        assert!(watcher.generation.is_some());
        assert_ne!(watcher.generation_identity, Some(original));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_declaration_file_topology_stays_silent() {
        use std::sync::mpsc::channel;
        use std::time::Duration;

        let catalog = tempfile::tempdir().unwrap();
        let agent = catalog.path().join("agents/h/live");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("agent.kdl"),
            r#"agent "live" { host "h"; command "x" }"#,
        )
        .unwrap();

        let (tx, rx) = channel();
        let _watcher = watch_catalog_declarations(catalog.path(), tx).expect("start watcher");

        std::fs::write(agent.join("scratch.log"), "noise").unwrap();
        std::fs::rename(agent.join("scratch.log"), agent.join("scratch2.log")).unwrap();
        std::fs::remove_file(agent.join("scratch2.log")).unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "scratch-file create/rename/remove must not wake a full-catalog reconcile"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn replaced_declaration_directory_is_resubscribed_on_refresh() {
        use std::sync::mpsc::channel;
        use std::time::Duration;

        let catalog = tempfile::tempdir().unwrap();
        let root = catalog.path();
        let agent = root.join("agents/h/live");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("agent.kdl"),
            r#"agent "live" { host "h"; command "x" }"#,
        )
        .unwrap();

        let (tx, rx) = channel();
        let mut watcher = watch_catalog_declarations(root, tx).expect("start watcher");

        // Delete and recreate at the SAME pathname: the backend watch died with the old inode
        // while `watched` still holds the name, so only identity comparison can catch this.
        std::fs::remove_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        watcher.refresh();
        while rx.try_recv().is_ok() {}

        std::fs::write(
            agent.join("agent.kdl"),
            r#"agent "live" { host "h"; command "changed" }"#,
        )
        .unwrap();
        rx.recv_timeout(Duration::from_secs(1))
            .expect("a replaced directory must be resubscribed by refresh");
    }
}
