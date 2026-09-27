//! Typed, capability-closed Component Model execution for resource observations.

use serde_json::Value;
use st2_resource_protocol::SnapshotDigest;

#[derive(Debug, Clone, PartialEq)]
pub struct ObservationRequest {
    /// Host-only identity for cancellation ownership; it is never passed to the guest.
    pub invocation_id: u64,
    pub uri: String,
    pub selector: Value,
    pub prior_digest: Option<SnapshotDigest>,
    pub demand_watermark: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SchedulingCapability {
    Demand,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderDescriptor {
    pub capabilities: Vec<SchedulingCapability>,
    pub selector_schema: Value,
    pub default_selector: Value,
    pub topics: Vec<String>,
    pub snapshot_media_type: String,
    pub snapshot_schema_id: String,
}

#[cfg(feature = "runtime")]
mod bindings;
#[cfg(feature = "runtime")]
mod cache;
#[cfg(feature = "runtime")]
mod limits;

#[cfg(feature = "runtime")]
pub use cache::{CacheDisposition, CacheOpenError, CacheRejection, PrivateArtifactCache};

#[cfg(feature = "runtime")]
mod runtime {
    use std::collections::{BTreeSet, HashMap};
    use std::fmt;
    use std::hash::{DefaultHasher, Hash as _, Hasher as _};
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Arc, Condvar, Mutex};

    use sha2::{Digest as _, Sha256};
    use st2_resource_protocol::{
        FactError, FactValue, MAX_SELECTOR_BYTES, ObservationResult, ProtocolError, Publication,
        ResourceFact, SnapshotBytes, SnapshotSizeError, validate_topics,
    };
    use wasmtime::component::{Component, Linker};
    use wasmtime::{Config, Engine, Store, Trap, UpdateDeadline};

    use crate::bindings::Provider;
    use crate::bindings::exports::st2::resource_provider::provider_api as guest;
    use crate::cache::{self, CacheDisposition, CacheIdentity, CacheLookup, PrivateArtifactCache};
    use crate::limits::InvocationLimits;
    use crate::{ObservationRequest, ProviderDescriptor, SchedulingCapability};

    pub const WASMTIME_VERSION: &str = "48.0.1";
    pub const DEFAULT_MAX_COMPONENT_BYTES: usize = 16 * 1024 * 1024;
    pub const DEFAULT_FUEL_PER_OBSERVATION: u64 = 10_000_000;
    pub const DEFAULT_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;
    pub const DEFAULT_MAX_TABLE_ELEMENTS: usize = 10_000;
    pub const DEFAULT_MAX_INSTANCES: usize = 128;
    pub const DEFAULT_MAX_TABLES: usize = 64;
    pub const DEFAULT_MAX_MEMORIES: usize = 64;
    const MAX_DESCRIPTOR_JSON_BYTES: usize = 64 * 1024;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RuntimeConfig {
        pub max_component_bytes: usize,
        pub fuel_per_observation: u64,
        pub max_memory_bytes: usize,
        pub max_table_elements: usize,
        pub max_instances: usize,
        pub max_tables: usize,
        pub max_memories: usize,
    }

    impl Default for RuntimeConfig {
        fn default() -> Self {
            Self {
                max_component_bytes: DEFAULT_MAX_COMPONENT_BYTES,
                fuel_per_observation: DEFAULT_FUEL_PER_OBSERVATION,
                max_memory_bytes: DEFAULT_MAX_MEMORY_BYTES,
                max_table_elements: DEFAULT_MAX_TABLE_ELEMENTS,
                max_instances: DEFAULT_MAX_INSTANCES,
                max_tables: DEFAULT_MAX_TABLES,
                max_memories: DEFAULT_MAX_MEMORIES,
            }
        }
    }

    impl RuntimeConfig {
        fn validate(&self) -> Result<(), BuildError> {
            let nonzero = self.max_component_bytes > 0
                && self.fuel_per_observation > 0
                && self.max_memory_bytes > 0
                && self.max_table_elements > 0
                && self.max_instances > 0
                && self.max_tables > 0
                && self.max_memories > 0;
            if nonzero {
                Ok(())
            } else {
                Err(BuildError::InvalidConfig("all limits must be nonzero"))
            }
        }

        fn identity(&self) -> String {
            sha256_hex(
                format!(
                    "component-model=1;fuel=1;epoch=1;component-bytes={};fuel-per-call={};memory={};table-elements={};instances={};tables={};memories={}",
                    self.max_component_bytes,
                    self.fuel_per_observation,
                    self.max_memory_bytes,
                    self.max_table_elements,
                    self.max_instances,
                    self.max_tables,
                    self.max_memories,
                )
                .as_bytes(),
            )
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum InterruptionReason {
        Cancelled,
        TimedOut,
    }

    struct InvocationControlState {
        reason: AtomicU8,
        changed: Mutex<()>,
        wake: Condvar,
    }

    #[derive(Clone)]
    pub struct InvocationControl {
        state: Arc<InvocationControlState>,
    }

    impl InvocationControl {
        fn new() -> Self {
            Self {
                state: Arc::new(InvocationControlState {
                    reason: AtomicU8::new(INTERRUPTION_NONE),
                    changed: Mutex::new(()),
                    wake: Condvar::new(),
                }),
            }
        }

        pub fn interruption_reason(&self) -> Option<InterruptionReason> {
            match self.state.reason.load(Ordering::Acquire) {
                INTERRUPTION_CANCELLED => Some(InterruptionReason::Cancelled),
                INTERRUPTION_TIMED_OUT => Some(InterruptionReason::TimedOut),
                _ => None,
            }
        }

        pub fn wait_for_interruption(&self) -> InterruptionReason {
            let mut guard = self
                .state
                .changed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                if let Some(reason) = self.interruption_reason() {
                    return reason;
                }
                guard = self
                    .state
                    .wake
                    .wait(guard)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }

        fn interrupt(&self, reason: u8) -> bool {
            let guard = self
                .state
                .changed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let changed = self
                .state
                .reason
                .compare_exchange(
                    INTERRUPTION_NONE,
                    reason,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok();
            if changed {
                self.state.wake.notify_all();
            }
            drop(guard);
            changed
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub enum CapabilityPhase<'a> {
        Describe,
        Observe(&'a ObservationRequest),
    }

    #[derive(Clone)]
    pub struct CapabilityContext<'a> {
        phase: CapabilityPhase<'a>,
        control: InvocationControl,
    }

    impl<'a> CapabilityContext<'a> {
        pub fn phase(&self) -> CapabilityPhase<'a> {
            self.phase
        }

        pub fn control(&self) -> &InvocationControl {
            &self.control
        }
    }

    pub trait CapabilityModule: Send + Sync + 'static {
        type Invocation: Send + 'static;

        fn import_names(&self) -> &'static [&'static str];

        fn add_to_linker(
            &self,
            linker: &mut Linker<InvocationStore<Self::Invocation>>,
        ) -> Result<(), wasmtime::Error>;

        fn begin(&self, context: CapabilityContext<'_>) -> Self::Invocation;
    }

    #[derive(Debug, Clone, Copy, Default)]
    pub struct NoCapabilities;

    impl CapabilityModule for NoCapabilities {
        type Invocation = ();

        fn import_names(&self) -> &'static [&'static str] {
            &[]
        }

        fn add_to_linker(
            &self,
            _linker: &mut Linker<InvocationStore<Self::Invocation>>,
        ) -> Result<(), wasmtime::Error> {
            Ok(())
        }

        fn begin(&self, _context: CapabilityContext<'_>) -> Self::Invocation {}
    }

    pub struct InvocationStore<T> {
        capability: T,
        control: InvocationControl,
        limits: InvocationLimits,
    }

    impl<T> InvocationStore<T> {
        pub fn capability(&self) -> &T {
            &self.capability
        }

        pub fn capability_mut(&mut self) -> &mut T {
            &mut self.capability
        }

        pub fn control(&self) -> &InvocationControl {
            &self.control
        }
    }

    pub struct Executor<C: CapabilityModule = NoCapabilities> {
        engine: Engine,
        linker: Arc<Linker<InvocationStore<C::Invocation>>>,
        capabilities: Arc<C>,
        allowed_imports: Arc<BTreeSet<&'static str>>,
        components: Mutex<HashMap<ComponentDigest, Arc<Component>>>,
        config: RuntimeConfig,
        cache: Option<PrivateArtifactCache>,
        cache_identity: CacheIdentity,
        runtime_token: Arc<()>,
    }

    impl Executor<NoCapabilities> {
        pub fn closed(
            config: RuntimeConfig,
            cache: Option<PrivateArtifactCache>,
        ) -> Result<Self, BuildError> {
            Self::new(config, cache, NoCapabilities)
        }
    }

    impl<C: CapabilityModule> Executor<C> {
        pub fn new(
            config: RuntimeConfig,
            cache: Option<PrivateArtifactCache>,
            capabilities: C,
        ) -> Result<Self, BuildError> {
            config.validate()?;
            let mut wasmtime_config = Config::new();
            wasmtime_config
                .wasm_component_model(true)
                .consume_fuel(true)
                .epoch_interruption(true);
            let engine = Engine::new(&wasmtime_config)
                .map_err(|error| BuildError::Engine(error.to_string()))?;
            let mut linker = Linker::new(&engine);
            capabilities
                .add_to_linker(&mut linker)
                .map_err(|error| BuildError::Linker(error.to_string()))?;
            let allowed_imports = capabilities.import_names().iter().copied().collect();
            let mut compatibility_hasher = DefaultHasher::new();
            engine
                .precompile_compatibility_hash()
                .hash(&mut compatibility_hasher);
            let cache_identity = CacheIdentity {
                executor_build_identity: executor_build_identity().to_owned(),
                wasmtime_version: WASMTIME_VERSION,
                target: env!("ST2_WASIP2_TARGET"),
                engine_compatibility: format!("{:016x}", compatibility_hasher.finish()),
                config_identity: config.identity(),
            };
            Ok(Self {
                engine,
                linker: Arc::new(linker),
                capabilities: Arc::new(capabilities),
                allowed_imports: Arc::new(allowed_imports),
                components: Mutex::new(HashMap::new()),
                config,
                cache,
                cache_identity,
                runtime_token: Arc::new(()),
            })
        }

        pub fn load(&self, bytes: &[u8]) -> Result<LoadedComponent, LoadError> {
            if bytes.len() > self.config.max_component_bytes {
                return Err(LoadError::ComponentTooLarge {
                    actual: bytes.len(),
                    maximum: self.config.max_component_bytes,
                });
            }
            let digest = ComponentDigest::of(bytes);
            // Compilation is intentionally serialized per executor: racing the same digest must
            // not manufacture a second compiled identity outside the one immutable cache.
            let mut components = self
                .components
                .lock()
                .map_err(|_| LoadError::Internal("compiled component cache lock poisoned"))?;
            if let Some(component) = components.get(&digest).cloned() {
                return Ok(self.loaded(digest, component, CacheDisposition::MemoryHit));
            }

            let lookup = self.cache.as_ref().map_or(CacheLookup::Miss, |cache| {
                cache::load(cache, &self.engine, digest, &self.cache_identity)
            });
            let (component, disposition) = match lookup {
                CacheLookup::Hit(component) => (component, CacheDisposition::DiskHit),
                CacheLookup::Miss => {
                    let component = self.compile(bytes)?;
                    let disposition = match &self.cache {
                        Some(cache) => {
                            match cache::store(cache, &component, digest, &self.cache_identity) {
                                Ok(()) => CacheDisposition::CompiledAndStored,
                                Err(error) => CacheDisposition::CompiledButNotStored(error),
                            }
                        }
                        None => CacheDisposition::CompiledWithoutCache,
                    };
                    (component, disposition)
                }
                CacheLookup::Rejected(rejection) => {
                    let component = self.compile(bytes)?;
                    (component, CacheDisposition::RejectedAndCompiled(rejection))
                }
            };
            self.admit_imports(&component)?;
            let component = Arc::new(component);
            components.insert(digest, Arc::clone(&component));
            Ok(self.loaded(digest, component, disposition))
        }

        pub fn interruption_handle(&self) -> InterruptionHandle {
            InterruptionHandle {
                engine: self.engine.clone(),
                control: InvocationControl::new(),
                runtime_token: Arc::clone(&self.runtime_token),
            }
        }

        pub fn describe(
            &self,
            component: &LoadedComponent,
            interruption: Option<&InterruptionHandle>,
        ) -> Result<ProviderDescriptor, DescribeError> {
            if !Arc::ptr_eq(&self.runtime_token, &component.runtime_token) {
                return Err(DescribeError::WrongExecutor);
            }
            if let Some(interruption) = interruption
                && !Arc::ptr_eq(&self.runtime_token, &interruption.runtime_token)
            {
                return Err(DescribeError::WrongExecutor);
            }
            let control =
                interruption.map_or_else(InvocationControl::new, |handle| handle.control.clone());
            if let Some(reason) = control.interruption_reason() {
                return Err(DescribeError::from(reason));
            }
            let mut store = self
                .begin_invocation(CapabilityPhase::Describe, &control)
                .map_err(|error| DescribeError::Instantiation(error.to_string()))?;
            let bindings = Provider::instantiate(
                &mut store,
                component.component.as_ref(),
                self.linker.as_ref(),
            )
            .map_err(|error| {
                classify_execution_error(&store, &control.state.reason, error, true).describe()
            })?;
            let result = bindings
                .st2_resource_provider_provider_api()
                .call_describe(&mut store)
                .map_err(|error| {
                    classify_execution_error(&store, &control.state.reason, error, false).describe()
                })?;
            if let Some(reason) = control.interruption_reason() {
                return Err(DescribeError::from(reason));
            }
            let descriptor = result.map_err(GuestDescriptorError::from)?;
            map_descriptor(descriptor).map_err(DescribeError::InvalidDescriptor)
        }

        pub fn observe(
            &self,
            component: &LoadedComponent,
            request: &ObservationRequest,
            interruption: Option<&InterruptionHandle>,
        ) -> Result<ObservationResult, ObserveError> {
            if !Arc::ptr_eq(&self.runtime_token, &component.runtime_token) {
                return Err(ObserveError::WrongExecutor);
            }
            if let Some(interruption) = interruption
                && !Arc::ptr_eq(&self.runtime_token, &interruption.runtime_token)
            {
                return Err(ObserveError::WrongExecutor);
            }
            if request.uri.len() > 64 * 1024 {
                return Err(ObserveError::InvalidRequest("URI exceeds 64 KiB"));
            }
            if matches!(request.demand_watermark, Some(0)) {
                return Err(ObserveError::InvalidRequest(
                    "demand watermark must be positive",
                ));
            }
            let selector_json = serde_json::to_string(&request.selector)
                .map_err(|_| ObserveError::InvalidRequest("selector is not JSON"))?;
            if selector_json.len() > MAX_SELECTOR_BYTES {
                return Err(ObserveError::InvalidRequest(
                    "selector exceeds the protocol limit",
                ));
            }
            let guest_request = guest::ObserveRequest {
                uri: request.uri.clone(),
                selector_json,
                prior_digest: request
                    .prior_digest
                    .map(|digest| digest.as_bytes().to_vec()),
                demand_watermark: request.demand_watermark,
            };
            let control =
                interruption.map_or_else(InvocationControl::new, |handle| handle.control.clone());
            if let Some(reason) = control.interruption_reason() {
                return Err(ObserveError::from(reason));
            }
            let mut store = self
                .begin_invocation(CapabilityPhase::Observe(request), &control)
                .map_err(|error| ObserveError::Instantiation(error.to_string()))?;
            let bindings = Provider::instantiate(
                &mut store,
                component.component.as_ref(),
                self.linker.as_ref(),
            )
            .map_err(|error| {
                classify_execution_error(&store, &control.state.reason, error, true).observe()
            })?;
            let result = bindings
                .st2_resource_provider_provider_api()
                .call_observe(&mut store, &guest_request)
                .map_err(|error| {
                    classify_execution_error(&store, &control.state.reason, error, false).observe()
                })?;
            if let Some(reason) = control.interruption_reason() {
                return Err(ObserveError::from(reason));
            }
            map_proposal(result).map_err(ObserveError::InvalidProposal)
        }

        fn begin_invocation(
            &self,
            phase: CapabilityPhase<'_>,
            control: &InvocationControl,
        ) -> Result<Store<InvocationStore<C::Invocation>>, wasmtime::Error> {
            let state = InvocationStore {
                capability: self.capabilities.begin(CapabilityContext {
                    phase,
                    control: control.clone(),
                }),
                control: control.clone(),
                limits: InvocationLimits::new(&self.config),
            };
            let mut store = Store::new(&self.engine, state);
            store.limiter(|state| &mut state.limits);
            store.set_fuel(self.config.fuel_per_observation)?;
            arm_epoch_deadline(&mut store, control);
            Ok(store)
        }

        fn compile(&self, bytes: &[u8]) -> Result<Component, LoadError> {
            let component = Component::new(&self.engine, bytes)
                .map_err(|error| LoadError::Compilation(format!("{error:#}")))?;
            self.admit_imports(&component)?;
            Ok(component)
        }

        fn admit_imports(&self, component: &Component) -> Result<(), LoadError> {
            let forbidden: Vec<String> = component
                .component_type()
                .imports(&self.engine)
                .map(|(name, _)| name)
                .filter(|name| !self.allowed_imports.contains(name))
                .map(str::to_owned)
                .collect();
            if forbidden.is_empty() {
                Ok(())
            } else {
                Err(LoadError::ForbiddenImports(forbidden))
            }
        }

        fn loaded(
            &self,
            digest: ComponentDigest,
            component: Arc<Component>,
            cache_disposition: CacheDisposition,
        ) -> LoadedComponent {
            LoadedComponent {
                digest,
                component,
                cache_disposition,
                runtime_token: Arc::clone(&self.runtime_token),
            }
        }
    }

    pub struct LoadedComponent {
        digest: ComponentDigest,
        component: Arc<Component>,
        cache_disposition: CacheDisposition,
        runtime_token: Arc<()>,
    }

    impl LoadedComponent {
        pub fn digest(&self) -> ComponentDigest {
            self.digest
        }

        pub fn cache_disposition(&self) -> &CacheDisposition {
            &self.cache_disposition
        }
    }

    const INTERRUPTION_NONE: u8 = 0;
    const INTERRUPTION_CANCELLED: u8 = 1;
    const INTERRUPTION_TIMED_OUT: u8 = 2;

    #[derive(Clone)]
    pub struct InterruptionHandle {
        engine: Engine,
        control: InvocationControl,
        runtime_token: Arc<()>,
    }

    impl InterruptionHandle {
        pub fn cancel(&self) -> bool {
            self.interrupt(INTERRUPTION_CANCELLED)
        }

        pub fn time_out(&self) -> bool {
            self.interrupt(INTERRUPTION_TIMED_OUT)
        }

        fn interrupt(&self, reason: u8) -> bool {
            let changed = self.control.interrupt(reason);
            if changed {
                self.engine.increment_epoch();
            }
            changed
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum LimitKind {
        Memory,
        Table,
    }

    #[derive(Debug)]
    pub enum BuildError {
        InvalidConfig(&'static str),
        Engine(String),
        Linker(String),
    }

    impl fmt::Display for BuildError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::InvalidConfig(message) => {
                    write!(formatter, "invalid executor config: {message}")
                }
                Self::Engine(message) => {
                    write!(formatter, "cannot create Wasmtime engine: {message}")
                }
                Self::Linker(message) => {
                    write!(formatter, "cannot create capability linker: {message}")
                }
            }
        }
    }

    impl std::error::Error for BuildError {}

    #[derive(Debug)]
    pub enum LoadError {
        ComponentTooLarge { actual: usize, maximum: usize },
        Compilation(String),
        ForbiddenImports(Vec<String>),
        Internal(&'static str),
    }

    impl fmt::Display for LoadError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::ComponentTooLarge { actual, maximum } => write!(
                    formatter,
                    "component is {actual} bytes; maximum is {maximum}",
                ),
                Self::Compilation(message) => {
                    write!(formatter, "component compilation failed: {message}")
                }
                Self::ForbiddenImports(imports) => {
                    write!(
                        formatter,
                        "component imports are not admitted: {}",
                        imports.join(", ")
                    )
                }
                Self::Internal(message) => formatter.write_str(message),
            }
        }
    }

    impl std::error::Error for LoadError {}

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum GuestDescriptorError {
        InvalidDescriptor(String),
        Unavailable(String),
    }

    impl From<guest::DescriptorError> for GuestDescriptorError {
        fn from(error: guest::DescriptorError) -> Self {
            match error {
                guest::DescriptorError::InvalidDescriptor(message) => {
                    Self::InvalidDescriptor(message)
                }
                guest::DescriptorError::Unavailable(message) => Self::Unavailable(message),
            }
        }
    }

    impl fmt::Display for GuestDescriptorError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::InvalidDescriptor(message) => {
                    write!(formatter, "guest descriptor is invalid: {message}")
                }
                Self::Unavailable(message) => {
                    write!(formatter, "guest descriptor is unavailable: {message}")
                }
            }
        }
    }

    impl std::error::Error for GuestDescriptorError {}

    #[derive(Debug)]
    pub enum DescriptorValidationError {
        DuplicateCapability,
        SelectorSchemaTooLarge { actual: usize },
        SelectorSchemaJson(serde_json::Error),
        DefaultSelectorTooLarge { actual: usize },
        DefaultSelectorJson(serde_json::Error),
        InvalidTopics(ProtocolError),
        EmptySnapshotMediaType,
        EmptySnapshotSchemaId,
    }

    impl fmt::Display for DescriptorValidationError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::DuplicateCapability => {
                    formatter.write_str("descriptor capabilities must be unique")
                }
                Self::SelectorSchemaTooLarge { actual } => write!(
                    formatter,
                    "selector schema is {actual} bytes; maximum is {MAX_DESCRIPTOR_JSON_BYTES}",
                ),
                Self::SelectorSchemaJson(error) => {
                    write!(formatter, "selector schema is not valid JSON: {error}")
                }
                Self::DefaultSelectorTooLarge { actual } => write!(
                    formatter,
                    "default selector is {actual} bytes; maximum is {MAX_SELECTOR_BYTES}",
                ),
                Self::DefaultSelectorJson(error) => {
                    write!(formatter, "default selector is not valid JSON: {error}")
                }
                Self::InvalidTopics(error) => error.fmt(formatter),
                Self::EmptySnapshotMediaType => {
                    formatter.write_str("snapshot media type must not be empty")
                }
                Self::EmptySnapshotSchemaId => {
                    formatter.write_str("snapshot schema ID must not be empty")
                }
            }
        }
    }

    impl std::error::Error for DescriptorValidationError {}

    #[derive(Debug)]
    pub enum DescribeError {
        WrongExecutor,
        Instantiation(String),
        Invocation(String),
        Trap(Trap),
        FuelExhausted,
        Cancelled,
        TimedOut,
        ResourceLimit(LimitKind),
        Guest(GuestDescriptorError),
        InvalidDescriptor(DescriptorValidationError),
    }

    impl From<GuestDescriptorError> for DescribeError {
        fn from(error: GuestDescriptorError) -> Self {
            Self::Guest(error)
        }
    }

    impl From<InterruptionReason> for DescribeError {
        fn from(reason: InterruptionReason) -> Self {
            match reason {
                InterruptionReason::Cancelled => Self::Cancelled,
                InterruptionReason::TimedOut => Self::TimedOut,
            }
        }
    }

    impl fmt::Display for DescribeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::WrongExecutor => formatter
                    .write_str("component or interruption handle belongs to another executor"),
                Self::Instantiation(message) => {
                    write!(formatter, "component instantiation failed: {message}")
                }
                Self::Invocation(message) => {
                    write!(formatter, "typed describe call failed: {message}")
                }
                Self::Trap(trap) => write!(formatter, "guest trapped: {trap}"),
                Self::FuelExhausted => formatter.write_str("guest exhausted its fuel allowance"),
                Self::Cancelled => formatter.write_str("descriptor call was cancelled"),
                Self::TimedOut => formatter.write_str("descriptor call timed out"),
                Self::ResourceLimit(kind) => write!(formatter, "guest exceeded its {kind:?} limit"),
                Self::Guest(error) => error.fmt(formatter),
                Self::InvalidDescriptor(error) => {
                    write!(formatter, "guest descriptor is invalid: {error}")
                }
            }
        }
    }

    impl std::error::Error for DescribeError {}

    #[derive(Debug)]
    pub enum ProposalError {
        Snapshot(SnapshotSizeError),
        Fact(FactError),
        Protocol(ProtocolError),
    }

    impl fmt::Display for ProposalError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Snapshot(error) => error.fmt(formatter),
                Self::Fact(error) => error.fmt(formatter),
                Self::Protocol(error) => error.fmt(formatter),
            }
        }
    }

    impl std::error::Error for ProposalError {}

    #[derive(Debug)]
    pub enum ObserveError {
        WrongExecutor,
        InvalidRequest(&'static str),
        Instantiation(String),
        Invocation(String),
        Trap(Trap),
        FuelExhausted,
        Cancelled,
        TimedOut,
        ResourceLimit(LimitKind),
        InvalidProposal(ProposalError),
    }

    impl From<InterruptionReason> for ObserveError {
        fn from(reason: InterruptionReason) -> Self {
            match reason {
                InterruptionReason::Cancelled => Self::Cancelled,
                InterruptionReason::TimedOut => Self::TimedOut,
            }
        }
    }

    impl fmt::Display for ObserveError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::WrongExecutor => formatter
                    .write_str("component or interruption handle belongs to another executor"),
                Self::InvalidRequest(message) => {
                    write!(formatter, "invalid observation request: {message}")
                }
                Self::Instantiation(message) => {
                    write!(formatter, "component instantiation failed: {message}")
                }
                Self::Invocation(message) => {
                    write!(formatter, "typed observation call failed: {message}")
                }
                Self::Trap(trap) => write!(formatter, "guest trapped: {trap}"),
                Self::FuelExhausted => formatter.write_str("guest exhausted its fuel allowance"),
                Self::Cancelled => formatter.write_str("observation was cancelled"),
                Self::TimedOut => formatter.write_str("observation timed out"),
                Self::ResourceLimit(kind) => write!(formatter, "guest exceeded its {kind:?} limit"),
                Self::InvalidProposal(error) => {
                    write!(formatter, "guest proposal is invalid: {error}")
                }
            }
        }
    }

    impl std::error::Error for ObserveError {}

    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    pub struct ComponentDigest([u8; 32]);

    impl ComponentDigest {
        pub fn of(bytes: &[u8]) -> Self {
            Self(Sha256::digest(bytes).into())
        }

        pub fn as_bytes(&self) -> &[u8; 32] {
            &self.0
        }
    }

    impl fmt::Debug for ComponentDigest {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt::Display::fmt(self, formatter)
        }
    }

    impl fmt::Display for ComponentDigest {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            for byte in self.0 {
                write!(formatter, "{byte:02x}")?;
            }
            Ok(())
        }
    }

    fn arm_epoch_deadline<T: 'static>(store: &mut Store<T>, control: &InvocationControl) {
        arm_epoch_deadline_with_hook(store, control, || {});
    }

    fn arm_epoch_deadline_with_hook<T: 'static>(
        store: &mut Store<T>,
        control: &InvocationControl,
        before_deadline: impl FnOnce(),
    ) {
        let callback_state = Arc::clone(&control.state);
        store.epoch_deadline_callback(move |_| {
            if callback_state.reason.load(Ordering::Acquire) == INTERRUPTION_NONE {
                Ok(UpdateDeadline::Continue(1))
            } else {
                Ok(UpdateDeadline::Interrupt)
            }
        });
        before_deadline();
        store.set_epoch_deadline(1);
        // Cancellation may have incremented the engine epoch just before the deadline was armed.
        // Rechecking closes that lost-tick window without requiring a second external tick.
        if control.interruption_reason().is_some() {
            store.set_epoch_deadline(0);
        }
    }

    enum InvocationFailure {
        Instantiation(String),
        Invocation(String),
        Trap(Trap),
        FuelExhausted,
        Cancelled,
        TimedOut,
        ResourceLimit(LimitKind),
    }

    impl InvocationFailure {
        fn observe(self) -> ObserveError {
            match self {
                Self::Instantiation(message) => ObserveError::Instantiation(message),
                Self::Invocation(message) => ObserveError::Invocation(message),
                Self::Trap(trap) => ObserveError::Trap(trap),
                Self::FuelExhausted => ObserveError::FuelExhausted,
                Self::Cancelled => ObserveError::Cancelled,
                Self::TimedOut => ObserveError::TimedOut,
                Self::ResourceLimit(kind) => ObserveError::ResourceLimit(kind),
            }
        }

        fn describe(self) -> DescribeError {
            match self {
                Self::Instantiation(message) => DescribeError::Instantiation(message),
                Self::Invocation(message) => DescribeError::Invocation(message),
                Self::Trap(trap) => DescribeError::Trap(trap),
                Self::FuelExhausted => DescribeError::FuelExhausted,
                Self::Cancelled => DescribeError::Cancelled,
                Self::TimedOut => DescribeError::TimedOut,
                Self::ResourceLimit(kind) => DescribeError::ResourceLimit(kind),
            }
        }
    }

    fn classify_execution_error(
        store: &Store<InvocationStore<impl Send + 'static>>,
        reason: &AtomicU8,
        error: wasmtime::Error,
        instantiating: bool,
    ) -> InvocationFailure {
        if let Some(kind) = store.data().limits.exceeded() {
            return InvocationFailure::ResourceLimit(kind);
        }
        match reason.load(Ordering::Acquire) {
            INTERRUPTION_CANCELLED => return InvocationFailure::Cancelled,
            INTERRUPTION_TIMED_OUT => return InvocationFailure::TimedOut,
            _ => {}
        }
        if let Some(trap) = error.downcast_ref::<Trap>() {
            return match trap {
                Trap::OutOfFuel => InvocationFailure::FuelExhausted,
                trap => InvocationFailure::Trap(*trap),
            };
        }
        if instantiating {
            InvocationFailure::Instantiation(error.to_string())
        } else {
            InvocationFailure::Invocation(error.to_string())
        }
    }

    fn map_descriptor(
        descriptor: guest::ProviderDescriptor,
    ) -> Result<ProviderDescriptor, DescriptorValidationError> {
        let mut capabilities = Vec::with_capacity(descriptor.capabilities.len());
        let mut unique_capabilities = BTreeSet::new();
        for capability in descriptor.capabilities {
            let capability = match capability {
                guest::SchedulingCapability::Demand => SchedulingCapability::Demand,
            };
            if !unique_capabilities.insert(capability) {
                return Err(DescriptorValidationError::DuplicateCapability);
            }
            capabilities.push(capability);
        }
        if descriptor.selector_schema_json.len() > MAX_DESCRIPTOR_JSON_BYTES {
            return Err(DescriptorValidationError::SelectorSchemaTooLarge {
                actual: descriptor.selector_schema_json.len(),
            });
        }
        let selector_schema = serde_json::from_str(&descriptor.selector_schema_json)
            .map_err(DescriptorValidationError::SelectorSchemaJson)?;
        let default_selector = serde_json::from_str(&descriptor.default_selector_json)
            .map_err(DescriptorValidationError::DefaultSelectorJson)?;
        let default_selector_bytes = serde_json::to_vec(&default_selector)
            .expect("serializing a decoded JSON value cannot fail")
            .len();
        if default_selector_bytes > MAX_SELECTOR_BYTES {
            return Err(DescriptorValidationError::DefaultSelectorTooLarge {
                actual: default_selector_bytes,
            });
        }
        validate_topics(&descriptor.topics).map_err(DescriptorValidationError::InvalidTopics)?;
        if descriptor.snapshot_media_type.is_empty() {
            return Err(DescriptorValidationError::EmptySnapshotMediaType);
        }
        if descriptor.snapshot_schema_id.is_empty() {
            return Err(DescriptorValidationError::EmptySnapshotSchemaId);
        }
        Ok(ProviderDescriptor {
            capabilities,
            selector_schema,
            default_selector,
            topics: descriptor.topics,
            snapshot_media_type: descriptor.snapshot_media_type,
            snapshot_schema_id: descriptor.snapshot_schema_id,
        })
    }

    fn map_proposal(
        proposal: guest::ObservationResult,
    ) -> Result<ObservationResult, ProposalError> {
        let proposal = match proposal {
            guest::ObservationResult::Unchanged => ObservationResult::Unchanged,
            guest::ObservationResult::Failed(diagnostic) => {
                ObservationResult::Failed { diagnostic }
            }
            guest::ObservationResult::Published(publication) => {
                let facts = publication
                    .facts
                    .map(|facts| {
                        facts
                            .into_iter()
                            .map(map_fact)
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .transpose()?;
                ObservationResult::Published {
                    publication: Publication {
                        schema_id: publication.schema_id,
                        media_type: publication.media_type,
                        bytes: SnapshotBytes::new(publication.bytes)
                            .map_err(ProposalError::Snapshot)?,
                        topics: publication.topics,
                        facts,
                    },
                }
            }
        };
        proposal.validate().map_err(ProposalError::Protocol)?;
        Ok(proposal)
    }

    fn map_fact(fact: guest::Fact) -> Result<ResourceFact, ProposalError> {
        ResourceFact::new(
            fact.key,
            map_fact_value(fact.before),
            map_fact_value(fact.after),
        )
        .map_err(ProposalError::Fact)
    }

    fn map_fact_value(value: guest::FactValue) -> FactValue {
        match value {
            guest::FactValue::Omitted => FactValue::Omitted,
            guest::FactValue::Null => FactValue::Null,
            guest::FactValue::Value(value) => FactValue::Value(value),
        }
    }

    pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        let mut result = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write as _;
            write!(result, "{byte:02x}").expect("writing into a String cannot fail");
        }
        result
    }

    fn executor_build_identity() -> &'static str {
        option_env!("ST2_EXECUTOR_BUILD_IDENTITY").unwrap_or(concat!(
            env!("CARGO_PKG_NAME"),
            "@",
            env!("CARGO_PKG_VERSION")
        ))
    }
    #[cfg(test)]
    mod tests {
        use std::sync::{Arc, Barrier};

        use super::*;

        #[test]
        fn cancellation_before_deadline_arm_forces_the_current_epoch() {
            let mut config = Config::new();
            config.epoch_interruption(true);
            let engine = Engine::new(&config).unwrap();
            let control = InvocationControl::new();
            let handle = InterruptionHandle {
                engine: engine.clone(),
                control: control.clone(),
                runtime_token: Arc::new(()),
            };
            let barrier = Arc::new(Barrier::new(2));
            let cancelling_barrier = Arc::clone(&barrier);
            let cancelling = std::thread::spawn(move || {
                cancelling_barrier.wait();
                assert!(handle.cancel());
                cancelling_barrier.wait();
            });
            let mut store = Store::new(&engine, ());
            arm_epoch_deadline_with_hook(&mut store, &control, || {
                barrier.wait();
                barrier.wait();
            });
            cancelling.join().unwrap();

            let module = wasmtime::Module::new(
                &engine,
                wat::parse_str("(module (func (export \"run\") (loop $spin (br $spin))))").unwrap(),
            )
            .unwrap();
            let instance = wasmtime::Instance::new(&mut store, &module, &[]).unwrap();
            let run = instance
                .get_typed_func::<(), ()>(&mut store, "run")
                .unwrap();
            let error = run.call(&mut store, ()).unwrap_err();
            assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::Interrupt));
        }
    }
}

#[cfg(feature = "runtime")]
pub use runtime::{
    BuildError, CapabilityContext, CapabilityModule, CapabilityPhase, ComponentDigest,
    DEFAULT_FUEL_PER_OBSERVATION, DEFAULT_MAX_COMPONENT_BYTES, DEFAULT_MAX_INSTANCES,
    DEFAULT_MAX_MEMORIES, DEFAULT_MAX_MEMORY_BYTES, DEFAULT_MAX_TABLE_ELEMENTS, DEFAULT_MAX_TABLES,
    DescribeError, DescriptorValidationError, Executor, GuestDescriptorError, InterruptionHandle,
    InterruptionReason, InvocationControl, InvocationStore, LimitKind, LoadError, LoadedComponent,
    NoCapabilities, ObserveError, ProposalError, RuntimeConfig, WASMTIME_VERSION,
};

#[cfg(feature = "runtime")]
pub(crate) use runtime::sha256_hex;
