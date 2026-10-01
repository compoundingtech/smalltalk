//! Resident Component Model supervision for observable Resource Profiles.
//!
//! The worker owns component instances and their atomic proposal fences. Reconcile callers submit
//! complete desired binding sets; removals are acknowledged only after registrations have been
//! fenced and provider invocations no longer belong to the desired generation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
#[cfg(feature = "wasip2-provider-runtime")]
use std::time::Duration;
use std::time::{Instant, SystemTime};

use agent_spec::profile::{
    ProfileCapability, ProfileDescriptor, ResourceProfileRegistry, RuntimeTopology,
};
use agent_spec::spec::AgentSpec;
use anyhow::Context as _;
use notify::RecommendedWatcher;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
#[cfg(feature = "wasip2-provider-runtime")]
use st_resource_protocol::ProposalFence;
#[cfg(feature = "wasip2-provider-runtime")]
use st2_resource_providers::{
    GitHubIssueConfig, GitHubIssueModule, GitHubPrConfig, GitHubPrModule, PtyStatsConfig,
    PtyStatsModule, VistaConfig, VistaModule,
};
#[cfg(feature = "wasip2-provider-runtime")]
use st2_resource_wasip2::{
    Executor as Wasip2Executor, LoadedComponent, ObservationRequest as Wasip2ObservationRequest,
    ProviderDescriptor as Wasip2ProviderDescriptor, RuntimeConfig as Wasip2RuntimeConfig,
    SchedulingCapability as Wasip2SchedulingCapability,
};

use crate::catalog::CatalogConfig;
use crate::resource_observe::{
    ObservationAuthority, ObserveReceipt, ObserveReceiptStatus, ObserveRequest,
    PendingRequestRecord, prepare_scope, prune_terminal_receipts, read_receipt, remove_request,
    scan_requests, write_receipt,
};
use crate::resource_profile::{
    AcceptedObservation, AcceptedOutput, AcceptedPublication, BindingId, BindingRegistration,
    CatchUp, PublicationContract, PublicationOutcome, RegistrationToken, ResourceFact,
    RuntimeHealthState, RuntimeLifecycle, RuntimeMessage, RuntimeOwner, SnapshotDigest,
    SnapshotTarget, TopicSelection,
};
#[cfg(any(test, feature = "wasip2-provider-runtime"))]
use crate::resource_profile::{OwnerClaim, RuntimeIncarnation};

const MAILBOX_CAPACITY: usize = 64;
static ID_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceProfileHealth {
    pub scheme: String,
    pub binding: Option<String>,
    pub state: RuntimeHealthState,
    pub detail: Option<String>,
}

#[derive(Debug)]
pub struct ResourceProfileRefreshReport {
    pub warnings: Vec<String>,
}

pub struct ResourceProfileSupervisor {
    tx: SyncSender<Msg>,
    worker: Option<JoinHandle<()>>,
    catalog_root: PathBuf,
    this_host: String,
    observe_watcher: Option<RecommendedWatcher>,
    component_cache: Mutex<ComponentSnapshotCache>,
    observe_bridge: Option<JoinHandle<()>>,
}

impl ResourceProfileSupervisor {
    pub fn new(catalog_root: PathBuf, this_host: String) -> anyhow::Result<Self> {
        let catalog_root = lexical_absolute(&catalog_root)?;
        let scope = crate::park::SupervisorScope::current(&catalog_root, &this_host)?;
        prepare_scope(&scope)?;
        let request_dir = scope.observe_request_dir();
        let receipt_dir = scope.observe_receipt_dir();
        let (watch_tx, watch_rx) = mpsc::channel();
        let observe_watcher =
            crate::watch::watch_recursive_mutations(&request_dir, watch_tx).ok();
        if observe_watcher.is_none() {
            tracing::warn!(
                "Resource observation request watcher is unavailable; \
                 falling back to supervisor refreshes"
            );
        }
        let (tx, rx) = mpsc::sync_channel(MAILBOX_CAPACITY);
        let worker_root = catalog_root.clone();
        let worker_host = this_host.clone();
        let completion_tx = tx.clone();
        let worker = thread::Builder::new()
            .name("st-resource-profile".to_owned())
            .spawn(move || {
                Worker::new(
                    worker_root,
                    worker_host,
                    request_dir,
                    receipt_dir,
                    completion_tx,
                )
                .run(rx);
            })
            .context("spawn Resource Profile supervisor")?;
        let bridge_tx = tx.clone();
        let observe_bridge = thread::Builder::new()
            .name("st-resource-observe-watch".to_owned())
            .spawn(move || {
                while watch_rx.recv().is_ok() {
                    if bridge_tx.send(Msg::ObserveRequests).is_err() {
                        break;
                    }
                }
            })
            .context("spawn Resource observation request watcher")?;
        let supervisor = Self {
            tx,
            worker: Some(worker),
            component_cache: Mutex::new(ComponentSnapshotCache::default()),
            catalog_root,
            this_host,
            observe_watcher,
            observe_bridge: Some(observe_bridge),
        };
        // Watch-before-scan: when the watcher is available, a final rename racing startup is
        // either in this scan or queued by the watcher. Without a watcher, later supervisor
        // refreshes continue to scan the durable request directory.
        let _ = supervisor.tx.send(Msg::ObserveRequests);
        Ok(supervisor)
    }

    /// Reconcile the exact set of bindings owned by canonical agent seats proven live this pass.
    /// The caller holds the catalog read lock while deriving `config`, `profiles`, and `generation`.
    pub fn refresh(
        &self,
        config: &CatalogConfig,
        profiles: &ResourceProfileRegistry,
        generation: Option<u64>,
        live_specs: &[AgentSpec],
    ) -> ResourceProfileRefreshReport {
        let (desired, mut warnings) = desired_bindings(
            &self.catalog_root,
            &self.this_host,
            config,
            profiles,
            generation,
            live_specs,
            &self.component_cache,
        );
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self
            .tx
            .send(Msg::Refresh {
                desired,
                catalog_generation: generation,
                reply: reply_tx,
            })
            .is_err()
        {
            warnings.push("Resource Profile supervisor worker stopped".to_owned());
        } else if let Ok(worker_warnings) = reply_rx.recv() {
            warnings.extend(worker_warnings);
        } else {
            warnings.push("Resource Profile supervisor refresh acknowledgement failed".to_owned());
        }
        ResourceProfileRefreshReport { warnings }
    }

    /// Fence one agent before its canonical seat is replaced. Completion is synchronous.
    pub fn deactivate(&self, spec: &AgentSpec) {
        let recipient = spec.bus_id(&self.this_host);
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self
            .tx
            .send(Msg::Deactivate {
                recipient,
                reply: reply_tx,
            })
            .is_ok()
        {
            let _ = reply_rx.recv();
        }
    }

    pub fn health(&self) -> Vec<ResourceProfileHealth> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self.tx.send(Msg::Health { reply: reply_tx }).is_err() {
            return Vec::new();
        }
        reply_rx.recv().unwrap_or_default()
    }
}

impl Drop for ResourceProfileSupervisor {
    fn drop(&mut self) {
        self.observe_watcher.take();
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if self.tx.send(Msg::Shutdown { reply: reply_tx }).is_ok() {
            let _ = reply_rx.recv();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(bridge) = self.observe_bridge.take() {
            let _ = bridge.join();
        }
    }
}

#[derive(Debug)]
enum Msg {
    Refresh {
        desired: BTreeMap<String, DesiredBinding>,
        catalog_generation: Option<u64>,
        reply: SyncSender<Vec<String>>,
    },
    Deactivate {
        recipient: String,
        reply: SyncSender<()>,
    },
    Health {
        reply: SyncSender<Vec<ResourceProfileHealth>>,
    },
    ObserveRequests,
    #[cfg(feature = "wasip2-provider-runtime")]
    ObservationCompleted(ObservationCompletion),
    Shutdown {
        reply: SyncSender<()>,
    },
}
#[cfg(feature = "wasip2-provider-runtime")]
#[derive(Debug)]
struct ObservationCompletion {
    job_id: u64,
    runtime_key: RuntimeKey,
    stable_key: String,
    authority: ObservationAuthority,
    watermark: u64,
    fence: ProposalFence,
    result: Result<st_resource_protocol::ObservationResult, String>,
}

#[cfg(feature = "wasip2-provider-runtime")]
struct ObservationJob {
    runtime_key: RuntimeKey,
    stable_key: String,
    cancellation: ObservationCancellation,
    join: JoinHandle<()>,
}

#[cfg(feature = "wasip2-provider-runtime")]
struct ObservationLaunch {
    job_id: u64,
    runtime_key: RuntimeKey,
    stable_key: String,
    authority: ObservationAuthority,
    watermark: u64,
    fence: ProposalFence,
    request: Wasip2ObservationRequest,
    provider: Arc<ProviderRuntime>,
    cancellation: ObservationCancellation,
}

#[cfg(feature = "wasip2-provider-runtime")]
#[derive(Clone)]
struct ObservationCancellation {
    interruption: st2_resource_wasip2::InterruptionHandle,
}

#[cfg(feature = "wasip2-provider-runtime")]
impl ObservationCancellation {
    fn cancel(&self) {
        self.interruption.cancel();
    }
}

#[cfg(feature = "wasip2-provider-runtime")]
fn catch_observation(
    observe: impl FnOnce() -> Result<
        st_resource_protocol::ObservationResult,
        st2_resource_wasip2::ObserveError,
    >,
) -> Result<st_resource_protocol::ObservationResult, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(observe))
        .map_err(|_| "provider observation panicked".to_owned())?
        .map_err(|error| error.to_string())
}

#[derive(Debug)]
struct ProviderComponentSnapshot {
    relative: PathBuf,
    identity: String,
    #[cfg_attr(not(feature = "wasip2-provider-runtime"), allow(dead_code))]
    bytes: Arc<[u8]>,
}

impl PartialEq for ProviderComponentSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.relative == other.relative && self.identity == other.identity
    }
}

#[derive(Default)]
struct ComponentSnapshotCache {
    catalog_generation: Option<u64>,
    snapshots: BTreeMap<String, Arc<ProviderComponentSnapshot>>,
}

impl ComponentSnapshotCache {
    fn begin_refresh(&mut self, catalog_generation: Option<u64>) {
        if catalog_generation.is_none() || self.catalog_generation != catalog_generation {
            self.snapshots.clear();
            self.catalog_generation = catalog_generation;
        }
    }

    fn load(
        &mut self,
        catalog_root: &Path,
        component: &str,
    ) -> anyhow::Result<Arc<ProviderComponentSnapshot>> {
        if let Some(snapshot) = self.snapshots.get(component) {
            return Ok(Arc::clone(snapshot));
        }
        let relative = crate::catalog::resolve_provider_component(catalog_root, component)?;
        let bytes = crate::catalog_transaction::read_provider_component(catalog_root, &relative)
            .with_context(|| format!("read provider component '{}'", relative.display()))?;
        let snapshot = Arc::new(ProviderComponentSnapshot {
            relative,
            identity: format!("{:x}", Sha256::digest(&bytes)),
            bytes: Arc::from(bytes),
        });
        self.snapshots
            .insert(component.to_owned(), Arc::clone(&snapshot));
        Ok(snapshot)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct DesiredBinding {
    stable_key: String,
    catalog_root: PathBuf,
    this_host: String,
    recipient: String,
    binding_name: String,
    scheme: String,
    generation: u64,
    topology: RuntimeTopology,
    runtime: crate::catalog::DeclaredProfileRuntime,
    component: Arc<ProviderComponentSnapshot>,
    uri: String,
    selector: Value,
    descriptor: ProfileDescriptor,
    target: SnapshotTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum RuntimeKey {
    Shared {
        scheme: String,
        generation: u64,
    },
    PerBinding {
        scheme: String,
        generation: u64,
        binding: String,
    },
}

impl DesiredBinding {
    fn runtime_key(&self) -> RuntimeKey {
        match self.topology {
            RuntimeTopology::Shared => RuntimeKey::Shared {
                scheme: self.scheme.clone(),
                generation: self.generation,
            },
            RuntimeTopology::PerBinding => RuntimeKey::PerBinding {
                scheme: self.scheme.clone(),
                generation: self.generation,
                binding: hash_text(&self.stable_key),
            },
        }
    }
}

fn desired_bindings(
    catalog_root: &Path,
    this_host: &str,
    config: &CatalogConfig,
    profiles: &ResourceProfileRegistry,
    generation: Option<u64>,
    live_specs: &[AgentSpec],
    component_cache: &Mutex<ComponentSnapshotCache>,
) -> (BTreeMap<String, DesiredBinding>, Vec<String>) {
    let catalog_generation = generation.unwrap_or(0);
    let refresh = profiles.begin_refresh();
    let mut component_cache = component_cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    component_cache.begin_refresh(generation);
    let mut descriptors = BTreeMap::new();
    let mut runtimes = BTreeMap::new();
    let mut generations = BTreeMap::new();
    let mut components = BTreeMap::new();
    let mut warnings = Vec::new();
    for declared in &config.profiles {
        let Some(runtime) = declared.runtime.as_ref() else {
            continue;
        };
        match refresh.try_descriptor(&declared.scheme) {
            Ok(Some(descriptor))
                if descriptor
                    .capabilities
                    .contains(&ProfileCapability::Observe) =>
            {
                let component = match component_cache.load(catalog_root, &runtime.component) {
                    Ok(component) => component,
                    Err(error) => {
                        warnings.push(format!(
                            "Resource Profile '{}': provider component unavailable: {error:#}",
                            declared.scheme
                        ));
                        continue;
                    }
                };
                generations.insert(
                    declared.scheme.clone(),
                    profile_generation(
                        catalog_generation,
                        declared,
                        &descriptor,
                        profiles,
                        &component.identity,
                    ),
                );
                descriptors.insert(declared.scheme.clone(), descriptor);
                runtimes.insert(declared.scheme.clone(), runtime.clone());
                components.insert(declared.scheme.clone(), component);
            }
            Ok(_) => warnings.push(format!(
                "Resource Profile '{}': runtime has no observable descriptor",
                declared.scheme
            )),
            Err(error) => warnings.push(format!(
                "Resource Profile '{}': descriptor unavailable: {error}",
                declared.scheme
            )),
        }
    }

    let mut desired = BTreeMap::new();
    for spec in live_specs {
        let declaration = lexical_absolute(&spec.path).unwrap_or_else(|_| spec.path.clone());
        let agent_dir = declaration.parent().unwrap_or(Path::new("/"));
        for resource in &spec.resources {
            if resource.inactive_reason().is_some() {
                continue;
            }
            let Some((scheme, _)) = resource.uri().split_once(':') else {
                continue;
            };
            let (Some(descriptor), Some(runtime), Some(component), Some(generation)) = (
                descriptors.get(scheme),
                runtimes.get(scheme),
                components.get(scheme),
                generations.get(scheme),
            ) else {
                continue;
            };
            let selector = resource
                .selector()
                .cloned()
                .unwrap_or_else(|| descriptor.default_selector.clone());
            if let Err(error) = descriptor.validate_selector(&selector) {
                warnings.push(format!(
                    "Resource Profile '{}' binding {} resource '{}': {error}",
                    scheme,
                    spec.bus_id(this_host),
                    resource.name()
                ));
                continue;
            }
            let resolution = match refresh.try_resolve(agent_dir, resource.uri()) {
                Ok(Some(resolution)) => resolution,
                Ok(None) => {
                    warnings.push(format!(
                        "Resource Profile '{}' binding {} resource '{}': resolver returned no carrier",
                        scheme,
                        spec.bus_id(this_host),
                        resource.name()
                    ));
                    continue;
                }
                Err(error) => {
                    warnings.push(format!(
                        "Resource Profile '{}' binding {} resource '{}': {error}",
                        scheme,
                        spec.bus_id(this_host),
                        resource.name()
                    ));
                    continue;
                }
            };
            let target = match SnapshotTarget::new(resolution.containment_root, &resolution.path) {
                Ok(target) => target,
                Err(error) => {
                    warnings.push(format!(
                        "Resource Profile '{}' binding {} resource '{}': {error}",
                        scheme,
                        spec.bus_id(this_host),
                        resource.name()
                    ));
                    continue;
                }
            };
            let recipient = spec.bus_id(this_host);
            let stable_key = format!("{recipient}\0{}", resource.name());
            desired.insert(
                stable_key.clone(),
                DesiredBinding {
                    stable_key,
                    catalog_root: catalog_root.to_path_buf(),
                    this_host: this_host.to_owned(),
                    recipient,
                    binding_name: resource.name().to_owned(),
                    scheme: scheme.to_owned(),
                    generation: *generation,
                    topology: descriptor.runtime.topology,
                    runtime: runtime.clone(),
                    component: Arc::clone(component),
                    uri: resource.uri().to_owned(),
                    selector,
                    descriptor: descriptor.clone(),
                    target,
                },
            );
        }
    }
    (desired, warnings)
}

struct Worker {
    catalog_root: PathBuf,
    this_host: String,
    request_dir: PathBuf,
    receipt_dir: PathBuf,
    initialized: bool,
    catalog_generation: Option<u64>,
    desired: BTreeMap<String, DesiredBinding>,
    runtimes: BTreeMap<RuntimeKey, RuntimeProcess>,
    #[cfg(feature = "wasip2-provider-runtime")]
    completion_tx: SyncSender<Msg>,
    #[cfg(feature = "wasip2-provider-runtime")]
    jobs: BTreeMap<u64, ObservationJob>,
}

impl Worker {
    fn new(
        catalog_root: PathBuf,
        this_host: String,
        request_dir: PathBuf,
        receipt_dir: PathBuf,
        #[cfg(feature = "wasip2-provider-runtime")] completion_tx: SyncSender<Msg>,
        #[cfg(not(feature = "wasip2-provider-runtime"))] _completion_tx: SyncSender<Msg>,
    ) -> Self {
        Self {
            catalog_root,
            this_host,
            request_dir,
            receipt_dir,
            initialized: false,
            catalog_generation: None,
            desired: BTreeMap::new(),
            runtimes: BTreeMap::new(),
            #[cfg(feature = "wasip2-provider-runtime")]
            completion_tx,
            #[cfg(feature = "wasip2-provider-runtime")]
            jobs: BTreeMap::new(),
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        while let Ok(message) = rx.recv() {
            let mut scan_requests_now = false;
            match message {
                Msg::Refresh {
                    desired,
                    catalog_generation,
                    reply,
                } => {
                    self.catalog_generation = catalog_generation;
                    self.desired = desired.clone();
                    let warnings = self.reconcile(desired);
                    self.initialized = true;
                    scan_requests_now = true;
                    let _ = reply.send(warnings);
                }
                Msg::Deactivate { recipient, reply } => {
                    self.deactivate_recipient(&recipient);
                    self.desired
                        .retain(|_, binding| binding.recipient != recipient);
                    scan_requests_now = true;
                    let _ = reply.send(());
                }
                Msg::Health { reply } => {
                    let health = self
                        .runtimes
                        .values()
                        .flat_map(RuntimeProcess::health)
                        .collect();
                    let _ = reply.send(health);
                }
                Msg::ObserveRequests => {
                    scan_requests_now = true;
                }
                #[cfg(feature = "wasip2-provider-runtime")]
                Msg::ObservationCompleted(completion) => {
                    self.complete_observation(completion);
                }
                Msg::Shutdown { reply } => {
                    self.shutdown_jobs(&rx);
                    self.stop_all();
                    let _ = reply.send(());
                    break;
                }
            }
            self.retry_observe_dispatches();
            if scan_requests_now {
                self.consume_observe_requests();
                self.retry_observe_dispatches();
            }
        }
        self.stop_all();
    }

    fn reconcile(&mut self, desired: BTreeMap<String, DesiredBinding>) -> Vec<String> {
        #[cfg(feature = "wasip2-provider-runtime")]
        self.cancel_replaced_jobs(&desired);
        let mut warnings = Vec::new();
        let desired_runtime_keys = desired
            .values()
            .map(DesiredBinding::runtime_key)
            .collect::<BTreeSet<_>>();
        let obsolete = self
            .runtimes
            .keys()
            .filter(|key| !desired_runtime_keys.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in obsolete {
            if let Some(mut runtime) = self.runtimes.remove(&key) {
                runtime.finalize_all_demand(
                    ObserveReceiptStatus::StaleGeneration,
                    Some("binding generation was replaced".to_owned()),
                );
                runtime.deactivate_all();
                runtime.stop();
            }
        }

        let mut grouped: BTreeMap<RuntimeKey, Vec<DesiredBinding>> = BTreeMap::new();
        for binding in desired.into_values() {
            grouped
                .entry(binding.runtime_key())
                .or_default()
                .push(binding);
        }
        for (key, bindings) in grouped {
            if !self.runtimes.contains_key(&key) {
                match RuntimeProcess::spawn(
                    key.clone(),
                    &bindings[0],
                    &self.catalog_root,
                    &self.this_host,
                ) {
                    Ok(runtime) => {
                        self.runtimes.insert(key.clone(), runtime);
                    }
                    Err(error) => {
                        warnings.push(format!(
                            "Resource Profile '{}': runtime spawn failed: {error:#}",
                            bindings[0].scheme
                        ));
                        continue;
                    }
                }
            }
            let Some(runtime) = self.runtimes.get_mut(&key) else {
                continue;
            };
            if let Err(error) = runtime.reconcile_bindings(bindings) {
                warnings.push(format!(
                    "Resource Profile '{}': runtime registration failed: {error:#}",
                    runtime.scheme
                ));
                if let Some(mut failed) = self.runtimes.remove(&key) {
                    failed.finalize_all_demand(
                        ObserveReceiptStatus::ProviderUnavailable,
                        Some("runtime registration failed".to_owned()),
                    );
                    failed.stop();
                }
            }
        }
        warnings
    }

    #[cfg(feature = "wasip2-provider-runtime")]
    fn cancel_replaced_jobs(&self, desired: &BTreeMap<String, DesiredBinding>) {
        for job in self.jobs.values() {
            let remains_current = desired.get(&job.stable_key).is_some_and(|next| {
                next.runtime_key() == job.runtime_key
                    && self
                        .runtimes
                        .get(&job.runtime_key)
                        .and_then(|runtime| runtime.bindings.get(&job.stable_key))
                        .is_some_and(|active| active.desired == *next)
            });
            if !remains_current {
                job.cancellation.cancel();
            }
        }
    }

    fn deactivate_recipient(&mut self, recipient: &str) {
        #[cfg(feature = "wasip2-provider-runtime")]
        for job in self.jobs.values() {
            let belongs_to_recipient = self
                .runtimes
                .get(&job.runtime_key)
                .and_then(|runtime| runtime.bindings.get(&job.stable_key))
                .is_some_and(|active| active.desired.recipient == recipient);
            if belongs_to_recipient {
                job.cancellation.cancel();
            }
        }
        for runtime in self.runtimes.values_mut() {
            runtime.deactivate_recipient(recipient);
        }
        let empty = self
            .runtimes
            .iter()
            .filter_map(|(key, runtime)| runtime.bindings.is_empty().then_some(key.clone()))
            .collect::<Vec<_>>();
        for key in empty {
            if let Some(mut runtime) = self.runtimes.remove(&key) {
                runtime.stop();
            }
        }
    }
    fn retry_observe_dispatches(&mut self) {
        #[cfg(feature = "wasip2-provider-runtime")]
        {
            let keys = self.runtimes.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                let (launches, errors, scheme) = {
                    let runtime = self
                        .runtimes
                        .get_mut(&key)
                        .expect("runtime selected from the same map");
                    let (launches, errors) = runtime.prepare_observe_dispatches(&key);
                    (launches, errors, runtime.scheme.clone())
                };
                for error in errors {
                    eprintln!("st: Resource Profile '{scheme}': {error}");
                }
                for launch in launches {
                    self.spawn_observation(launch);
                }
            }
        }
        #[cfg(not(feature = "wasip2-provider-runtime"))]
        for runtime in self.runtimes.values_mut() {
            for error in runtime.retry_observe_dispatches() {
                eprintln!("st: Resource Profile '{}': {error}", runtime.scheme);
            }
        }
    }

    #[cfg(feature = "wasip2-provider-runtime")]
    fn spawn_observation(&mut self, launch: ObservationLaunch) {
        let ObservationLaunch {
            job_id,
            runtime_key,
            stable_key,
            authority,
            watermark,
            fence,
            request,
            provider,
            cancellation,
        } = launch;
        let completion_tx = self.completion_tx.clone();
        let thread_runtime_key = runtime_key.clone();
        let thread_stable_key = stable_key.clone();
        let thread_authority = authority.clone();
        let thread_cancellation = cancellation.clone();
        let thread_fence = fence;
        let spawn = thread::Builder::new()
            .name(format!("st-resource-observe-{job_id}"))
            .spawn(move || {
                let result = catch_observation(|| provider.observe(&request, &thread_cancellation));
                let _ = completion_tx.send(Msg::ObservationCompleted(ObservationCompletion {
                    job_id,
                    runtime_key: thread_runtime_key,
                    stable_key: thread_stable_key,
                    authority: thread_authority,
                    watermark,
                    fence: thread_fence,
                    result,
                }));
            });
        match spawn {
            Ok(join) => {
                self.jobs.insert(
                    job_id,
                    ObservationJob {
                        runtime_key,
                        stable_key,
                        cancellation,
                        join,
                    },
                );
            }
            Err(error) => {
                cancellation.cancel();
                if let Some(runtime) = self.runtimes.get_mut(&runtime_key)
                    && let Some(active) = runtime.bindings.get_mut(&stable_key)
                {
                    active.health.state = RuntimeHealthState::Degraded;
                    active.health.detail =
                        Some(format!("observation worker could not start: {error}"));
                    let _ = settle_active_demand(
                        &runtime.request_dir,
                        &runtime.receipt_dir,
                        &runtime.owner,
                        active,
                        watermark,
                        ObserveReceiptStatus::SettledFailed,
                        None,
                        Some("provider observation worker could not start".to_owned()),
                    );
                    runtime.refresh_process_health();
                }
            }
        }
    }

    #[cfg(feature = "wasip2-provider-runtime")]
    fn complete_observation(&mut self, completion: ObservationCompletion) {
        let Some(job) = self.jobs.remove(&completion.job_id) else {
            return;
        };
        let _ = job.join.join();
        if job.runtime_key != completion.runtime_key || job.stable_key != completion.stable_key {
            return;
        }
        let Some(runtime) = self.runtimes.get_mut(&completion.runtime_key) else {
            return;
        };
        let Some(active) = runtime.bindings.get(&completion.stable_key) else {
            return;
        };
        let fence_is_current = active.binding_id == completion.authority.binding_id
            && active.registration == completion.authority.registration
            && active.desired.generation == completion.fence.generation()
            && active.revision == completion.fence.revision()
            && active.catch_up.state().current_snapshot_digest() == completion.fence.prior_digest()
            && active
                .demand
                .in_flight
                .as_ref()
                .is_some_and(|batch| batch.watermark == completion.watermark);
        if !fence_is_current {
            return;
        }
        match completion.result {
            Ok(result) => {
                let failure_detail = match &result {
                    st_resource_protocol::ObservationResult::Failed { diagnostic } => Some(
                        diagnostic
                            .clone()
                            .unwrap_or_else(|| "provider returned a failed observation".to_owned()),
                    ),
                    _ => None,
                };
                let message = RuntimeMessage::ObservationResult {
                    owner: completion.authority.owner,
                    binding_id: completion.authority.binding_id,
                    registration: completion.authority.registration,
                    demand_watermark: completion.watermark,
                    result,
                };
                let catalog_root = active.desired.catalog_root.clone();
                let this_host = active.desired.this_host.clone();
                if let Err(error) = runtime.accept(message, &catalog_root, &this_host) {
                    if let Some(active) = runtime.bindings.get_mut(&completion.stable_key) {
                        active.health.state = RuntimeHealthState::Degraded;
                        active.health.detail =
                            Some(format!("provider proposal rejected: {error:#}"));
                        let _ = settle_active_demand(
                            &runtime.request_dir,
                            &runtime.receipt_dir,
                            &runtime.owner,
                            active,
                            completion.watermark,
                            ObserveReceiptStatus::SettledFailed,
                            None,
                            Some("provider proposal was rejected".to_owned()),
                        );
                    }
                } else if let Some(active) = runtime.bindings.get_mut(&completion.stable_key) {
                    if let Some(detail) = failure_detail {
                        active.health.state = RuntimeHealthState::Degraded;
                        active.health.detail = Some(detail);
                    } else {
                        active.health.state = RuntimeHealthState::Ready;
                        active.health.detail = None;
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    scheme = %runtime.scheme,
                    error = %error,
                    "resource provider observation failed"
                );
                if let Some(active) = runtime.bindings.get_mut(&completion.stable_key) {
                    active.health.state = RuntimeHealthState::Degraded;
                    active.health.detail = Some(error);
                    let _ = settle_active_demand(
                        &runtime.request_dir,
                        &runtime.receipt_dir,
                        &runtime.owner,
                        active,
                        completion.watermark,
                        ObserveReceiptStatus::SettledFailed,
                        None,
                        Some("provider observation failed".to_owned()),
                    );
                }
            }
        }
        runtime.refresh_process_health();
    }

    #[cfg(feature = "wasip2-provider-runtime")]
    fn shutdown_jobs(&mut self, rx: &Receiver<Msg>) {
        for job in self.jobs.values() {
            job.cancellation.cancel();
        }
        for runtime in self.runtimes.values_mut() {
            runtime.finalize_all_demand(
                ObserveReceiptStatus::ProviderUnavailable,
                Some("Resource Profile supervisor shut down".to_owned()),
            );
        }
        while !self.jobs.is_empty() {
            match rx.recv() {
                Ok(Msg::ObservationCompleted(completion)) => {
                    self.complete_observation(completion);
                }
                Ok(Msg::Refresh { reply, .. }) => {
                    let _ = reply.send(vec![
                        "Resource Profile supervisor is shutting down".to_owned(),
                    ]);
                }
                Ok(Msg::Deactivate { reply, .. }) | Ok(Msg::Shutdown { reply }) => {
                    let _ = reply.send(());
                }
                Ok(Msg::Health { reply }) => {
                    let _ = reply.send(Vec::new());
                }
                Ok(Msg::ObserveRequests) => {}
                Err(_) => break,
            }
        }
        for (_, job) in std::mem::take(&mut self.jobs) {
            let _ = job.join.join();
        }
    }

    #[cfg(not(feature = "wasip2-provider-runtime"))]
    fn shutdown_jobs(&mut self, _rx: &Receiver<Msg>) {}

    fn consume_observe_requests(&mut self) {
        if !self.initialized {
            return;
        }
        let _span = tracing::info_span!(
            "resource.observe.consume",
            catalog = %self.catalog_root.display(),
            host = %self.this_host
        )
        .entered();
        let (records, errors) = scan_requests(&self.request_dir);
        for error in errors {
            eprintln!("st: {error}");
        }
        for record in records {
            match read_receipt(&self.receipt_dir, &record.request.request_id) {
                Ok(Some(receipt)) if receipt.status.is_terminal() => {
                    let _ = remove_request(&record.path);
                    continue;
                }
                Ok(_) => {}
                Err(error) => {
                    eprintln!(
                        "st: reading observe receipt for {:?}: {error:#}",
                        record.request.request_id
                    );
                    continue;
                }
            }
            if self
                .runtimes
                .values()
                .any(|runtime| runtime.contains_request(&record.request.request_id))
            {
                continue;
            }
            if let Some(expected) = record.request.expected_catalog_generation {
                match self.catalog_generation {
                    Some(current) if expected < current => {
                        self.finish_request_without_dispatch(
                            &record,
                            ObserveReceiptStatus::StaleGeneration,
                            "catalog generation changed before dispatch",
                        );
                        continue;
                    }
                    Some(current) if expected > current => continue,
                    None => continue,
                    Some(_) => {}
                }
            }
            let stable_key = record.request.stable_key();
            let runtime_key = self.runtimes.iter().find_map(|(key, runtime)| {
                runtime
                    .bindings
                    .contains_key(&stable_key)
                    .then(|| key.clone())
            });
            let Some(runtime_key) = runtime_key else {
                let (status, detail) = match self.desired.get(&stable_key) {
                    Some(desired) if desired.runtime.demand => (
                        ObserveReceiptStatus::ProviderUnavailable,
                        "the declared provider runtime is not available",
                    ),
                    Some(_) => (
                        ObserveReceiptStatus::AbsentBinding,
                        "the profile runtime does not declare the demand capability",
                    ),
                    None => (
                        ObserveReceiptStatus::AbsentBinding,
                        "no active observable binding matches the target",
                    ),
                };
                self.finish_request_without_dispatch(&record, status, detail);
                continue;
            };
            let Some(active) = self
                .runtimes
                .get(&runtime_key)
                .and_then(|runtime| runtime.bindings.get(&stable_key))
            else {
                continue;
            };
            if !active.desired.runtime.demand {
                self.finish_request_without_dispatch(
                    &record,
                    ObserveReceiptStatus::AbsentBinding,
                    "the profile runtime does not declare the demand capability",
                );
                continue;
            }
            if record.request.expected_snapshot_digest.is_some()
                && record.request.expected_snapshot_digest
                    != active.catch_up.state().current_snapshot_digest()
            {
                self.finish_request_without_dispatch(
                    &record,
                    ObserveReceiptStatus::StaleGeneration,
                    "snapshot digest changed before dispatch",
                );
                continue;
            }
            let enqueue = self
                .runtimes
                .get_mut(&runtime_key)
                .context("resolved runtime disappeared before demand enqueue")
                .and_then(|runtime| {
                    runtime.enqueue_demand(
                        stable_key,
                        record.request.clone(),
                        record.path.clone(),
                        record.modified_at,
                    )
                });
            if let Err(error) = enqueue {
                self.finish_request_without_dispatch(
                    &record,
                    ObserveReceiptStatus::ProviderUnavailable,
                    &error.to_string(),
                );
                continue;
            }
        }
        for error in prune_terminal_receipts(&self.receipt_dir) {
            eprintln!("st: {error}");
        }
    }

    fn finish_request_without_dispatch(
        &self,
        record: &PendingRequestRecord,
        status: ObserveReceiptStatus,
        diagnostic: &str,
    ) {
        let receipt = ObserveReceipt::new(
            &record.request,
            status,
            None,
            None,
            None,
            Some(diagnostic.to_owned()),
        );
        match receipt.and_then(|receipt| write_receipt(&self.receipt_dir, &receipt)) {
            Ok(()) => {
                crate::metrics::record_resource_observe_request(status.wire_str());
                if let Err(error) = remove_request(&record.path) {
                    eprintln!(
                        "st: consuming observe request {}: {error:#}",
                        record.path.display()
                    );
                }
            }
            Err(error) => eprintln!(
                "st: writing observe receipt for {:?}: {error:#}",
                record.request.request_id
            ),
        }
    }

    fn stop_all(&mut self) {
        for (_, mut runtime) in std::mem::take(&mut self.runtimes) {
            runtime.stop();
        }
    }
}

struct RuntimeProcess {
    scheme: String,
    owner: RuntimeOwner,
    lifecycle: RuntimeLifecycle,
    #[cfg(feature = "wasip2-provider-runtime")]
    provider: Arc<ProviderRuntime>,
    bindings: BTreeMap<String, ActiveBinding>,
    request_dir: PathBuf,
    receipt_dir: PathBuf,
    process_health: ResourceProfileHealth,
}

#[cfg(feature = "wasip2-provider-runtime")]
enum ProviderRuntime {
    GitHubIssue {
        executor: Wasip2Executor<GitHubIssueModule>,
        component: LoadedComponent,
    },
    GitHubPr {
        executor: Wasip2Executor<GitHubPrModule>,
        component: LoadedComponent,
    },
    PtyStats {
        executor: Wasip2Executor<PtyStatsModule>,
        component: LoadedComponent,
    },
    Vista {
        executor: Wasip2Executor<VistaModule>,
        component: LoadedComponent,
    },
}
#[cfg(feature = "wasip2-provider-runtime")]
fn validate_provider_descriptor(
    provider: &Wasip2ProviderDescriptor,
    profile: &ProfileDescriptor,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        provider.capabilities == [Wasip2SchedulingCapability::Demand],
        "provider descriptor must declare exactly the demand scheduling capability"
    );
    anyhow::ensure!(
        provider.snapshot_media_type == profile.snapshot.media_type,
        "provider snapshot media type '{}' does not match profile contract '{}'",
        provider.snapshot_media_type,
        profile.snapshot.media_type
    );
    anyhow::ensure!(
        provider.snapshot_schema_id == profile.snapshot.schema_id,
        "provider snapshot schema '{}' does not match profile contract '{}'",
        provider.snapshot_schema_id,
        profile.snapshot.schema_id
    );
    let profile_topics = profile
        .topics
        .iter()
        .map(|topic| topic.name.as_str())
        .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        provider
            .topics
            .iter()
            .all(|topic| profile_topics.contains(topic.as_str())),
        "provider descriptor declares a topic outside the profile contract"
    );
    Ok(())
}

#[cfg(feature = "wasip2-provider-runtime")]
impl ProviderRuntime {
    fn cancellation(&self) -> ObservationCancellation {
        let interruption = match self {
            Self::GitHubIssue { executor, .. } => executor.interruption_handle(),
            Self::GitHubPr { executor, .. } => executor.interruption_handle(),
            Self::PtyStats { executor, .. } => executor.interruption_handle(),
            Self::Vista { executor, .. } => executor.interruption_handle(),
        };
        ObservationCancellation { interruption }
    }

    fn observe(
        &self,
        request: &Wasip2ObservationRequest,
        cancellation: &ObservationCancellation,
    ) -> Result<st_resource_protocol::ObservationResult, st2_resource_wasip2::ObserveError> {
        match self {
            Self::GitHubIssue {
                executor,
                component,
                ..
            } => executor.observe(component, request, Some(&cancellation.interruption)),
            Self::GitHubPr {
                executor,
                component,
                ..
            } => executor.observe(component, request, Some(&cancellation.interruption)),
            Self::PtyStats {
                executor,
                component,
                ..
            } => executor.observe(component, request, Some(&cancellation.interruption)),
            Self::Vista {
                executor,
                component,
                ..
            } => executor.observe(component, request, Some(&cancellation.interruption)),
        }
    }
}

struct ActiveBinding {
    desired: DesiredBinding,
    binding_id: BindingId,
    registration: RegistrationToken,
    catch_up: CatchUp,
    health: ResourceProfileHealth,
    demand: DemandState,
    #[cfg_attr(not(feature = "wasip2-provider-runtime"), allow(dead_code))]
    revision: u64,
}

#[derive(Debug, Default)]
struct DemandState {
    next_watermark: u64,
    in_flight: Option<DemandBatch>,
    trailing: Option<DemandBatch>,
    settled: Vec<DemandSettlement>,
}

#[derive(Debug)]
struct DemandBatch {
    watermark: u64,
    requests: Vec<PendingDemand>,
    dispatched_at: Option<Instant>,
}

#[derive(Debug)]
struct DemandSettlement {
    batch: DemandBatch,
    status: ObserveReceiptStatus,
    digest: Option<SnapshotDigest>,
    diagnostic: Option<String>,
}

#[derive(Debug)]
struct PendingDemand {
    request: ObserveRequest,
    request_path: PathBuf,
    #[cfg_attr(not(feature = "wasip2-provider-runtime"), allow(dead_code))]
    queued_at: SystemTime,
    last_status: Option<ObserveReceiptStatus>,
}

impl DemandState {
    fn push(
        &mut self,
        request: ObserveRequest,
        request_path: PathBuf,
        queued_at: SystemTime,
    ) -> anyhow::Result<()> {
        if self.trailing.is_none() {
            let watermark = self
                .next_watermark
                .checked_add(1)
                .context("demand watermark exhausted")?;
            self.next_watermark = watermark;
            self.trailing = Some(DemandBatch {
                watermark,
                requests: Vec::new(),
                dispatched_at: None,
            });
        }
        self.trailing
            .as_mut()
            .context("trailing demand batch is absent after allocation")?
            .requests
            .push(PendingDemand {
                request,
                request_path,
                queued_at,
                last_status: None,
            });
        Ok(())
    }

    fn contains_request(&self, request_id: &str) -> bool {
        self.in_flight
            .iter()
            .chain(self.trailing.iter())
            .chain(self.settled.iter().map(|settlement| &settlement.batch))
            .flat_map(|batch| &batch.requests)
            .any(|pending| pending.request.request_id == request_id)
    }
}

impl RuntimeProcess {
    fn spawn(
        _key: RuntimeKey,
        sample: &DesiredBinding,
        catalog_root: &Path,
        this_host: &str,
    ) -> anyhow::Result<Self> {
        #[cfg(not(feature = "wasip2-provider-runtime"))]
        {
            let _ = (sample, catalog_root, this_host);
            anyhow::bail!(
                "component provider runtime unavailable because st2 was built without \
                 `wasip2-provider-runtime`"
            );
        }
        #[cfg(feature = "wasip2-provider-runtime")]
        {
            let provider = match &sample.runtime.capability {
                crate::catalog::DeclaredProviderCapability::GitHubIssue {
                    auth_executable,
                    connect_timeout_ms,
                    total_timeout_ms,
                } => {
                    let auth_executable =
                        crate::expand::expand_catalog(auth_executable, catalog_root);
                    let module = GitHubIssueModule::new(GitHubIssueConfig {
                        auth_executable: PathBuf::from(auth_executable),
                        connect_timeout: Duration::from_millis(*connect_timeout_ms),
                        total_timeout: Duration::from_millis(*total_timeout_ms),
                    })
                    .map_err(anyhow::Error::msg)?;
                    let executor =
                        Wasip2Executor::new(Wasip2RuntimeConfig::default(), None, module)?;
                    let component = executor.load(&sample.component.bytes)?;
                    let descriptor = executor.describe(&component, None)?;
                    validate_provider_descriptor(&descriptor, &sample.descriptor)?;
                    ProviderRuntime::GitHubIssue {
                        executor,
                        component,
                    }
                }
                crate::catalog::DeclaredProviderCapability::GitHubPr {
                    auth_executable,
                    connect_timeout_ms,
                    total_timeout_ms,
                } => {
                    let auth_executable =
                        crate::expand::expand_catalog(auth_executable, catalog_root);
                    let module = GitHubPrModule::new(GitHubPrConfig {
                        auth_executable: PathBuf::from(auth_executable),
                        connect_timeout: Duration::from_millis(*connect_timeout_ms),
                        total_timeout: Duration::from_millis(*total_timeout_ms),
                    })
                    .map_err(anyhow::Error::msg)?;
                    let executor =
                        Wasip2Executor::new(Wasip2RuntimeConfig::default(), None, module)?;
                    let component = executor.load(&sample.component.bytes)?;
                    let descriptor = executor.describe(&component, None)?;
                    validate_provider_descriptor(&descriptor, &sample.descriptor)?;
                    ProviderRuntime::GitHubPr {
                        executor,
                        component,
                    }
                }
                crate::catalog::DeclaredProviderCapability::PtyStats {
                    executable,
                    cwd,
                    deadline_ms,
                } => {
                    let executable = crate::expand::expand_catalog(executable, catalog_root);
                    let cwd = crate::expand::expand_catalog(cwd, catalog_root);
                    let config = PtyStatsConfig::resolve(
                        executable,
                        PathBuf::from(cwd),
                        Duration::from_millis(*deadline_ms),
                    )
                    .map_err(anyhow::Error::msg)?;
                    let module = PtyStatsModule::new(config);
                    let executor =
                        Wasip2Executor::new(Wasip2RuntimeConfig::default(), None, module)?;
                    let component = executor.load(&sample.component.bytes)?;
                    let descriptor = executor.describe(&component, None)?;
                    validate_provider_descriptor(&descriptor, &sample.descriptor)?;
                    ProviderRuntime::PtyStats {
                        executor,
                        component,
                    }
                }
                crate::catalog::DeclaredProviderCapability::Vista {
                    executable,
                    cwd,
                    deadline_ms,
                } => {
                    let executable = crate::expand::expand_catalog(executable, catalog_root);
                    let cwd = crate::expand::expand_catalog(cwd, catalog_root);
                    let config = VistaConfig::resolve(
                        executable,
                        PathBuf::from(cwd),
                        Duration::from_millis(*deadline_ms),
                    )
                    .map_err(anyhow::Error::msg)?;
                    let module = VistaModule::new(config);
                    let executor =
                        Wasip2Executor::new(Wasip2RuntimeConfig::default(), None, module)?;
                    let component = executor.load(&sample.component.bytes)?;
                    let descriptor = executor.describe(&component, None)?;
                    validate_provider_descriptor(&descriptor, &sample.descriptor)?;
                    ProviderRuntime::Vista {
                        executor,
                        component,
                    }
                }
            };
            let sequence = ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let incarnation = RuntimeIncarnation::new(format!("{}-{sequence}", sample.generation))?;
            let claim = OwnerClaim::new(hash_text(&format!(
                "{}\0{}\0{}\0{sequence}",
                catalog_root.display(),
                this_host,
                sample.scheme
            )))?;
            let owner = RuntimeOwner::new(incarnation, claim);
            let mut lifecycle = RuntimeLifecycle::new();
            lifecycle.claim(owner.clone());
            let scope = crate::park::SupervisorScope::current(catalog_root, this_host)?;
            Ok(Self {
                scheme: sample.scheme.clone(),
                owner,
                lifecycle,
                provider: Arc::new(provider),
                bindings: BTreeMap::new(),
                request_dir: scope.observe_request_dir(),
                receipt_dir: scope.observe_receipt_dir(),
                process_health: ResourceProfileHealth {
                    scheme: sample.scheme.clone(),
                    binding: None,
                    state: RuntimeHealthState::Ready,
                    detail: None,
                },
            })
        }
    }

    fn reconcile_bindings(&mut self, desired: Vec<DesiredBinding>) -> anyhow::Result<()> {
        let desired_keys = desired
            .iter()
            .map(|binding| binding.stable_key.clone())
            .collect::<BTreeSet<_>>();
        let removed = self
            .bindings
            .keys()
            .filter(|key| !desired_keys.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in removed {
            self.unregister(&key);
        }
        for binding in desired {
            if self
                .bindings
                .get(&binding.stable_key)
                .is_some_and(|active| active.desired == binding)
            {
                if let Some(active) = self.bindings.get_mut(&binding.stable_key) {
                    let _ = emit_pending_for(active);
                }
                continue;
            }
            self.unregister(&binding.stable_key);
            self.register(binding)?;
        }
        Ok(())
    }

    fn register(&mut self, desired: DesiredBinding) -> anyhow::Result<()> {
        let sequence = ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let binding_id = BindingId::new(hash_text(&format!(
            "{}\0{}\0{}",
            desired.stable_key, desired.generation, sequence
        )))?;
        let registration = RegistrationToken::new(hash_text(&format!(
            "{}\0{}\0{sequence}",
            self.owner.claim().as_str(),
            desired.stable_key
        )))?;
        let selection = selector_topics(&desired.selector)?;
        let contract = PublicationContract::new(
            desired.descriptor.snapshot.schema_id.clone(),
            desired.descriptor.snapshot.media_type.clone(),
            desired
                .descriptor
                .topics
                .iter()
                .map(|topic| topic.name.clone()),
            selection,
        )?;
        let registration_state = BindingRegistration::new(
            binding_id.clone(),
            registration.clone(),
            desired.target.clone(),
            contract,
        );
        self.lifecycle.register(&self.owner, registration_state)?;

        let state_directory = binding_state_directory(&desired)?;
        std::fs::create_dir_all(&state_directory)
            .with_context(|| format!("create {}", state_directory.display()))?;
        let mut catch_up = CatchUp::open_for_snapshot(&state_directory, &desired.target)?;
        let pending = catch_up.set_deliverable(true)?;
        let mut active = ActiveBinding {
            health: ResourceProfileHealth {
                scheme: desired.scheme.clone(),
                binding: Some(desired.binding_name.clone()),
                state: RuntimeHealthState::Ready,
                detail: None,
            },
            desired,
            binding_id,
            registration,
            catch_up,
            demand: DemandState::default(),
            revision: 0,
        };
        if pending.is_some() {
            let _ = emit_pending_for(&mut active);
        }
        self.bindings
            .insert(active.desired.stable_key.clone(), active);
        self.refresh_process_health();
        Ok(())
    }

    fn unregister(&mut self, stable_key: &str) {
        let Some(mut active) = self.bindings.remove(stable_key) else {
            return;
        };
        finalize_active_demand(
            &self.request_dir,
            &self.receipt_dir,
            &self.owner,
            &mut active,
            ObserveReceiptStatus::StaleGeneration,
            Some("binding registration was removed or replaced".to_owned()),
        );
        let _ = active.catch_up.set_deliverable(false);
        let _ = self
            .lifecycle
            .unregister(&self.owner, &active.binding_id, &active.registration);
        self.refresh_process_health();
    }

    fn deactivate_recipient(&mut self, recipient: &str) {
        let removed = self
            .bindings
            .iter()
            .filter_map(|(key, active)| {
                (active.desired.recipient == recipient).then_some(key.clone())
            })
            .collect::<Vec<_>>();
        for key in removed {
            self.unregister(&key);
        }
    }

    fn deactivate_all(&mut self) {
        let keys = self.bindings.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            self.unregister(&key);
        }
    }

    fn enqueue_demand(
        &mut self,
        stable_key: String,
        request: ObserveRequest,
        request_path: PathBuf,
        queued_at: SystemTime,
    ) -> anyhow::Result<()> {
        self.bindings
            .get_mut(&stable_key)
            .context("active binding disappeared before demand enqueue")?
            .demand
            .push(request, request_path, queued_at)
    }

    fn contains_request(&self, request_id: &str) -> bool {
        self.bindings
            .values()
            .any(|active| active.demand.contains_request(request_id))
    }

    #[cfg(feature = "wasip2-provider-runtime")]
    fn prepare_observe_dispatches(
        &mut self,
        runtime_key: &RuntimeKey,
    ) -> (Vec<ObservationLaunch>, Vec<String>) {
        let mut launches = Vec::new();
        let mut errors = Vec::new();
        for active in self.bindings.values_mut() {
            let authority = ObservationAuthority {
                owner: self.owner.clone(),
                binding_id: active.binding_id.clone(),
                registration: active.registration.clone(),
            };
            errors.extend(retry_settled_demand(
                &self.request_dir,
                &self.receipt_dir,
                &authority,
                active,
            ));
        }
        let keys = self
            .bindings
            .iter()
            .filter(|(_, active)| {
                active.demand.in_flight.is_none() && active.demand.trailing.is_some()
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for stable_key in keys {
            let active = self
                .bindings
                .get_mut(&stable_key)
                .expect("binding selected from the same map");
            let authority = ObservationAuthority {
                owner: self.owner.clone(),
                binding_id: active.binding_id.clone(),
                registration: active.registration.clone(),
            };
            let mut batch = active
                .demand
                .trailing
                .take()
                .expect("binding was selected with trailing demand");
            batch.dispatched_at = Some(Instant::now());
            errors.extend(write_batch_status(
                &self.request_dir,
                &self.receipt_dir,
                &authority,
                &mut batch,
                ObserveReceiptStatus::Accepted,
                None,
                None,
            ));
            for pending in &batch.requests {
                crate::metrics::record_resource_observe_dispatch(
                    SystemTime::now()
                        .duration_since(pending.queued_at)
                        .unwrap_or(Duration::ZERO),
                );
            }
            let watermark = batch.watermark;
            active.demand.in_flight = Some(batch);
            let fence = ProposalFence::new(
                active.desired.generation,
                active.revision,
                active.catch_up.state().current_snapshot_digest(),
            );
            let job_id = ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let cancellation = self.provider.cancellation();
            launches.push(ObservationLaunch {
                job_id,
                runtime_key: runtime_key.clone(),
                stable_key,
                authority,
                watermark,
                fence,
                request: Wasip2ObservationRequest {
                    invocation_id: job_id,
                    uri: active.desired.uri.clone(),
                    selector: active.desired.selector.clone(),
                    prior_digest: fence.prior_digest(),
                    demand_watermark: Some(watermark),
                },
                provider: Arc::clone(&self.provider),
                cancellation,
            });
        }
        (launches, errors)
    }

    #[cfg(not(feature = "wasip2-provider-runtime"))]
    fn retry_observe_dispatches(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        for active in self.bindings.values_mut() {
            let authority = ObservationAuthority {
                owner: self.owner.clone(),
                binding_id: active.binding_id.clone(),
                registration: active.registration.clone(),
            };
            errors.extend(retry_settled_demand(
                &self.request_dir,
                &self.receipt_dir,
                &authority,
                active,
            ));
        }
        let keys = self
            .bindings
            .iter()
            .filter(|(_, active)| {
                active.demand.in_flight.is_none() && active.demand.trailing.is_some()
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            let active = self
                .bindings
                .get_mut(&key)
                .expect("binding selected from the same map");
            let authority = ObservationAuthority {
                owner: self.owner.clone(),
                binding_id: active.binding_id.clone(),
                registration: active.registration.clone(),
            };
            let mut batch = active
                .demand
                .trailing
                .take()
                .expect("binding was selected with trailing demand");
            batch.dispatched_at = Some(Instant::now());
            errors.extend(write_batch_status(
                &self.request_dir,
                &self.receipt_dir,
                &authority,
                &mut batch,
                ObserveReceiptStatus::Accepted,
                None,
                None,
            ));
            let watermark = batch.watermark;
            active.demand.in_flight = Some(batch);
            if let Err(error) = settle_active_demand(
                &self.request_dir,
                &self.receipt_dir,
                &self.owner,
                active,
                watermark,
                ObserveReceiptStatus::ProviderUnavailable,
                None,
                Some("component provider runtime is unavailable".to_owned()),
            ) {
                errors.push(error.to_string());
            }
        }
        errors
    }

    fn finalize_all_demand(&mut self, status: ObserveReceiptStatus, diagnostic: Option<String>) {
        for active in self.bindings.values_mut() {
            finalize_active_demand(
                &self.request_dir,
                &self.receipt_dir,
                &self.owner,
                active,
                status,
                diagnostic.clone(),
            );
        }
    }

    #[cfg_attr(not(feature = "wasip2-provider-runtime"), allow(dead_code))]
    fn accept(
        &mut self,
        message: RuntimeMessage,
        catalog_root: &Path,
        this_host: &str,
    ) -> anyhow::Result<()> {
        match self.lifecycle.accept_output(&message)? {
            AcceptedOutput::Publication(publication) => {
                let (_, delivery_error) =
                    process_publication(&mut self.bindings, publication, catalog_root, this_host)?;
                if let Some(error) = delivery_error {
                    return Err(error);
                }
            }
            AcceptedOutput::Health(health) => {
                if let Some(binding_id) = health.binding_id() {
                    if let Some(active) = self
                        .bindings
                        .values_mut()
                        .find(|active| &active.binding_id == binding_id)
                    {
                        active.health.state = health.state();
                        active.health.detail = health.detail().map(str::to_owned);
                    }
                } else {
                    self.process_health.state = health.state();
                    self.process_health.detail = health.detail().map(str::to_owned);
                }
            }
            AcceptedOutput::ObservationResult(observation) => {
                let (binding_id, watermark, result) = observation.into_parts();
                {
                    let active = self
                        .bindings
                        .values()
                        .find(|active| &active.binding_id == binding_id)
                        .context("accepted observation result has no active binding")?;
                    validate_active_demand(active, watermark)?;
                }
                let (status, digest, diagnostic, delivery_error) = match result {
                    AcceptedObservation::Unchanged => {
                        (ObserveReceiptStatus::SettledUnchanged, None, None, None)
                    }
                    AcceptedObservation::Failed { diagnostic } => (
                        ObserveReceiptStatus::SettledFailed,
                        None,
                        diagnostic.map(str::to_owned),
                        None,
                    ),
                    AcceptedObservation::Published(publication) => {
                        let (outcome, delivery_error) = process_publication(
                            &mut self.bindings,
                            publication,
                            catalog_root,
                            this_host,
                        )?;
                        (
                            ObserveReceiptStatus::SettledChanged,
                            Some(outcome.digest()),
                            None,
                            delivery_error,
                        )
                    }
                };
                let active = self
                    .bindings
                    .values_mut()
                    .find(|active| &active.binding_id == binding_id)
                    .context("accepted observation result has no active binding")?;
                settle_active_demand(
                    &self.request_dir,
                    &self.receipt_dir,
                    &self.owner,
                    active,
                    watermark,
                    status,
                    digest,
                    diagnostic,
                )?;
                if let Some(error) = delivery_error {
                    eprintln!(
                        "st: Resource Profile '{}': demanded publication delivery failed: {error:#}",
                        self.scheme
                    );
                }
            }
        }
        self.refresh_process_health();
        Ok(())
    }

    fn refresh_process_health(&mut self) {
        let degraded = self
            .bindings
            .values()
            .filter(|active| active.health.state == RuntimeHealthState::Degraded)
            .map(|active| active.desired.binding_name.as_str())
            .collect::<Vec<_>>();
        if degraded.is_empty() {
            self.process_health.state = RuntimeHealthState::Ready;
            self.process_health.detail = None;
        } else {
            self.process_health.state = RuntimeHealthState::Degraded;
            self.process_health.detail =
                Some(format!("degraded bindings: {}", degraded.join(", ")));
        }
    }

    fn health(&self) -> Vec<ResourceProfileHealth> {
        std::iter::once(self.process_health.clone())
            .chain(self.bindings.values().map(|active| active.health.clone()))
            .collect()
    }

    fn stop(&mut self) {}
}

#[cfg_attr(not(feature = "wasip2-provider-runtime"), allow(dead_code))]
fn process_publication(
    bindings: &mut BTreeMap<String, ActiveBinding>,
    publication: AcceptedPublication<'_>,
    catalog_root: &Path,
    this_host: &str,
) -> anyhow::Result<(PublicationOutcome, Option<anyhow::Error>)> {
    let binding_id = publication.binding_id().clone();
    let active = bindings
        .values_mut()
        .find(|active| active.binding_id == binding_id)
        .context("accepted publication has no active binding")?;
    let next_revision = active
        .revision
        .checked_add(1)
        .context("provider publication revision exhausted")?;
    let (outcome, pending) = active.catch_up.publish(publication)?;
    active.revision = next_revision;
    let delivery_error = if pending.is_some() {
        emit_pending(catalog_root, this_host, active).err()
    } else {
        None
    };
    Ok((outcome, delivery_error))
}

fn validate_active_demand(active: &ActiveBinding, watermark: u64) -> anyhow::Result<()> {
    let expected_watermark = active
        .demand
        .in_flight
        .as_ref()
        .context("runtime returned an observation result without an outstanding demand")?
        .watermark;
    anyhow::ensure!(
        expected_watermark == watermark,
        "runtime returned demand watermark {watermark}, expected {expected_watermark}"
    );
    Ok(())
}

fn write_batch_status(
    request_dir: &Path,
    receipt_dir: &Path,
    authority: &ObservationAuthority,
    batch: &mut DemandBatch,
    status: ObserveReceiptStatus,
    digest: Option<SnapshotDigest>,
    diagnostic: Option<String>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for pending in &mut batch.requests {
        if pending.last_status == Some(status) {
            continue;
        }
        let receipt = ObserveReceipt::new(
            &pending.request,
            status,
            Some(authority.clone()),
            Some(batch.watermark),
            digest,
            diagnostic.clone(),
        );
        match receipt.and_then(|receipt| write_receipt(receipt_dir, &receipt)) {
            Ok(()) => {
                pending.last_status = Some(status);
                crate::metrics::record_resource_observe_request(status.wire_str());
                if status.is_terminal()
                    && let Err(error) = remove_request(&pending.request_path)
                {
                    errors.push(format!(
                        "removing terminal observe request {} from {}: {error:#}",
                        pending.request.request_id,
                        request_dir.display()
                    ));
                }
            }
            Err(error) => errors.push(format!(
                "writing observe receipt for {:?}: {error:#}",
                pending.request.request_id
            )),
        }
    }
    errors
}

fn retry_settled_demand(
    request_dir: &Path,
    receipt_dir: &Path,
    authority: &ObservationAuthority,
    active: &mut ActiveBinding,
) -> Vec<String> {
    let mut errors = Vec::new();
    for settlement in &mut active.demand.settled {
        errors.extend(write_batch_status(
            request_dir,
            receipt_dir,
            authority,
            &mut settlement.batch,
            settlement.status,
            settlement.digest,
            settlement.diagnostic.clone(),
        ));
    }
    active.demand.settled.retain(|settlement| {
        settlement
            .batch
            .requests
            .iter()
            .any(|pending| pending.last_status != Some(settlement.status))
    });
    errors
}

#[allow(clippy::too_many_arguments)]
fn settle_active_demand(
    request_dir: &Path,
    receipt_dir: &Path,
    owner: &RuntimeOwner,
    active: &mut ActiveBinding,
    watermark: u64,
    status: ObserveReceiptStatus,
    digest: Option<SnapshotDigest>,
    diagnostic: Option<String>,
) -> anyhow::Result<()> {
    validate_active_demand(active, watermark)?;
    let batch = active
        .demand
        .in_flight
        .take()
        .context("validated outstanding demand disappeared before settlement")?;
    if let Some(dispatched_at) = batch.dispatched_at {
        for _ in &batch.requests {
            crate::metrics::record_resource_observe_settle(dispatched_at.elapsed());
        }
    }
    let authority = ObservationAuthority {
        owner: owner.clone(),
        binding_id: active.binding_id.clone(),
        registration: active.registration.clone(),
    };
    active.demand.settled.push(DemandSettlement {
        batch,
        status,
        digest,
        diagnostic,
    });
    for error in retry_settled_demand(request_dir, receipt_dir, &authority, active) {
        eprintln!("st: Resource Profile '{}': {error}", active.desired.scheme);
    }
    tracing::info!(
        recipient = %active.desired.recipient,
        binding = %active.desired.binding_name,
        demand_watermark = watermark,
        result = status.as_str(),
        "Resource observation settled"
    );
    Ok(())
}

fn finalize_active_demand(
    request_dir: &Path,
    receipt_dir: &Path,
    owner: &RuntimeOwner,
    active: &mut ActiveBinding,
    status: ObserveReceiptStatus,
    diagnostic: Option<String>,
) {
    let authority = ObservationAuthority {
        owner: owner.clone(),
        binding_id: active.binding_id.clone(),
        registration: active.registration.clone(),
    };
    for error in retry_settled_demand(request_dir, receipt_dir, &authority, active) {
        eprintln!("st: Resource Profile '{}': {error}", active.desired.scheme);
    }
    for mut batch in active
        .demand
        .in_flight
        .take()
        .into_iter()
        .chain(active.demand.trailing.take())
    {
        for error in write_batch_status(
            request_dir,
            receipt_dir,
            &authority,
            &mut batch,
            status,
            None,
            diagnostic.clone(),
        ) {
            eprintln!("st: Resource Profile '{}': {error}", active.desired.scheme);
        }
    }
}

fn selector_topics(selector: &Value) -> anyhow::Result<TopicSelection> {
    let topics = selector
        .as_object()
        .and_then(|object| object.get("topics"))
        .and_then(Value::as_array)
        .map(|topics| {
            topics
                .iter()
                .map(|topic| {
                    topic
                        .as_str()
                        .map(str::to_owned)
                        .context("selector topic is not a string")
                })
                .collect::<anyhow::Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(TopicSelection::new(topics)?)
}

fn profile_generation(
    catalog_generation: u64,
    declared: &crate::catalog::DeclaredProfile,
    descriptor: &ProfileDescriptor,
    profiles: &ResourceProfileRegistry,
    component_identity: &str,
) -> u64 {
    let module_identity = profiles
        .get(&declared.scheme)
        .and_then(|profile| profile.module())
        .and_then(|module| std::fs::metadata(module).ok())
        .map(|metadata| {
            format!(
                "{}:{:?}:{:?}",
                metadata.len(),
                metadata.modified().ok(),
                metadata.created().ok()
            )
        })
        .unwrap_or_default();
    let input = format!(
        "{catalog_generation}\0{}\0{}\0{}\0{}\0{:?}\0{:?}\0{module_identity}\0{component_identity}",
        declared.scheme,
        declared.wasm,
        declared.class,
        declared.notify_chain,
        declared.runtime.as_ref(),
        descriptor,
    );
    let digest = Sha256::digest(input.as_bytes());
    u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("SHA-256 prefix is eight bytes"),
    )
}

#[cfg_attr(not(feature = "wasip2-provider-runtime"), allow(dead_code))]
fn emit_pending(
    catalog_root: &Path,
    this_host: &str,
    active: &mut ActiveBinding,
) -> anyhow::Result<()> {
    emit_pending_at(catalog_root, this_host, active)
}

fn emit_pending_for(active: &mut ActiveBinding) -> anyhow::Result<()> {
    let root = active.desired.catalog_root.clone();
    let host = active.desired.this_host.clone();
    emit_pending_at(&root, &host, active)
}

fn emit_pending_at(
    catalog_root: &Path,
    this_host: &str,
    active: &mut ActiveBinding,
) -> anyhow::Result<()> {
    let Some(delivery) = active.catch_up.pending_delivery() else {
        return Ok(());
    };
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Body<'a> {
        binding: &'a str,
        snapshot_digest: String,
        topics: &'a [String],
        facts: &'a [ResourceFact],
    }
    let digest = delivery.digest();
    let topics = delivery.selected_topics().to_vec();
    let facts = delivery.facts();
    let body = serde_json::to_string(&Body {
        binding: &active.desired.binding_name,
        snapshot_digest: digest.to_string(),
        topics: &topics,
        facts,
    })?;
    let event_id = publication_event_id(
        &active.desired.recipient,
        &active.desired.binding_name,
        digest,
    );
    let subject = resource_change_subject(
        &active.desired.binding_name,
        facts,
        &topics,
        "snapshot updated",
    );
    crate::event::emit_builtin_resync(
        catalog_root,
        this_host,
        &active.desired.recipient,
        &event_id,
        Some(&active.desired.binding_name),
        Some(&subject),
        &body,
        true,
    )?;
    active.catch_up.acknowledge_delivery(digest)?;
    Ok(())
}

fn publication_event_id(recipient: &str, binding: &str, digest: SnapshotDigest) -> String {
    hash_text(&format!(
        "resource-profile\0{recipient}\0{binding}\0{digest}"
    ))
}

pub(crate) fn resource_change_subject(
    binding: &str,
    facts: &[ResourceFact],
    topics: &[String],
    fallback: &str,
) -> String {
    const SUBJECT_MAX_SCALARS: usize = 96;
    const MAX_RENDERED_FACTS: usize = 3;

    let topic_suffix = if topics.is_empty() {
        String::new()
    } else {
        format!(" [{}]", topics.join(", "))
    };
    let base = format!("{binding} · ");
    let mut rendered = Vec::new();
    for fact in facts.iter().take(MAX_RENDERED_FACTS) {
        let fact = render_fact(fact);
        let candidate = format!("{base}{}{topic_suffix}", {
            let mut candidate_facts = rendered.clone();
            candidate_facts.push(fact.clone());
            candidate_facts.join("; ")
        });
        if candidate.chars().count() > SUBJECT_MAX_SCALARS {
            break;
        }
        rendered.push(fact);
    }

    let detail = if rendered.is_empty() {
        fallback.to_owned()
    } else {
        rendered.join("; ")
    };
    let subject = format!("{base}{detail}{topic_suffix}");
    if subject.chars().count() <= SUBJECT_MAX_SCALARS {
        return subject;
    }

    let fallback = format!("{binding} · {fallback}");
    if fallback.chars().count() <= SUBJECT_MAX_SCALARS {
        fallback
    } else {
        fallback.chars().take(SUBJECT_MAX_SCALARS).collect()
    }
}

fn render_fact(fact: &ResourceFact) -> String {
    match (fact.before(), fact.after()) {
        (None, Some(Some(after))) => format!("{}={after}", fact.key()),
        (None, Some(None)) => format!("{}=removed", fact.key()),
        (Some(None), Some(Some(after))) => format!("{}=+{after}", fact.key()),
        (Some(Some(before)), Some(None)) => format!("{}=-{before}", fact.key()),
        (Some(Some(before)), Some(Some(after))) => {
            format!("{}={before}→{after}", fact.key())
        }
        (Some(None), Some(None)) => format!("{}=absent", fact.key()),
        (Some(Some(before)), None) => format!("{} was {before}", fact.key()),
        (Some(None), None) => format!("{} was absent", fact.key()),
        (None, None) => unreachable!("validated facts always carry a value"),
    }
}

fn binding_state_directory(desired: &DesiredBinding) -> anyhow::Result<PathBuf> {
    let state = lexical_absolute(&crate::run::state_root())?;
    Ok(state
        .join("st2")
        .join("resource-profiles")
        .join(hash_path(&desired.catalog_root))
        .join(hash_text(&desired.this_host))
        .join(hash_text(&format!(
            "{}\0{}\0{}",
            desired.recipient, desired.scheme, desired.binding_name
        ))))
}

fn lexical_absolute(path: &Path) -> anyhow::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
        }
    }
    Ok(normalized)
}

fn hash_path(path: &Path) -> String {
    hash_text(&path.to_string_lossy())
}

fn hash_text(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn runtime_keys_enforce_shared_and_per_binding_topology() {
        let shared_a = RuntimeKey::Shared {
            scheme: "dev.x".into(),
            generation: 7,
        };
        let shared_b = RuntimeKey::Shared {
            scheme: "dev.x".into(),
            generation: 7,
        };
        assert_eq!(shared_a, shared_b);
        let per_a = RuntimeKey::PerBinding {
            scheme: "dev.x".into(),
            generation: 7,
            binding: hash_text("a"),
        };
        let per_b = RuntimeKey::PerBinding {
            scheme: "dev.x".into(),
            generation: 7,
            binding: hash_text("b"),
        };
        assert_ne!(per_a, per_b);
    }

    #[test]
    fn stale_output_key_cannot_alias_a_hot_reloaded_generation() {
        assert_ne!(
            RuntimeKey::Shared {
                scheme: "dev.x".into(),
                generation: 1,
            },
            RuntimeKey::Shared {
                scheme: "dev.x".into(),
                generation: 2,
            }
        );
    }

    #[test]
    fn demand_batches_coalesce_bursts_and_keep_one_trailing_watermark() {
        let mut demand = DemandState::default();
        let first = ObserveRequest::new("h.a".into(), "one".into(), None, None).unwrap();
        let second = ObserveRequest::new("h.a".into(), "one".into(), None, None).unwrap();
        demand
            .push(first, PathBuf::from("first.json"), SystemTime::now())
            .unwrap();
        demand
            .push(second, PathBuf::from("second.json"), SystemTime::now())
            .unwrap();
        assert_eq!(demand.trailing.as_ref().unwrap().watermark, 1);
        assert_eq!(demand.trailing.as_ref().unwrap().requests.len(), 2);

        let mut in_flight = demand.trailing.take().unwrap();
        in_flight.dispatched_at = Some(Instant::now());
        demand.in_flight = Some(in_flight);
        demand
            .push(
                ObserveRequest::new("h.a".into(), "one".into(), None, None).unwrap(),
                PathBuf::from("third.json"),
                SystemTime::now(),
            )
            .unwrap();
        demand
            .push(
                ObserveRequest::new("h.a".into(), "one".into(), None, None).unwrap(),
                PathBuf::from("fourth.json"),
                SystemTime::now(),
            )
            .unwrap();
        assert_eq!(demand.in_flight.as_ref().unwrap().watermark, 1);
        assert_eq!(demand.trailing.as_ref().unwrap().watermark, 2);
        assert_eq!(demand.trailing.as_ref().unwrap().requests.len(), 2);
    }

    #[test]
    fn terminal_receipt_write_failure_preserves_the_batch_for_retry() {
        let temporary = tempfile::tempdir().unwrap();
        let request_dir = temporary.path().join("requests");
        let receipt_dir = temporary.path().join("receipts");
        fs::create_dir_all(&request_dir).unwrap();
        fs::create_dir_all(&receipt_dir).unwrap();
        let request = ObserveRequest::new("h.a".into(), "one".into(), None, None).unwrap();
        let request_path = request_dir.join(format!("{}.json", request.request_id));
        fs::write(&request_path, serde_json::to_vec(&request).unwrap()).unwrap();
        let receipt_path = receipt_dir.join(format!("{}.json", request.request_id));
        fs::create_dir(&receipt_path).unwrap();
        let mut batch = DemandBatch {
            watermark: 1,
            requests: vec![PendingDemand {
                request: request.clone(),
                request_path: request_path.clone(),
                queued_at: SystemTime::now(),
                last_status: Some(ObserveReceiptStatus::Accepted),
            }],
            dispatched_at: Some(Instant::now()),
        };
        let authority = ObservationAuthority {
            owner: RuntimeOwner::new(
                RuntimeIncarnation::new("incarnation").unwrap(),
                OwnerClaim::new("claim").unwrap(),
            ),
            binding_id: BindingId::new("binding").unwrap(),
            registration: RegistrationToken::new("registration").unwrap(),
        };

        let errors = write_batch_status(
            &request_dir,
            &receipt_dir,
            &authority,
            &mut batch,
            ObserveReceiptStatus::SettledUnchanged,
            None,
            None,
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(
            batch.requests[0].last_status,
            Some(ObserveReceiptStatus::Accepted)
        );
        assert!(request_path.is_file());

        fs::remove_dir(&receipt_path).unwrap();
        assert!(
            write_batch_status(
                &request_dir,
                &receipt_dir,
                &authority,
                &mut batch,
                ObserveReceiptStatus::SettledUnchanged,
                None,
                None,
            )
            .is_empty()
        );
        assert_eq!(
            batch.requests[0].last_status,
            Some(ObserveReceiptStatus::SettledUnchanged)
        );
        assert!(!request_path.exists());
        assert_eq!(
            read_receipt(&receipt_dir, &request.request_id)
                .unwrap()
                .unwrap()
                .status,
            ObserveReceiptStatus::SettledUnchanged
        );
    }

    #[test]
    fn publication_subject_renders_ordered_facts_and_reserves_topics() {
        let facts = vec![
            ResourceFact::current("state", "ready").unwrap(),
            ResourceFact::transition("label", None::<String>, Some("bug")).unwrap(),
            ResourceFact::transition("owner", Some("alice"), Some("bob")).unwrap(),
            ResourceFact::current("fourth", "omitted").unwrap(),
        ];
        assert_eq!(
            resource_change_subject(
                "review",
                &facts,
                &["ci.failure".to_owned()],
                "snapshot updated"
            ),
            "review · state=ready; label=+bug; owner=alice→bob [ci.failure]"
        );
    }

    #[test]
    fn publication_subject_drops_low_priority_facts_atomically_within_96_scalars() {
        let facts = vec![
            ResourceFact::current("priority", "x".repeat(80)).unwrap(),
            ResourceFact::current("lower", "must-not-leapfrog").unwrap(),
        ];
        let subject = resource_change_subject(
            "review",
            &facts,
            &["selected.topic".to_owned()],
            "snapshot updated",
        );
        assert_eq!(subject, "review · snapshot updated [selected.topic]");
        assert!(subject.chars().count() <= 96);
    }

    #[test]
    fn publication_subject_without_facts_has_a_useful_compatible_fallback() {
        assert_eq!(
            resource_change_subject("review", &[], &[], "snapshot updated"),
            "review · snapshot updated"
        );
    }

    #[cfg(feature = "wasip2-provider-runtime")]
    #[test]
    fn observation_panic_becomes_a_typed_failed_completion() {
        let result = catch_observation(|| panic!("synthetic provider panic"));
        assert_eq!(result.unwrap_err(), "provider observation panicked");
    }
}
