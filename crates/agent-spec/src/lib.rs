//! agent-spec — read a catalog of rendered agent declarations.
//!
//! One agent is one declarative file, Nomad-style: the agent is the job and its `pty`/`exec` blocks
//! are the tasks. This crate owns the two halves any reader of that catalog needs and nothing else:
//!
//! - [`spec`] — the shared model a declaration lowers to ([`AgentSpec`], [`Task`], [`Resource`], …).
//! - [`discovery`] — the catalog walk: parse every `*.{kdl,toml,json}` that looks like a
//!   declaration, and resolve each one's `identity`/`host` with the catalog's precedence rule
//!   (content wins, the path supplies defaults, a mismatch is a warning).
//! - [`declared`] — the strict, source-located canonical KDL document. It preserves the complete
//!   typed node tree and duplicate occurrences for publication and downstream policy checks.
//!
//! KDL is the canonical on-disk format; TOML and JSON lower to the runner model. Consumers that need
//! declared shape use [`DeclaredDocument`]; consumers that need resolved runtime meaning use
//! [`discovery`]. Both flow through the same KDL parser.
//!
//! st2 consumes this crate, which is what keeps it a reference implementation rather than a copy:
//! a second reader (a TUI, a linter) sees exactly the fields the runner sees, including the ones
//! the runner's roster JSON does not carry (`supervisor`, `role`, `workspace`, `host`).
//!
//! The runner-normalized model deliberately drops render-only fields. [`DeclaredDocument`] retains
//! them without assigning policy, so st2 stays render-agnostic while policy consumers do not need a
//! second parser.

/// Source revision of the complete public parser and lowering contract.
///
/// Hermetic builds inject the full source revision. Native builds use the full clean Git revision
/// or an explicit `local-dirty.*`/`local.unknown` identity, so unlike builds cannot compare equal.
pub const AGENT_SPEC_REVISION: &str = env!("AGENT_SPEC_REVISION");

pub mod declared;
pub mod discovery;
mod kdl_format;
pub mod spec;
pub mod profile;
#[cfg(feature = "wasm-resolver")]
pub mod profile_wasm;

pub use declared::{
    DeclaredAgent, DeclaredDiagnostic, DeclaredDiagnosticCode, DeclaredDocument, DeclaredEntry,
    DeclaredNode, DeclaredParse, DeclaredSeverity, DeclaredSpan, DeclaredValue,
    parse_declared_document, parse_declared_file,
};
pub use discovery::{
    Declared, Discovered, SpecError, discover, discover_file, discover_strict, is_catalog_path,
    parse_declared, path_defaults,
};
pub use spec::{
    AGENT_ADDRESS_MAX_BYTES, AGENT_ADDRESS_SEGMENT_MAX_BYTES, AGENT_ID_MAX_BYTES,
    AgentDesiredState, AgentSpec, ClaudeDriver, CodexDriver, DeliveryReadiness, DeliveryTransport,
    Driver, JobType, OpenCodeDriver, PiDriver, Resource, Restart, RestartMode, STREAM_TASK_PREFIX,
    SessionDriver, Stream, StreamLaunch, Task, TaskKind, TaskLifecycle, parse_duration,
    stream_name_of_task, validate_agent_address, validate_agent_id,
    validate_desired_state_reason,
};
pub use profile::{
    DEFAULT_SELECTOR_LIMIT_BYTES, DescriptorValidationError, PROFILE_DESCRIPTOR_ABI_VERSION,
    ProfileCapability, ProfileClass, ProfileDescriptor, ProfileRuntime, ProfileSnapshot,
    ProfileSource, ProfileTopic, Resolution, ResourceProfile, ResourceProfileRefresh,
    ResourceProfileRegistry, RuntimeTopology, SelectorSchema, SelectorValidationError,
};
