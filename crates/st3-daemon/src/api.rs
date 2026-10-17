use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Extension, Path as AxumPath, Query, State};
use axum::http::{Request, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use futures_util::{SinkExt as _, StreamExt as _};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpListener;
use tokio::sync::{Notify, watch};
use tower::ServiceExt as _;

use crate::archive::hydrate_eval;
use crate::graph::{parse_intent, resolve_document_references};
use crate::message_body;
#[cfg(test)]
use crate::model::AttentionRequest;
pub(crate) use crate::client_values::{
    client_attention_actions, client_attention_id, client_timestamp, client_work_state,
};
pub(crate) use crate::store::agent_queue_values::{
    AGENT_QUEUE_FIELDS, agent_queue_fields, agent_queue_metadata,
};
use crate::model::ClientReplicated;
use crate::model::{
    ApplyRequest, ApplyResponse, AttachRequest, Attachment, AttentionItemView,
    AttentionRequestView, AttentionResolveRequest, AttentionWithdrawRequest, ClaimInput,
    ClaimRecord, ClaimsPage, ClientPageInfo, ClientResourcePage, ClientSyncNotice, ClientSyncPeer,
    ContextClearRequest, DoctorCheck, DoctorReport, DocumentListResponse, DocumentPutRequest,
    DocumentVersion, EvalStartRequest, EvalStartResponse, EvalStatus, EventRecord,
    GateResultRequest, HumanReviewView, IntentInput, LaunchApproveAndStartRequest,
    LaunchApproveAndStartView, LaunchDecisionAnswerRequest, LaunchDecisionOption,
    LaunchDecisionRequest, LaunchDecisionResponse, LaunchDecisionType, LaunchStartRequest,
    LocalTerminal, MAX_EVAL_TIMEOUT_MS, MessageLifecycleRequest, MessagePage, MessageSendReceipt,
    MessageSendRequest, MessageView, MissionOutputView, MissionProductionRequest, MissionRequest,
    MissionResponse, MissionRetireRequest, MissionRevisionRequest, MissionRunOutcomeRequest,
    MissionRunReportRequest, MissionRunReportView, MissionRunRequest, MissionRunView,
    OperationalRepairApplyRequest, OperationalRepairPlan, OperationalRepairResult, PlannerSpec,
    PlanningApprovalRequest, PlanningCancelRequest, PlanningCandidateSubmitRequest,
    PlanningProposalRequest, PlanningRevisionRequest,
    PlanningSessionStartRequest, PlanningSessionView, QuickAgentRequest, QuickAgentResponse,
    ReplicaRecordView, ReplicationExportRequest, ReplicationExportResponse,
    ReplicationPeerFailureRequest, ReplicationReceiveRequest, ReplicationReceiveResponse,
    ReplicationRepairRequest, ReplicationStatus, ReviewRequest, RevisionApprovalRequest,
    RevisionCancelRequest, RevisionCutover, RevisionProposalView, RevisionSubmissionView,
    RunGenerationView, SessionControlResponse, SessionInputMode, SessionInputRequest,
    SessionLogChunk, SessionScreen, SessionSignalRequest, St3Error, StatusResponse, StepRunView,
    WorkRequest, WorkRetryRequest, WorkWakeRequest,
};
use crate::model::{PersonAskRequest, PersonStepResponse};
use crate::store::Store;

mod client_blobs;
#[cfg(test)]
mod terminal_gate_evidence;
mod message_bodies;
mod client_adapters;
mod client_presence;
mod client_v0;
mod custom;
use crate::delivery_presence;
pub(crate) mod agent_harness;
#[cfg(test)]
mod harness_health_controls;
#[cfg(test)]
mod harness_health_output_controls;
mod delivery_probes;
mod github_watch;
mod harness_events;
mod mailbox;
mod mail_backlog;
pub(crate) mod mission_eligibility_doctor;
mod read_deadline;
mod response_body;
mod owned_sets;
mod published_attention;
pub use published_attention::start_attention_list;
#[cfg(test)]
pub(crate) use published_attention::{published_attention_rows, refresh_attention_list};
mod directive_notes;
mod request_latency;
mod client_observations;
mod terminal_view;
mod work_response;
mod published_lists;
#[cfg(target_os = "linux")]
mod tile_endpoint;
#[cfg(target_os = "linux")]
pub use tile_endpoint::{
    SOCKET as TILE_ENDPOINT_SOCKET, spawn as spawn_tile_endpoints,
    spawn_from as spawn_tile_endpoints_from,
};
pub use published_lists::{start_published_lists, wait_published_lists_at_least};
mod changes;

#[cfg(test)]
mod legacy_handler_guard_tests;
// Route families: each file holds the handlers of one group of routes; the router below names
// them as family::handler.
mod agents;
mod client_reads;
mod fleet;
mod launches;
mod messages;
mod operations;
mod records;
mod replication;
mod runs;
mod sessions;
mod work;
// Other api code calls these handlers directly.
use launches::{
    approve_planning_session, cancel_planning_session, mission, preview_named_planning_candidate,
    revise_planning_session, start_planning_session,
};
use messages::post_message_claim;
use operations::{get_slo, get_slo_history};
use records::post_review;
use sessions::clear_context;
use work::retry_work;
// The tests call these handlers directly.
#[cfg(test)]
use client_reads::{
    client_agents, client_agents_detail, client_history, client_history_detail,
    client_launches_detail, client_work,
};
#[cfg(test)]
use messages::list_messages;
#[cfg(test)]
use operations::{doctor, events};
#[cfg(test)]
use records::post_claim;
#[cfg(test)]
use replication::{replication_export, replication_receive, replication_wake};

pub(crate) use client_v0::raw_terminal::splice as raw_terminal_splice;

/// The owner side of a peer raw terminal route: the plain byte splice, with no lease or control
/// channel. The daemon passes it to `peer::run_worker` as its `RawSplice`.
pub fn raw_terminal_owner_splice(
    socket: axum::extract::ws::WebSocket,
    transport: tokio::net::UnixStream,
) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(raw_terminal_splice(socket, transport, None, None, None, None, None))
}

pub(crate) use crate::delivery_presence::claude_channel_attached;

#[derive(Clone)]
pub struct AppState {
    /// Configured owning person for trusted-local assistant grant administration.
    pub owning_person: Option<String>,
    pub store: Arc<Store>,
    pub notify: Arc<Notify>,
    pub event_notify: watch::Sender<u64>,
    pub node: String,
    pub state_dir: std::path::PathBuf,
    pub pty_root: std::path::PathBuf,
    pub pty_binary: std::path::PathBuf,
    pub fleet_id: Option<String>,
    pub configured_peers: Vec<String>,
    pub client_relay: Option<crate::peer::ClientRelay>,
    pub native_session_home: Option<std::path::PathBuf>,
    pub planner_default: PlannerSpec,
}

const CLIENT_API_VERSION: &str = "st3.client.v0";
const CLIENT_PROJECTION_VERSION: &str = "client-projection.v0";
const CLIENT_DEFAULT_PAGE_ITEMS: usize = 50;
const CLIENT_MAX_PAGE_ITEMS: usize = 200;
const CLIENT_MAX_RESPONSE_BYTES: usize = 1_048_576;
// Keep complete result sets briefly so fleet writes cannot reorder or invalidate a traversal.
// Cursors expire after this bounded window or if the daemon restarts/evicts their snapshot.
const CLIENT_PAGE_TTL_MS: u128 = 300_000;
const CLIENT_PAGE_CACHE_CAPACITY: usize = 32;

struct CachedClientPage {
    snapshot_id: String,
    collection: String,
    items_digest: String,
    items: Arc<Vec<Value>>,
    expires_at_unix_ms: u128,
}

static CLIENT_PAGE_CACHE: OnceLock<Mutex<VecDeque<CachedClientPage>>> = OnceLock::new();

fn client_page_cache() -> &'static Mutex<VecDeque<CachedClientPage>> {
    CLIENT_PAGE_CACHE.get_or_init(|| Mutex::new(VecDeque::new()))
}

#[derive(Clone, Copy)]
enum ClientTransportBoundary {
    Unix,
    FabricLoopback,
}

impl ClientTransportBoundary {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unix => "unix",
            Self::FabricLoopback => "fabric-loopback",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ClientSnapshot {
    id: String,
    host_id: String,
    store_index: u64,
    projection_version: String,
    created_at: String,
    /// For rows a background refresher folded: when it published them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    published_at: Option<String>,
}

/// A client page and the snapshot it was read in, which the envelope names.
type ClientPageResponse = (Extension<ClientSnapshot>, Json<ClientResourcePage>);

#[derive(Clone, Debug, Default, Deserialize)]
struct ClientListQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    #[serde(default)]
    history: bool,
    person: Option<String>,
    actor: Option<String>,
    owner_run: Option<String>,
    status: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    native_only: bool,
    /// Agents and current work: wait briefly for a roster, or a work list, at least as new as this
    /// request, rather than answering at once from the newest published one. Agents skip the wait
    /// when the latest roster refresh failed and none is in flight.
    #[serde(default)]
    fresh: bool,
    /// Agents: wait for publication of this host-local mutation store_index, not the head.
    min_cut: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ClientPageCursor {
    snapshot: ClientSnapshot,
    collection: String,
    offset: usize,
    limit: usize,
    history: bool,
    person: Option<String>,
    actor: Option<String>,
    owner_run: Option<String>,
    status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(default)]
    native_only: bool,
    items_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before_index: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after_key: Option<(u128, String)>,
    expires_at_unix_ms: u128,
}

fn signal_changed(state: &AppState) {
    crate::reconcile_telemetry::record_wake(crate::reconcile_telemetry::WakeCause::Api, None);
    state.notify.notify_one();
    signal_visible_change(state);
}

/// The reconciler reads a message only when it is a work wake, tagged `st3-work:`.
pub(crate) fn is_work_wake(tags: &[String]) -> bool {
    tags.iter().any(|tag| tag.starts_with("st3-work:"))
}

/// A message write wakes the reconciler only for a work wake, since the reconciler reads no other
/// message. Agents' and people's conversations and the delivery probes write five claims for each
/// message, and each woke a reconcile pass on every member. Clients, mailboxes and peers still
/// hear of every message.
pub(crate) fn signal_message_changed(state: &AppState, kind: &str, work_wake: bool) {
    if work_wake {
        signal_claim_changed(state, kind);
    } else {
        signal_visible_change(state);
    }
}

/// [`signal_changed`] for a claim, counting the wake under the claim's kind.
fn signal_claim_changed(state: &AppState, kind: &str) {
    crate::reconcile_telemetry::record_wake(crate::reconcile_telemetry::WakeCause::Api, Some(kind));
    state.notify.notify_one();
    signal_visible_change(state);
}

// Usage samples and lease renewals are durable and visible, but neither can
// advance a mission on its own. Terminal child state, claim expiry deadlines,
// and harness readiness still wake the reconciler through their own paths.
// Local telemetry only wakes clients following this node. Harness observations also
// affect work eligibility and acknowledgement deadlines; their transitions wake reconciliation.
fn signal_local_change(state: &AppState) {
    state
        .event_notify
        .send_modify(|generation| *generation = generation.saturating_add(1));
}

fn signal_visible_change(state: &AppState) {
    state
        .event_notify
        .send_modify(|generation| *generation = generation.saturating_add(1));
    let _ = fs::write(
        state.state_dir.join("replication.wake"),
        format!("{}\n", uuid::Uuid::now_v7()),
    );
}

#[cfg(test)]
mod storage_contention_response_tests;

#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: String,
    message: String,
    details: Box<serde_json::Map<String, Value>>,
}

impl ApiError {
    fn bad(error: St3Error) -> Self {
        let status = match error.code {
            "recipient-retired"
            | "lifecycle-unavailable"
            | "retirement-upgrade-required"
            | "stale-subject"
            | "owned-set-refused"
            | "set-managed-subject"
            | "missing-subject-token"
            | "idempotency-key-expired"
            | "stale-document-token"
            | "stale-incarnation"
            | "stale-launch-preview"
            | "fleet-leaving"
            | "glass-deleted"
            | "glass-limit" => StatusCode::CONFLICT,
            "launch-review-not-authorized"
            | "wrong-message-recipient"
            | "lane-approval-denied"
            | "glass-owner-forbidden" => StatusCode::FORBIDDEN,
            "lane-not-found" | "not-found" => StatusCode::NOT_FOUND,
            "database-busy" | "database-locked" => StatusCode::SERVICE_UNAVAILABLE,
            "read-deadline" => StatusCode::GATEWAY_TIMEOUT,
            "internal" => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        };
        let mut details = error.details;
        details.retain(|key, _| !matches!(key.as_str(), "projection_stage" | "projection_claim_id"
            | "projection_subject" | "projection_operation_id" | "projection_context_truncated"
            | "projection_frontier_unknown"));
        Self {
            status,
            code: error.code.into(),
            message: error.message,
            details: Box::new(details),
        }
    }

    fn internal(error: impl std::fmt::Display + 'static) -> Self {
        let typed = &error as &dyn std::any::Any;
        if let Some(error) = typed.downcast_ref::<read_deadline::WorkError>()
            && let Some(error) = read_deadline::error(error) {
            return error;
        }
        if smallclaims::read_budget::check().is_err() {
            return Self::bad(St3Error::new("read-deadline", "the read exceeded its deadline or was cancelled"));
        }
        let store_error = typed.downcast_ref::<St3Error>().or_else(|| {
            typed
                .downcast_ref::<anyhow::Error>()
                .and_then(|error| error.downcast_ref::<St3Error>())
        });
        if let Some(error) = store_error {
            return Self::bad(St3Error {
                code: error.code,
                message: error.message.clone(),
                details: error.details.clone(),
            });
        }
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal".into(),
            message: error.to_string(),
            details: Box::default(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not-found".into(),
            message: message.into(),
            details: Box::default(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body =
            json!({ "code": self.code, "message": self.message, "details": self.details });
        if let Some(applied) = body["details"].get("applied").cloned() {
            body["applied"] = applied;
        }
        (self.status, Json(body)).into_response()
    }
}

/// Build routes only. The owner starts background publications with
/// [`start_published_lists`]; a bare router refuses unpublished current runtime reads.
pub fn router(state: AppState) -> Router {
    crate::relay_trace::init();
    delivery_presence::start();
    router_for_transport(state, ClientTransportBoundary::Unix)
}

/// Already-admitted snapshot/receipt transport fixtures only. There is no CLI/API switch
/// into this router: real native custody controls and the daemon use `router`.
#[cfg(feature = "test-support")]
pub(crate) fn admitted_mailbox_protocol_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/mailbox/bind", post(mailbox::bind_admitted_fixture))
        .route("/v1/mailbox", get(mailbox::subscribe_admitted_fixture))
        .fallback_service(router(state.clone()))
        .with_state(state)
}

/// Synthetic private-store transport controls; never physical native admission.
#[cfg(feature = "test-support")]
pub(crate) fn synthetic_mailbox_protocol_router(state: AppState, subject: &str) -> Router {
    assert!(subject.starts_with("agent/example/"));
    admitted_mailbox_protocol_router(state).layer(Extension(NativeDeliveryPeer {
        agent: subject.into(),
        transport: "omp-channel",
        pid: std::process::id(),
        archives_inbox: false,
    }))
}

/// Build the loopback-only client gateway. Unlike the local Unix boundary, every ordinary
/// client request on this router requires a paired bearer credential.
pub fn fabric_router(state: AppState) -> Router {
    router_for_transport(state, ClientTransportBoundary::FabricLoopback)
}

fn router_for_transport(state: AppState, transport: ClientTransportBoundary) -> Router {
    crate::slo::install();
    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/harnesses", get(operations::available_harnesses))
        .route("/v1/client/capabilities", get(client_capabilities))
        .route("/v1/client/notes", get(directive_notes::read))
        .route("/v1/notes", axum::routing::put(directive_notes::write))
        .route("/v1/client/sets", get(owned_sets::list))
        .route("/v1/client/sets/{*id}", get(owned_sets::get))
        .route("/v1/client/arrangements", get(client_v0::arrangements::list))
        .route(
            "/v1/client/arrangements/{person_name}/{uuid}",
            get(client_v0::arrangements::get),
        )
        .route("/v1/client/glasses", get(client_v0::glasses_list))
        .route(
            "/v1/client/glasses/{id}",
            get(client_v0::glass_get)
                .put(client_v0::glass_put)
                .delete(client_v0::glass_delete),
        )
        .route(
            "/v1/client/request-latency",
            get(client_v0::request_latency),
        )
        .route("/v1/client/observations", post(client_observations::report)
            .layer(DefaultBodyLimit::max(client_observations::MAX_BODY_BYTES)))
        .route("/v1/client/documents/content", get(client_v0::document_get))
        .route("/v1/client/usage", get(client_v0::usage_period))
        .route("/v1/client/slo", get(client_v0::slo_list))
        .route("/v1/client/slo/history", get(client_v0::slo_history))
        .route("/v1/client/mail-backlog", get(mail_backlog::get))
        .route("/v1/client/clients", get(client_v0::clients_list))
        .route(
            "/v1/client/subject-definition",
            get(client_v0::subject_definition),
        )
        .route("/v1/client/publication-definition", get(client_v0::publication_definition))
        .route("/v1/client/now", get(client_v0::now))
        .route("/v1/client/machines", get(client_v0::machines))
        .route("/v1/client/hosts/{*id}", get(client_v0::host_repositories))
        .route("/v1/client/devices", get(client_v0::devices))
        .route("/v1/client/attention", get(client_reads::client_attention))
        .route("/v1/client/attention/{*id}", get(client_reads::client_attention_detail))
        // The same rows under the name a person reads.
        .route("/v1/client/alerts", get(client_reads::client_attention))
        .route("/v1/client/alerts/{*id}", get(client_reads::client_attention_detail))
        .route("/v1/client/messages", get(client_reads::client_messages))
        .route("/v1/client/messages/{*id}", get(client_reads::client_messages_detail))
        .route("/v1/client/message-bodies/{*id}", get(message_bodies::client_body))
        .route("/v1/client/launches", get(client_reads::client_launches))
        .route(
            "/v1/client/launches/{id}/variants",
            get(client_reads::client_launch_variants),
        )
        .route(
            "/v1/client/launches/{id}/variants/{variant}",
            get(client_reads::client_launch_variant_detail),
        )
        .route(
            "/v1/client/launches/{id}/decisions",
            get(client_reads::client_launch_decisions),
        )
        .route(
            "/v1/client/launches/{id}/decisions/{decision}",
            get(client_reads::client_launch_decision_detail),
        )
        .route(
            "/v1/client/launches/{id}/approvals",
            get(client_reads::client_launch_approvals),
        )
        .route(
            "/v1/client/launches/{id}/approvals/{approval}",
            get(client_reads::client_launch_approval_detail),
        )
        .route("/v1/client/launches/{id}", get(client_reads::client_launches_detail))
        .route("/v1/client/work", get(client_reads::client_work))
        .route("/v1/client/changes", get(changes::list))
        .route("/v1/client/changes/sync", post(changes::sync))
        .route("/v1/client/change", get(changes::show))
        .route("/v1/client/work/{*id}", get(client_reads::client_work_detail))
        .route("/v1/client/agents", get(client_reads::client_agents))
        .route("/v1/client/resources", get(client_v0::resources::list))
        .route("/v1/client/agents/{*id}", get(client_reads::client_agents_detail))
        .route(
            "/v1/client/agent-workspaces/{*id}",
            get(client_v0::agent_workspace),
        )
        .route(
            "/v1/client/agent-declarations/{*id}",
            get(client_v0::agent_declaration),
        )
        .route("/v1/client/agent-queues/{*id}", get(client_v0::agent_queue))
        .route("/v1/client/status-history/{*id}", get(client_v0::status_history))
        .route("/v1/client/lanes", get(client_v0::lanes))
        .route("/v1/client/lanes/{*id}", get(client_v0::lane_detail))
        .route("/v1/client/history", get(client_reads::client_history))
        .route("/v1/client/history/{*id}", get(client_reads::client_history_detail))
        .route(
            "/v1/client/conversations/search",
            get(client_v0::search::search),
        )
        .route(
            "/v1/client/conversations/{id}/content/{reference}/chunk",
            get(client_v0::conversation_blocks::chunk),
        )
        .route("/v1/client/sessions", get(client_reads::client_sessions))
        .route("/v1/client/sessions/{*id}", get(client_reads::client_sessions_detail))
        .route(
            "/v1/client/conversations/{id}/changes",
            get(client_v0::conversation_changes),
        )
        .route(
            "/v1/client/conversations/{id}/stream",
            get(client_v0::conversation_stream),
        )
        .route("/v1/client/missions", get(client_v0::missions))
        .route("/v1/client/missions-tree", get(client_v0::missions_tree))
        .route("/v1/client/missions/{*id}", get(client_v0::mission_detail))
        .route("/v1/client/runtimes", get(client_v0::runtimes))
        .route("/v1/client/runtimes/{*id}", get(client_v0::runtime_detail))
        .route("/v1/client/observers", get(client_v0::observers))
        .route(
            "/v1/client/observers/{*id}",
            get(client_v0::observer_detail),
        )
        .route("/v1/client/subscriptions", get(client_v0::subscriptions))
        .route(
            "/v1/client/subscriptions/{*id}",
            get(client_v0::subscription_detail),
        )
        .route("/v1/client/terminals", get(client_v0::terminals))
        .route("/v1/client/operations", get(client_v0::operations))
        .route(
            "/v1/client/operations/{*id}",
            get(client_v0::operation_detail),
        )
        .route("/v1/client/events", get(client_v0::events))
        .route(
            "/v1/client/collections/stream",
            get(client_v0::collection_stream),
        )
        .route("/v1/client/actions", post(client_v0::action))
        .route("/v1/client/assistant/propose", post(client_v0::assistant::propose))
        .route("/v1/client/assistant/attention-content", get(client_v0::assistant::read_projection))
        .route("/v1/client/assistant/attention-rich", get(client_v0::assistant::read_projection))
        .route("/v1/client/assistant/missions-rich", get(client_v0::assistant::read_projection))
        .route("/v1/client/assistant/agents-rich", get(client_v0::assistant::read_projection))
        .route(
            "/v1/client/blobs",
            post(client_blobs::upload).layer(DefaultBodyLimit::max(client_blobs::UPLOAD_BODY_LIMIT)),
        )
        .route("/v1/client/blobs/{id}", get(client_blobs::get))
        .route("/v1/client/blobs/{id}/chunk", get(client_blobs::chunk))
        .route("/v1/client/adapter/import", post(client_adapters::import_message))
        .route("/v1/client/adapter/deliveries", get(client_adapters::deliveries))
        .route("/v1/client/pairings", post(client_v0::pairing_begin))
        .route("/v1/client/assistant/grants/{id}", get(client_v0::assistant::inspect))
        .route("/v1/client/assistant/grants/{id}/revoke", post(client_v0::assistant::revoke))
        .route("/v1/client/assistant/grants/{id}/read-trust", post(client_v0::assistant::pin_read_trust))
        .route("/v1/client/assistant/grants/assistant-grant/{id}", get(client_v0::assistant::inspect))
        .route("/v1/client/assistant/grants/assistant-grant/{id}/revoke", post(client_v0::assistant::revoke))
        .route("/v1/client/assistant/grants/assistant-grant/{id}/read-trust", post(client_v0::assistant::pin_read_trust))
        .route("/v1/client/assistant/grants/assistant-grant/{id}/policy", post(client_v0::assistant::update_policy))
        .route("/v1/client/assistant/summary", get(client_v0::assistant::read_projection))
        .route("/v1/client/assistant/attention", get(client_v0::assistant::read_projection))
        .route("/v1/client/assistant/missions", get(client_v0::assistant::read_projection))
        .route("/v1/client/assistant/agents", get(client_v0::assistant::read_projection))
        .route("/v1/client/assistant/conversation-content", get(client_v0::assistant::read_projection))
        .route("/v1/client/assistant/terminal-content", get(client_v0::assistant::read_projection))
        .route("/v1/client/assistant/grants/{id}/policy", post(client_v0::assistant::update_policy))
        .route(
            "/v1/client/pairings/{id}/complete",
            post(client_v0::pairing_complete),
        )
        .route(
            "/v1/client/terminals/{id}/screen",
            get(client_v0::terminal_screen),
        )
        .route(
            "/v1/client/terminals/{id}/stream",
            get(client_v0::terminal_stream),
        )
        .route(
            "/v1/client/terminals/{id}/raw-attachments",
            post(client_v0::raw_terminal::attachment),
        )
        .route(
            "/v1/client/terminals/{id}/raw-stream",
            get(client_v0::raw_terminal::stream),
        )
        .route("/v1/schema", get(schema))
        .route(
            "/v1/schema/registrations",
            get(custom::registrations).post(custom::register),
        )
        .route("/v1/custom/reply", post(custom::reply))
        .route("/v1/custom/basis", get(custom::basis))
        .route("/v1/client/custom-subjects", get(custom::list))
        .route("/v1/client/custom-subjects/{*id}", get(custom::read))
        .route("/v1/intent/mission", post(launches::mission))
        .route("/v1/gate-checks", post(runs::start_gate_check))
        .route("/v1/gate-checks/{id}", get(runs::read_gate_check))
        .route("/v1/sets/preview", post(owned_sets::preview))
        .route("/v1/sets/apply", post(owned_sets::apply))
        .route("/v1/intent/apply", post(launches::apply_with_bound))
        .route("/v1/agents/rename", post(agents::rename_agent))
        .route("/v1/agents/restart", post(agents::restart_agent))
        .route("/v1/agents/rollout", post(agents::rollout_agent))
        .route("/v1/agents/start", post(agents::start_mission_seat))
        .route("/v1/agents/source-offline", post(agents::override_placement_source))
        .route("/v1/agents/suspend", post(agents::suspend_agent))
        .route("/v1/agents/resume", post(agents::resume_agent))
        .route("/v1/internal/seat-snapshot", post(agents::seat_snapshot_chunk))
        .route("/v1/agents/native-session", post(agents::report_native_session))
        .route("/v1/missions/{id}", get(launches::get_mission))
        .route("/v1/missions/{id}/retire", post(launches::retire_mission))
        .route("/v1/launches/{id}", get(launches::get_planning_session))
        .route("/v1/launches/{id}/submit", post(launches::submit_planning_candidate))
        .route(
            "/v1/launches/{id}/variants/{variant}/submit",
            post(launches::submit_named_planning_candidate),
        )
        .route(
            "/v1/launches/{id}/preview",
            post(launches::preview_planning_candidate),
        )
        .route(
            "/v1/launches/{id}/variants/{variant}/preview",
            post(launches::preview_named_planning_candidate),
        )
        .route(
            "/v1/launches/{id}/variants/{left}/compare/{right}",
            get(launches::compare_planning_variants),
        )
        .route(
            "/v1/launches/{id}/variants/{variant}/propose",
            post(launches::propose_planning_variant),
        )
        .route("/v1/launches/{id}/approve", post(launches::approve_planning_session))
        .route(
            "/v1/launches/{id}/approve-and-launch",
            post(launches::approve_and_start_launch),
        )
        .route("/v1/launches/{id}/start", post(launches::start_approved_launch))
        .route("/v1/launches/{id}/decisions", post(launches::request_launch_decision))
        .route(
            "/v1/launches/{id}/decisions/{decision}/answer",
            post(launches::answer_launch_decision),
        )
        .route("/v1/launches", post(launches::start_planning_session))
        .route("/v1/launches/{id}/revise", post(launches::revise_planning_session))
        .route("/v1/launches/{id}/cancel", post(launches::cancel_planning_session))
        .route("/v1/documents", get(records::list_documents).post(records::put_document))
        .route("/v1/rules", get(records::list_rules))
        .route("/v1/rules/audit", get(records::list_rule_audits))
        .route("/v1/rules/set", post(records::set_rule))
        .route("/v1/documents/content", get(records::get_document))
        .route("/v1/diagnostics/harness", post(operations::post_harness_diagnostic))
        .route(
            "/v1/delivery/hold",
            get(get_delivery_hold).post(post_delivery_hold),
        )
        .route("/v1/claims", get(records::list_claims).post(records::post_claim))
        .route("/v1/usage", get(operations::get_usage))
        .route("/v1/slo", get(operations::get_slo))
        .route("/v1/slo/history", get(operations::get_slo_history))
        .route("/v1/usage/messages", get(operations::get_coordination_counts))
        .route("/v1/claims/by-id/{id}", get(records::get_claim))
        .route("/v1/reviews", get(records::list_reviews))
        .route("/v1/reviews/{*subject}", post(records::post_review))
        .route("/v1/work/delegation", post(set_delegation_policy))
        .route("/v1/attention", get(records::list_attention).post(records::request_attention))
        .route("/v1/attention/resolve/{*subject}", post(records::resolve_attention))
        .route("/v1/subscription-requests", get(messages::list_subscription_requests))
        .route(
            "/v1/subscription-requests/{decision}/{request}",
            post(messages::decide_subscription_request),
        )
        .route(
            "/v1/attention/withdraw/{*subject}",
            post(records::withdraw_attention),
        )
        .route("/v1/messages", get(messages::list_messages).post(messages::send_message))
        .route("/v1/messages/page", get(messages::list_messages_page))
        .route("/v1/messages/cleanup", post(mail_backlog::cleanup))
        .route("/v1/mailbox", get(mailbox::subscribe))
        .route("/v1/harness-events", post(harness_events::publish))
        .route("/v1/harness-prompts/state", get(harness_events::prompt_state))
        .route("/v1/harness-prompts/answer", get(harness_events::prompt_answer))
        .route("/v1/mailbox/bind", post(mailbox::bind))
        .route("/v1/mailbox/attachment", get(mailbox::attachment))
        .route("/v1/mailbox/receipts", post(mailbox::receipt))
        .route("/v1/messages/{message_id}/claims", post(messages::post_message_claim))
        .route("/v1/messages/read/{*subject}", get(messages::read_message))
        .route("/v1/messages/body/{*subject}", get(message_bodies::body))
        .route("/v1/messages/delivery/{*subject}", get(messages::message_delivery))
        .route("/v1/messages/by-key", get(messages::message_by_key))
        .route("/v1/status", get(operations::status))
        .route("/v1/desired/{*subject}", get(operations::get_desired))
        .route("/v1/events", get(operations::events))
        .route("/v1/events/page", get(events_page))
        .route("/v1/doctor", get(operations::doctor))
        .route("/v1/repair", get(operations::operational_repair_plan))
        .route("/v1/repair/apply", post(operations::apply_operational_repair))
        .route("/v1/backup", get(operations::backup_export))
        .route("/v1/replication/status", get(replication::replication_status))
        .route("/v1/replication/records", get(replication::replication_records))
        .route("/v1/replication/records/{*record}", get(replication::replication_record))
        .route("/v1/replication/repair", post(replication::repair_replication_record))
        .route("/v1/checkpoint/plan", post(replication::checkpoint_plan))
        .route("/v1/checkpoint/status", get(replication::checkpoint_status))
        .route("/v1/checkpoint/excuse", post(replication::checkpoint_excuse))
        .route("/v1/checkpoint/resume", post(replication::checkpoint_resume))
        .route(
            "/v1/internal/replication/export",
            post(replication::replication_export).layer(DefaultBodyLimit::max(crate::peer::MAX_EXCHANGE_BYTES)),
        )
        .route(
            "/v1/internal/replication/receive",
            post(replication::replication_receive).layer(DefaultBodyLimit::max(crate::peer::MAX_EXCHANGE_BYTES)),
        )
        .route(
            "/v1/internal/replication/peer-failure",
            post(replication::replication_peer_failure),
        )
        .route(
            "/v1/internal/replication/worker-status",
            post(replication_worker_status),
        )
        .route(
            "/v1/internal/replication/checkpoint",
            post(replication::replication_checkpoint_manifest),
        )
        .route(
            "/v1/internal/replication/checkpoint-need",
            post(replication::replication_checkpoint_need),
        )
        .route(
            "/v1/internal/replication/checkpoint-adopt",
            post(replication::replication_checkpoint_adopt)
                .layer(DefaultBodyLimit::max(crate::peer::MAX_MANIFEST_BYTES)),
        )
        .route("/v1/internal/fleet/membership", get(fleet::fleet_membership_view))
        .route(
            "/v1/internal/fleet/admission-residue",
            get(fleet::fleet_admission_residue),
        )
        .route("/v1/internal/fleet/status", get(fleet::fleet_status))
        .route(
            "/v1/internal/fleet/invites",
            get(fleet::fleet_invite_list).post(fleet::fleet_invite_create),
        )
        .route(
            "/v1/internal/fleet/invites/revoke",
            post(fleet::fleet_invite_revoke),
        )
        .route("/v1/internal/fleet/redeem", post(fleet::fleet_redeem))
        .route("/v1/internal/fleet/remove", post(fleet::fleet_remove))
        .route("/v1/internal/fleet/leave/begin", post(fleet::fleet_leave_begin))
        .route("/v1/internal/fleet/leave/cancel", post(fleet::fleet_leave_cancel))
        .route("/v1/internal/fleet/leave/claim", post(fleet::fleet_leave_claim))
        .route(
            "/v1/internal/fleet/endpoints",
            post(fleet::fleet_publish_endpoints),
        )
        .route("/v1/internal/replication-wake", post(replication::replication_wake))
        .route(
            crate::peer::CLIENT_READ_FORWARD_PATH,
            post(forward_client_read).layer(DefaultBodyLimit::max(16_384)),
        )
        .route("/v1/evals", post(runs::start_eval))
        .route("/v1/evals/{*run}", get(runs::get_eval))
        .route("/v1/mission-runs", get(runs::list_mission_runs))
        .route("/v1/mission-runs/tree", get(runs::list_mission_run_tree))
        .route("/v1/mission-overview", get(runs::mission_overview))
        .route("/v1/outcome-history", get(runs::outcome_history))
        .route("/v1/performance", get(runs::performance_report))
        .route(
            "/v1/mission-runs/{run}/generations",
            get(runs::list_run_generations),
        )
        .route(
            "/v1/mission-runs/{run}/revision-proposal",
            get(runs::get_run_revision_proposal),
        )
        .route("/v1/mission-runs/{run}/revision", post(runs::revise_mission_run))
        .route(
            "/v1/mission-runs/{run}/outcome",
            post(runs::set_mission_run_outcome),
        )
        .route(
            "/v1/mission-runs/{run}/report-to",
            post(runs::set_mission_run_report),
        )
        .route("/v1/mission-runs/{run}", get(runs::get_mission_run))
        .route("/v1/run-generations/{generation}", get(runs::get_run_generation))
        .route(
            "/v1/revision-proposals/{proposal}",
            get(get_revision_proposal),
        )
        .route(
            "/v1/revision-proposals/{proposal}/approve",
            post(approve_revision_proposal),
        )
        .route(
            "/v1/revision-proposals/{proposal}/cancel",
            post(cancel_revision_proposal),
        )
        .route("/v1/work/ask", post(work::ask_person))
        .route("/v1/work/start", post(work::start_work))
        .route("/v1/work/handoff/{*subject}", post(work::handoff_work))
        .route("/v1/work/acknowledge/{*subject}", post(work::acknowledge_work))
        .route("/v1/github/watch", post(github_watch::watch))
        .route("/v1/github/unwatch", post(github_watch::unwatch))
        .route("/v1/github/watches", get(github_watch::watches))
        .route("/v1/github/comment", post(github_watch::comment))
        .route("/v1/github/own", post(github_watch::own))
        .route("/v1/work/done", post(work::done_person_step))
        .route("/v1/work/cancel-ask", post(work::cancel_person_ask))
        .route("/v1/work", get(work::list_work))
        .route("/v1/work-items/{*subject}", get(work::get_work))
        .route("/v1/work/mission/{*subject}", post(work::publish_work_mission))
        .route("/v1/work/wake/{*subject}", post(work::wake_work))
        .route("/v1/work/retry/{*subject}", post(work::retry_work))
        .route("/v1/work/extend/{*subject}", post(work::extend_work))
        .route("/v1/work/{action}/{*subject}", post(work::post_work_action))
        .route("/v1/gate-results", post(work::post_gate_result))
        .route("/v1/agent-queue-moves", post(work::move_agent_queue))
        .route("/v1/sekrets/node", get(records::sekrets_node))
        .route("/v1/sekrets/attest", post(records::sekrets_attest))
        .route("/v1/lanes", get(records::list_lanes))
        .route("/v1/lanes/{*lane}", get(records::get_lane))
        .route("/v1/lane-changes", post(records::change_lane))
        .route("/v1/sessions", get(sessions::list_sessions))
        .route("/v1/sessions/{subject}/context/clear", post(sessions::clear_context))
        .route("/v1/sessions/{subject}/signal", post(sessions::signal_session))
        .route("/v1/sessions/input/{*subject}", post(sessions::input_session))
        .route("/v1/sessions/logs/{*subject}", get(sessions::logs_session))
        .route("/v1/sessions/screen/{*subject}", get(sessions::screen_session))
        .route("/v1/sessions/attach/{*subject}", post(sessions::attach_session))
        .route("/v1/sessions/{subject}/attach", post(sessions::attach_session))
        .route(
            "/v1/sessions/local-terminal/{*subject}",
            get(sessions::local_terminal),
        )
        .route("/v1/sessions/terminal/{*subject}", get(sessions::terminal_session))
        .route(
            "/v1/hosts/{host}/agent-workspace",
            get(host_agent_workspace),
        );
    app.layer(from_fn_with_state(state.clone(), refuse_while_leaving))
        .layer(from_fn_with_state(
            (state.clone(), transport),
            response_envelope,
        ))
        .with_state(state)
}

async fn schema() -> Json<Value> {
    let registry = st3_schema::registry();
    Json(json!({
        "schema": registry.name,
        "digest": registry.digest(),
        "subjects": registry.subjects,
        "resources": registry.resources,
        "claims": registry.claims,
    }))
}

async fn response_envelope(
    State(state): State<(AppState, ClientTransportBoundary)>,
    request: Request<Body>,
    next: Next,
) -> Response {
    read_deadline::envelope(state, request, next).await
}

async fn response_envelope_unbounded(
    State((state, transport)): State<(AppState, ClientTransportBoundary)>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    // Anonymous pairing discovery is public, static compatibility metadata. Bypass
    // authentication, snapshots, presence and the normal envelope/activity counter entirely.
    if matches!(transport, ClientTransportBoundary::FabricLoopback)
        && request.method() == axum::http::Method::GET
        && request.uri().path() == "/v1/client/capabilities"
        && !request.headers().contains_key(axum::http::header::AUTHORIZATION)
    {
        return Json(json!({
            "api_version": CLIENT_API_VERSION,
            "capabilities": [{"id":"device-key-proofs", "version":1, "state":"granted"}],
        }))
        .into_response();
    }
    let started = Instant::now();
    let client_request = request.uri().path().starts_with("/v1/client/");
    let request_id = if client_request {
        crate::relay_trace::current().map_or_else(
            || format!("request/{}", new_request_id()),
            |trace| trace.id().to_owned(),
        )
    } else {
        new_request_id()
    };
    let request_path = request.uri().path().to_owned();
    let request_query = request.uri().query().map(str::to_owned);
    let long_poll = request_query.as_deref().is_some_and(asks_to_wait);
    let served_remote = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let request_route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|route| route.as_str().to_owned())
        .unwrap_or_else(|| "/unmatched".to_owned());
    let caller = request
        .extensions()
        .get::<crate::profile::Caller>()
        .map(|caller| caller.0.clone())
        .unwrap_or_else(|| "(tcp)".into());
    let request_method = request.method().clone();
    let client_class = crate::otel_client_class::ClientClass::from_header(
        request
            .headers()
            .get(client_presence::CLIENT_HEADER)
            .and_then(|value| value.to_str().ok()),
    );
    // The OTel server span exists only while trace export is on; the disabled path
    // constructs nothing (st2's O11Y-R02 gate).
    let otel = request_trace(
        &request_method,
        &request_route,
        client_class,
        request.headers(),
        &request_id,
    );
    if let Some(server) = otel.as_ref()
        && request.headers().contains_key(axum::http::header::UPGRADE)
    {
        use opentelemetry::trace::TraceContextExt as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        request.extensions_mut().insert(crate::otel::UpgradeContext(
            Some(server.context().span().span_context().clone()),
        ));
    }
    let profile = crate::profile::Op::start(
        format!("{} {request_route}", request.method()),
        Some(caller.clone()),
    );
    let origin = smallclaims::sqlite::telemetry::route_origin(request_method.as_str(), &request_route);
    let observation_report = request_route == "/v1/client/observations";
    // These point readers admit snapshot metadata inside their pinned read before
    // formatting it. Authentication still runs here; their response extension
    // supplies the envelope snapshot. All other routes keep admission snapshots.
    let defer_detail_snapshot = (request_method == axum::http::Method::GET
        || request_method == axum::http::Method::HEAD)
        && matches!(
            request_route.as_str(),
            "/v1/client/observers/{*id}" | "/v1/client/subscriptions/{*id}"
        );
    let fabric_boundary_error = (matches!(transport, ClientTransportBoundary::FabricLoopback)
        && !client_request
        && request.uri().path() != "/v1/health")
        .then(client_v0::fabric_boundary_forbidden);
    let cursor_snapshot = client_request.then(|| {
        request
            .uri()
            .query()
            .and_then(|query| {
                query.split('&').find_map(|pair| {
                    let (name, value) = pair.split_once('=')?;
                    (name == "cursor").then(|| {
                        urlencoding::decode(value)
                            .map(|value| value.into_owned())
                            .unwrap_or_else(|_| value.to_owned())
                    })
                })
            })
            .and_then(|cursor| decode_client_cursor_snapshot(&cursor).ok())
    });
    // Authentication can scan pairing claims, and creating a snapshot reads the store. Both
    // must leave the async acceptor free to admit independent requests when SQLite is busy.
    // Bearers on native Unix routes must not evade the assistant route boundary.
    let (client_authentication, client_snapshot) = if client_request
        || request.headers().contains_key(axum::http::header::AUTHORIZATION) {
        let mut auth_request = Request::builder()
            .method(request.method().clone())
            .uri(request.uri().clone())
            .body(Body::empty())
            .expect("the incoming request has a valid method and URI");
        *auth_request.headers_mut() = request.headers().clone();
        *auth_request.extensions_mut() = request.extensions().clone();
        let auth_state = state.clone();
        let transport = transport.as_str();
        let auth_profile = profile.clone();
        let auth_origin = origin.clone();
        let admission_queue = profile.as_ref().map(|op| op.wall_span("admission/queue"));
        let mut diagnostic_queue =
            crate::relay_trace::span(crate::relay_trace::Phase::AdmissionQueue);
        let admission_enqueued = Instant::now();
        let server_trace = otel.clone();
        let admitted = crate::api::read_deadline::spawn_blocking(move || {
            diagnostic_queue.finish(crate::relay_trace::Outcome::Completed);
            drop(admission_queue);
            let queue_ms = admission_enqueued.elapsed().as_millis() as i64;
            let _server = server_trace.as_ref().map(|span| span.enter());
            let _entered = crate::profile::enter(auth_profile.as_ref());
            let _origin = smallclaims::sqlite::telemetry::enter_origin(auth_origin);
            let authentication_span = crate::profile::span("admission/authenticate");
            let authenticate_started = Instant::now();
            let authentication =
                crate::relay_trace::result(crate::relay_trace::Phase::Authenticate, || {
                    client_v0::authenticate(&auth_state, &auth_request, transport)
                });
            let authenticate_ms = authenticate_started.elapsed().as_millis() as i64;
            drop(authentication_span);
            let snapshot = (!defer_detail_snapshot && !observation_report).then(|| {
                let snapshot_span = crate::profile::span("admission/snapshot");
                let snapshot_started = Instant::now();
                let snapshot = crate::relay_trace::work(crate::relay_trace::Phase::Snapshot, || {
                    client_request_snapshot(&auth_state, cursor_snapshot.flatten())
                });
                let snapshot_ms = snapshot_started.elapsed().as_millis() as i64;
                drop(snapshot_span);
                (snapshot, snapshot_ms)
            });
            let (snapshot, snapshot_ms) = snapshot.unzip();
            if let Some(server) = server_trace.as_ref() {
                server.record("st.admission.queue_ms", queue_ms);
                server.record("st.admission.authenticate_ms", authenticate_ms);
                if let Some(snapshot_ms) = snapshot_ms {
                    server.record("st.admission.snapshot_ms", snapshot_ms);
                }
            }
            (authentication, snapshot)
        })
        .await;
        match admitted {
            Ok((authentication, snapshot)) => (authentication.map(Some), snapshot),
            Err(error) => (Err(ApiError::internal(error)), None),
        }
    } else {
        (Ok(None), None)
    };
    if let Ok(Some(session)) = &client_authentication {
        // The clients connected now: kept in memory, shown by clients.list, never enforced.
        client_presence::note_request(&state.node, session, request.headers(), client_now_ms());
        request.extensions_mut().insert(session.clone());
    }
    if let Some(snapshot) = &client_snapshot {
        request.extensions_mut().insert(snapshot.clone());
    }
    let envelope_context = response_body::Context {
        state: state.clone(),
        client_request,
        observation_report,
        request_id: request_id.clone(),
        client_snapshot,
        server: otel.clone(),
        roster: request_route == "/v1/client/agents",
    };
    let response = match (fabric_boundary_error, client_authentication) {
        (Some(error), _) | (None, Err(error)) => error.into_response(),
        // Most handlers use synchronous SQLite and filesystem APIs. Run the whole
        // handler on a blocking thread so a busy projection or replication pass cannot
        // occupy an async worker needed to accept another call. Read workers are admitted
        // before taking store locks; nested work reuses the handler's reader.
        (None, Ok(_)) if request_path == "/v1/health" || observation_report => {
            let handler_started = Instant::now();
            let response = next.run(request).await;
            if let Some(server) = otel.as_ref() {
                server.record(
                    "st.handler.duration_ms",
                    handler_started.elapsed().as_millis() as i64,
                );
            }
            response
        }
        (None, Ok(_)) if smallclaims::read_budget::current().is_none() => {
            // A mutation's synchronous poll can borrow the writer. Its async waits must
            // own neither a writer permit nor a parked blocking parent (including bodies).
            let handler_profile = profile.clone();
            let cpu_kind = request_route.clone();
            let cpu_client = caller.clone();
            let mut handler_queue = profile.as_ref().map(|op| op.wall_span("handler/queue"));
            let mut diagnostic_queue = Some(crate::relay_trace::span(crate::relay_trace::Phase::HandlerQueue));
            let mut diagnostic_handler = crate::relay_trace::span(crate::relay_trace::Phase::Handler);
            let diagnostic = crate::relay_trace::current();
            let handler_enqueued = Instant::now();
            let first_poll = Arc::new(std::sync::OnceLock::<Instant>::new());
            let first_work = first_poll.clone();
            let server_trace = otel.clone();
            let writer_wait = (server_trace.is_some() || crate::otel::metrics_enabled()).then(|| {
                Arc::new(smallclaims::sqlite::telemetry::WriterWait::with_origin(origin))
            });
            let polled_writer_wait = writer_wait.clone();
            let remote = served_remote.clone();
            let result = read_deadline::spawn_mutation_handler(
                state.store.clone(),
                track_remote_reads(remote, next.run(request)),
                move |mut future, cx| {
                    if first_work.set(Instant::now()).is_ok() {
                        drop(handler_queue.take());
                        if let Some(mut queue) = diagnostic_queue.take() {
                            queue.finish(crate::relay_trace::Outcome::Completed);
                        }
                        if let Some(profile) = &handler_profile { profile.queued(); }
                    }
                    let _server = server_trace.as_ref().map(|span| span.enter());
                    let _writer_scope = smallclaims::sqlite::telemetry::enter_request(polled_writer_wait.clone());
                    let _entered = crate::profile::enter(handler_profile.as_ref());
                    crate::relay_trace::blocking(diagnostic.clone(), || {
                        crate::performance::with_cpu(Some(&cpu_kind), Some(&cpu_client), || {
                            future.as_mut().poll(cx)
                        })
                    })
                },
            ).await;
            let response = result.unwrap_or_else(|error| ApiError::internal(error).into_response());
            diagnostic_handler.finish(if response.status().is_success() {
                crate::relay_trace::Outcome::Completed
            } else {
                crate::relay_trace::Outcome::Failed
            });
            if let (Some(server), Some(started)) = (otel.as_ref(), first_poll.get()) {
                server.record("st.handler.queue_ms", started.duration_since(handler_enqueued).as_millis() as i64);
                server.record("st.handler.duration_ms", started.elapsed().as_millis() as i64);
                if let Some(wait) = writer_wait.as_ref() {
                    crate::otel::record_writer_wait(server, wait.totals());
                }
            }
            response
        }
        (None, Ok(_)) => {
            let runtime = tokio::runtime::Handle::current();
            let handler_profile = profile.clone();
            let cpu_kind = request_route.clone();
            let cpu_client = caller.clone();
            let handler_queue = profile.as_ref().map(|op| op.wall_span("handler/queue"));
            let mut diagnostic_queue =
                crate::relay_trace::span(crate::relay_trace::Phase::HandlerQueue);
            let handler_enqueued = Instant::now();
            let server_trace = otel.clone();
            let forwarded_handler = request_path == crate::peer::CLIENT_READ_FORWARD_PATH;
            let handler_envelope = envelope_context.clone();
            // The awaiting request owns body collection. Dropping it closes this
            // channel even when its already-started handler worker is detached.
            let (_body_owner, body_cancelled) = tokio::sync::oneshot::channel::<()>();
            let remote = served_remote.clone();
            let roster_trace = (request_route == "/v1/client/agents").then(|| server_trace.clone()).flatten();
            match crate::api::read_deadline::spawn_handler(
                roster_trace,
                None,
                move || {
                diagnostic_queue.finish(crate::relay_trace::Outcome::Completed);
                drop(handler_queue);
                let queue_ms = handler_enqueued.elapsed().as_millis() as i64;
                let _server = server_trace.as_ref().map(|span| span.enter());
                let writer_wait = (server_trace.is_some() || crate::otel::metrics_enabled()).then(|| {
                    Arc::new(smallclaims::sqlite::telemetry::WriterWait::with_origin(origin))
                });
                let _writer_scope = smallclaims::sqlite::telemetry::enter_request(writer_wait.clone());
                if let Some(profile) = &handler_profile {
                    profile.queued();
                }
                let _entered = crate::profile::enter(handler_profile.as_ref());
                let handler_started = Instant::now();
                let (response, handler_elapsed) =
                    crate::performance::with_cpu(Some(&cpu_kind), Some(&cpu_client), || {
                    let mut diagnostic_handler =
                        crate::relay_trace::span(crate::relay_trace::Phase::Handler);
                    let response = runtime.block_on(track_remote_reads(remote, crate::api::read_deadline::handler(async move {
                        // Cancel the actual forwarded relay, not only its outer waiter.
                        // Other routes retain their existing cooperative cancellation;
                        // this transport's mutation variants carry no read budget.
                        if let Some(budget) = smallclaims::read_budget::current()
                            .filter(|_| forwarded_handler)
                        {
                            match tokio::time::timeout(budget.remaining(), next.run(request)).await {
                                Ok(response) => response,
                                Err(_) => {
                                    budget.cancel();
                                    ApiError::bad(budget.check().unwrap_err()).into_response()
                                }
                            }
                        } else {
                            next.run(request).await
                        }
                    })));
                    diagnostic_handler.finish(if response.status().is_success() {
                        crate::relay_trace::Outcome::Completed
                    } else {
                        crate::relay_trace::Outcome::Failed
                    });
                    // The handler has returned its poll-scoped reader. Collect and frame
                    // JSON on this same blocking worker, under its existing read budget;
                    // a large response never runs a JSON codec on the async acceptor.
                    let handler_elapsed = handler_started.elapsed();
                    let response = runtime.block_on(handler_envelope.wrap_owned(response, body_cancelled))
                        .unwrap_or_else(IntoResponse::into_response);
                    (response, handler_elapsed)
                });
                if let Some(server) = server_trace.as_ref() {
                    server.record("st.handler.queue_ms", queue_ms);
                    server.record(
                        "st.handler.duration_ms",
                        handler_elapsed.as_millis() as i64,
                    );
                    if let Some(writer_wait) = writer_wait.as_ref() {
                        crate::otel::record_writer_wait(server, writer_wait.totals());
                    }
                }
                response
            })
            .await
            {
                Ok(response) => response,
                Err(error) => ApiError::internal(error).into_response(),
            }
        }
    };
    // Authentication errors and the small health/observation bypasses have no handler
    // worker. Ordinary handlers already framed their response on that worker.
    let mut response = if response.extensions().get::<response_body::Metrics>().is_some()
        || !response_body::is_json(&response)
    {
        response
    } else {
        envelope_context.wrap(response).await.unwrap_or_else(IntoResponse::into_response)
    };
    let metrics = response.extensions_mut().remove::<response_body::Metrics>();
    let status = response.status();
    record_request_latency(
        &request_method,
        &request_route,
        &request_path,
        request_query.as_deref(),
        &caller,
        started,
        Served {
            long_poll,
            remote: served_remote.load(std::sync::atomic::Ordering::Relaxed),
        },
    );
    crate::otel::record_http_request_duration(
        request_method.as_str(),
        &request_route,
        client_class,
        status,
        started.elapsed(),
    );
    finish_request_trace(otel, status);
    if let Some(profile) = profile {
        if let Some(metrics) = metrics {
            profile.enveloped(metrics.elapsed, metrics.bytes);
        }
        profile.finish();
    }
    response
}

static REQUEST_LATENCY: OnceLock<Mutex<request_latency::Meter>> = OnceLock::new();
/// The request's OTel server span: named `METHOD {route}`, a local root unless the caller
/// sent a valid W3C `traceparent`/`tracestate`, which HTTP requests and WebSocket
/// upgrades carry alike. `None` whenever trace export is off, so that path allocates
/// nothing. Phase durations land on this one span as numeric attributes
/// (`st.admission.queue_ms`, `st.admission.authenticate_ms`, `st.admission.snapshot_ms`,
/// `st.handler.queue_ms`, `st.handler.duration_ms`) instead of child spans: at
/// saturation five children per request priced in well above the O11Y-R18 CPU budget,
/// and a single span keeps the same phase visibility.
fn request_trace(
    method: &axum::http::Method,
    route: &str,
    client_class: crate::otel_client_class::ClientClass,
    headers: &axum::http::HeaderMap,
    request_id: &str,
) -> Option<tracing::Span> {
    if !crate::otel::export_enabled() {
        return None;
    }
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    let span_name = format!("{} {route}", method.as_str());
    let span = tracing::info_span!(
        target: "st3::http",
        "http server request",
        "otel.name" = span_name.as_str(),
        "otel.kind" = "server",
        "span.label" = route,
        "http.request.method" = method.as_str(),
        "http.route" = route,
        "st3.client.class" = client_class.as_str(),
        "st.request.id" = request_id,
        "http.response.status_code" = tracing::field::Empty,
        "st.parent.sampled" = tracing::field::Empty,
        "st.admission.queue_ms" = tracing::field::Empty,
        "st.admission.authenticate_ms" = tracing::field::Empty,
        "st.admission.snapshot_ms" = tracing::field::Empty,
        "st.handler.queue_ms" = tracing::field::Empty,
        "st.handler.duration_ms" = tracing::field::Empty,
        "st.roster.mode" = tracing::field::Empty,
        "st.roster.cards" = tracing::field::Empty,
        "st.page.rows" = tracing::field::Empty,
        "st.page.bytes" = tracing::field::Empty,
        "st.writer.wait_ms" = tracing::field::Empty,
        "st.writer.ops" = tracing::field::Empty,
    );
    let remote = crate::otel::extract_remote_context(headers);
    let parent = remote.span();
    let context = parent.span_context();
    if context.is_valid() {
        span.record(
            "st.parent.sampled",
            context.is_sampled() && context.trace_state().get("st").is_none(),
        );
    }
    span.set_parent(remote);
    Some(span)
}

/// Record the response status, mark server faults (5xx, or a handler panic surfaced as a
/// 500) as span errors — a 4xx is the caller's, not the server's — and end the server
/// span by dropping its last handle.
fn finish_request_trace(server: Option<tracing::Span>, status: axum::http::StatusCode) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    let Some(server) = server else {
        return;
    };
    server.record("http.response.status_code", i64::from(status.as_u16()));
    if status.is_server_error() {
        server.set_status(opentelemetry::trace::Status::error(format!(
            "HTTP {status}"
        )));
    }
}


fn request_latency() -> &'static Mutex<request_latency::Meter> {
    REQUEST_LATENCY.get_or_init(|| Mutex::new(request_latency::Meter::default()))
}

/// Count one published current-work stage's duration beside the request rows.
fn record_work_stage(stage: request_latency::WorkStage, elapsed: Duration) {
    request_latency().lock().unwrap_or_else(std::sync::PoisonError::into_inner).record_work_stage(stage, elapsed);
}

/// Count one agents roster stage's duration beside the request rows.
fn record_roster_stage(stage: request_latency::RosterStage, elapsed: Duration) {
    // A poisoned diagnostic lock must not stop the refresher that records into it.
    request_latency().lock().unwrap_or_else(std::sync::PoisonError::into_inner).record_roster_stage(stage, elapsed);
    #[cfg(test)]
    roster_stage_recorded().send_modify(|_| {});
}

/// Publication precedes the completed refresh timing; tests asserting timings wait here.
#[cfg(test)]
fn roster_stage_recorded() -> &'static tokio::sync::watch::Sender<()> {
    static RECORDED: OnceLock<tokio::sync::watch::Sender<()>> = OnceLock::new();
    RECORDED.get_or_init(|| tokio::sync::watch::channel(()).0)
}

fn request_latency_snapshot() -> Vec<Value> {
    request_latency().lock().unwrap().snapshot()
}

/// Only the targets' live windows, for the collector: see `slo/targets.toml`.
pub fn request_latency_target_windows() -> Value {
    request_latency().lock().unwrap().targets_only(Instant::now())
}

/// Every target's live windows, and every path's: see `slo/targets.toml`.
pub(crate) fn request_latency_windows() -> Value {
    request_latency().lock().unwrap().windows(Instant::now())
}

pub(crate) use crate::peer::track_remote_reads;

/// A query that asks the daemon to wait for a change makes the request a long poll.
fn asks_to_wait(query: &str) -> bool {
    query.split('&').any(|pair| match pair.split_once('=') {
        Some(("wait_ms", value)) => value.parse::<u64>().is_ok_and(|ms| ms > 0),
        Some(("wait", value)) => value == "true",
        _ => false,
    })
}

/// How a completed request was served, for its target.
#[derive(Clone, Copy, Default)]
struct Served {
    long_poll: bool,
    remote: bool,
}

/// A client collections socket's subscription sent its first snapshot or screen.
pub(crate) fn record_stream_latency(collection: &str, elapsed: Duration, remote: bool) {
    let timed = request_latency::Timed::resolve(
        &format!("stream {collection}"),
        remote,
        request_latency::Wait::No,
    );
    request_latency()
        .lock()
        .unwrap()
        .time(Instant::now(), timed, elapsed);
}

/// The most claims between two publications that one freshness sample reads.
pub(crate) const PUBLISH_SAMPLE_CLAIMS: u64 = 10_000;

/// Whether a publication at `cut` after one at `previous` is a freshness sample: it advanced,
/// by at most [`PUBLISH_SAMPLE_CLAIMS`] claims.
fn publish_sampled(previous: u64, cut: u64) -> bool {
    cut > previous && cut - previous <= PUBLISH_SAMPLE_CLAIMS
}

#[cfg(test)]
#[test]
fn a_publication_far_behind_its_previous_one_is_not_a_freshness_sample() {
    assert!(!publish_sampled(7, 7), "an unchanged cut");
    assert!(publish_sampled(7, 8));
    assert!(publish_sampled(7, 7 + PUBLISH_SAMPLE_CLAIMS));
    assert!(!publish_sampled(7, 8 + PUBLISH_SAMPLE_CLAIMS), "after a catch-up");
}

/// A refresher published `collection` at `cut`, after a publication at `previous`: time it
/// from the first claim this node committed after `previous` to the new publication, as the
/// `publish COLLECTION` path of `projection-fresh`. A publication with no newer local commit
/// (only replicated claims, or none) is not a sample, nor is one more than
/// [`PUBLISH_SAMPLE_CLAIMS`] claims after the previous: finding its first local commit would scan
/// them on the refresher's path, and a publication that far behind follows a catch-up, not a
/// commit a person waits on.
pub(crate) fn record_publish_latency(
    store: &Store,
    collection: &str,
    previous: u64,
    cut: u64,
    published_at_unix_ms: u128,
) {
    if !publish_sampled(previous, cut) {
        return;
    }
    let committed = match store.first_local_commit_after(previous, cut) {
        Ok(Some(committed)) => committed,
        Ok(None) => return,
        Err(error) => {
            eprintln!("st3: WARN cannot time the {collection} publication: {error:#}");
            return;
        }
    };
    let elapsed = published_at_unix_ms.saturating_sub(committed);
    let timed = request_latency::Timed::resolve(
        &format!("publish {collection}"),
        false,
        request_latency::Wait::No,
    );
    request_latency().lock().unwrap().time(
        Instant::now(),
        timed,
        Duration::from_millis(elapsed.min(u64::MAX as u128) as u64),
    );
}

fn record_request_latency(
    method: &axum::http::Method,
    route: &str,
    path: &str,
    query: Option<&str>,
    caller: &str,
    started: Instant,
    served: Served,
) {
    let elapsed = started.elapsed();
    crate::performance::record_request(route, Some(caller), elapsed);
    // The key and the target lookup allocate and scan; do them before taking the lock. A long
    // poll counts toward no target; a fresh agents list toward its own.
    let wait = if served.long_poll {
        request_latency::Wait::LongPoll
    } else if request_latency::is_fresh_agents_read(method, route, query) {
        request_latency::Wait::Fresh
    } else {
        request_latency::Wait::No
    };
    let timed = request_latency::Timed::resolve(&format!("{method} {route}"), served.remote, wait);
    {
        let mut meter = request_latency().lock().unwrap();
        meter.record(method, route, path, query, elapsed);
        meter.time(Instant::now(), timed, elapsed);
    }
    if elapsed < Duration::from_secs(1) {
        return;
    }
    // Request latency is an operational sample, not a graph change. Publishing a
    // claim here causes replication and reconciliation on every node, including
    // during the contention that made the request slow in the first place.
    eprintln!("st3: slow request {path} took {} ms", elapsed.as_millis());
}

pub(crate) use crate::model::new_request_id;

fn client_now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn client_snapshot_time(snapshot: &ClientSnapshot) -> u128 {
    chrono::DateTime::parse_from_rfc3339(&snapshot.created_at)
        .map(|time| u128::try_from(time.timestamp_millis()).unwrap_or_default())
        .unwrap_or_default()
}

fn new_client_snapshot(state: &AppState) -> ClientSnapshot {
    let store_index = state.store.index().unwrap_or_default();
    client_snapshot_at(state, store_index)
}

fn client_request_snapshot(
    state: &AppState,
    cursor_snapshot: Option<ClientSnapshot>,
) -> ClientSnapshot {
    // A relayed timeline cursor belongs to its owner host. Only that host can
    // validate its snapshot index and page cache; the gateway still needs a
    // local snapshot to resolve the current session owner and authenticate.
    cursor_snapshot
        .filter(|snapshot| snapshot.host_id == client_host_id(&state.node))
        .unwrap_or_else(|| new_client_snapshot(state))
}

fn client_snapshot_at(state: &AppState, store_index: u64) -> ClientSnapshot {
    #[cfg(test)]
    client_v0::observer_subscription_detail_tests::note_snapshot_construction(state);
    client_snapshot_with_time(
        state,
        store_index,
        state
            .store
            .projection_time_at(store_index)
            .unwrap_or_default(),
    )
}

/// The snapshot at `store_index` when its acceptance time (`projection_time_at`) is already known.
fn client_snapshot_with_time(state: &AppState, store_index: u64, unix_ms: u128) -> ClientSnapshot {
    let created_at = client_timestamp(unix_ms);
    let fingerprint = hex::encode(Sha256::digest(
        format!(
            "{CLIENT_PROJECTION_VERSION}:{}:{store_index}:{created_at}",
            state.node
        )
        .as_bytes(),
    ));
    ClientSnapshot {
        id: format!(
            "snapshot/{}/{store_index}/{}",
            state.node.replace(char::is_whitespace, "-"),
            &fingerprint[..16]
        ),
        host_id: client_host_id(&state.node),
        store_index,
        projection_version: CLIENT_PROJECTION_VERSION.into(),
        created_at,
        published_at: None,
    }
}

fn client_host_id(node: &str) -> String {
    format!("host/{}", node.replace(char::is_whitespace, "-"))
}

fn client_error_envelope(status: StatusCode, raw: &Value, request_id: &str) -> Value {
    let mut envelope = json!({
        "api_version": CLIENT_API_VERSION,
        "error_version": "st3.client.error.v0",
        "request_id": request_id,
        "code": client_error_code(raw.get("code").and_then(Value::as_str)),
        "message": raw.get("message").and_then(Value::as_str).unwrap_or("the request failed"),
        "retryable": client_error_retryable(status, raw.get("code").and_then(Value::as_str)),
        "details": raw.get("details").cloned().unwrap_or_else(|| json!({})),
    });
    if let Some(applied) = raw.get("applied") {
        envelope["applied"] = applied.clone();
    }
    envelope
}

fn client_error_retryable(status: StatusCode, code: Option<&str>) -> bool {
    // An ended list publishes nothing until the daemon restarts.
    if matches!(code, Some(published_lists::WORK_LIST_ENDED | client_v0::published_missions::ENDED | "runtime-list-ended")) {
        return false;
    }
    // An unconfirmed mutation needs the exact original request, not a generic
    // transient retry that might replace its identity or its signed bytes.
    if code == Some("message-send-unconfirmed") {
        return false;
    }
    if matches!(code, Some(
        "arrangement-exists" | "arrangement-folder-exists" | "arrangement-retired"
        | "arrangement-limit" | "arrangement-folder-deleted" | "arrangement-cycle"
        | "arrangement-body-too-large" | "arrangement-owner-forbidden"
        | "invalid-arrangement-subject" | "invalid-arrangement-action"
        | "invalid-arrangement-operations" | "invalid-arrangement-folder"
        | "invalid-arrangement-name" | "invalid-arrangement-key" | "invalid-subject-reference"
    )) { return false; }
    matches!(
        code,
        Some(
            "remote-unavailable"
                | "terminal-unavailable"
                | "cursor-gap"
                | "conversation-content-invalidated"
                | "page-cursor-expired"
                | "rate-limited"
                | "runtime-authority-indeterminate"
        )
    ) || matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::BAD_GATEWAY
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn client_error_code(code: Option<&str>) -> String {
    match code.unwrap_or("internal") {
        "recipient-retired"
        | "lifecycle-unavailable"
        | "retirement-upgrade-required"
        | "not-found"
        | "forbidden"
        | "attention-migrated"
        | "unsupported-capability"
        | "validation-failed"
        | "idempotency-conflict"
        | "idempotency-key-expired"
        | "issuer-required"
        | "suspend-authored-session-selection"
        | "arrangement-exists"
        | "arrangement-folder-exists"
        | "arrangement-retired"
        | "arrangement-limit"
        | "arrangement-folder-deleted"
        | "arrangement-cycle"
        | "arrangement-body-too-large"
        | "arrangement-owner-forbidden"
        | "invalid-arrangement-subject"
        | "invalid-arrangement-action"
        | "invalid-arrangement-operations"
        | "invalid-arrangement-folder"
        | "invalid-arrangement-name"
        | "invalid-arrangement-key"
        | "invalid-subject-reference"
        | "stale-fence"
        | "timeline-history-incomplete"
        | "projection-detail-too-large"
        | "projection-detail-invalid-source"
        | "cursor-gap"
        | "page-cursor-expired"
        | "agent-roster-cut-not-ready"
        | "rate-limited"
        | "mission-list-not-ready"
        | "mission-list-ended"
        | "runtime-not-local"
        | "runtime-authority-indeterminate"
        | "runtime-list-not-ready"
        | "runtime-list-ended"
        | "remote-unavailable"
        | "terminal-unavailable"
        | "conversation-content-invalidated"
        | "transcript-unavailable"
        | "terminal-ended"
        | "blob-too-large"
        | "message-too-large"
        | "message-store-full"
        | "long-message-signature-unsupported"
        | "unsupported-media-type"
        | "blob-content-mismatch"
        | "blob-quota-exceeded"
        | "blob-not-found"
        | "blob-expired"
        | "database-busy"
        | "database-locked"
        | "message-send-unconfirmed"
        | "response-budget-exceeded"
        | "content-not-text"
        | "internal" => code.unwrap_or("internal").to_owned(),
        "too-many-attachments" | "invalid-blob-reference" | "invalid-slo-period" => "validation-failed".into(),
        "launch-review-not-authorized"
        | "wrong-message-recipient"
        | "lane-approval-denied"
        | "glass-owner-forbidden"
        | "foreign-agent-actor" => "forbidden".into(),
        "lane-not-found" => "not-found".into(),
        "invalid-person-ask"
        | "invalid-person-response"
        | "invalid-person-request"
        | "invalid-person-answer"
        | "answer-required"
        | "unsupported-person-request"
        | "missing-ask-owner"
        | "ambiguous-ask-owner"
        | "update-not-asked" => "validation-failed".into(),
        // A review refusal says why in its message: answered and by whom, or what moved on.
        "review-not-requested"
        | "review-target-unknown"
        | "review-decision-not-offered"
        | "missing-review-reason"
        | "invalid-review-decision"
        | "feedback-gate-needs-step" => "validation-failed".into(),
        "wrong-reviewer" => "forbidden".into(),
        "stale-work-ask"
        | "stale-subject"
        | "missing-subject-token"
        | "stale-document-token"
        | "stale-incarnation"
        | "stale-launch-preview"
        | "wrong-work-incarnation" => "stale-fence".into(),
        "idempotency-mismatch" => "idempotency-conflict".into(),
        "run-not-queued"
        | "missing-queue-anchor"
        | "unexpected-queue-anchor"
        | "invalid-queue-anchor"
        | "invalid-queue-placement"
        | "ambiguous-lane"
        | "lane-closed"
        | "entry-not-in-lane"
        | "invalid-lane-actor"
        | "invalid-lane-entry"
        | "invalid-lane-outcome"
        | "invalid-lane-placement"
        | "missing-lane-anchor"
        | "unexpected-lane-anchor"
        | "invalid-lane-anchor"
        | "invalid-lane-state"
        | "invalid-lane-change" | "glass-limit" | "glass-deleted" | "invalid-glass-base" => "validation-failed".into(),
        // A retry of a request whose claim a checkpoint dropped cannot be answered again.
        "claim-checkpointed" => "idempotency-conflict".into(),
        _ => "internal".into(),
    }
}

fn encode_client_cursor(cursor: &ClientPageCursor) -> Result<String, ApiError> {
    let encoded = serde_json::to_vec(cursor).map_err(ApiError::internal)?;
    Ok(format!(
        "page/{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(encoded)
    ))
}

fn decode_client_cursor(cursor: &str) -> Result<ClientPageCursor, ApiError> {
    let encoded = cursor.strip_prefix("page/").ok_or_else(|| ApiError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: "validation-failed".into(),
        message: "the page cursor is malformed".into(),
        details: Box::default(),
    })?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "validation-failed".into(),
            message: "the page cursor is malformed".into(),
            details: Box::default(),
        })?;
    serde_json::from_slice(&bytes).map_err(|_| ApiError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        code: "validation-failed".into(),
        message: "the page cursor is malformed".into(),
        details: Box::default(),
    })
}

fn decode_client_cursor_snapshot(cursor: &str) -> Result<ClientSnapshot, ApiError> {
    if cursor.starts_with(client_v0::NATIVE_PAGE_CURSOR_PREFIX) {
        client_v0::native_page_cursor_snapshot(cursor)
    } else {
        decode_client_cursor(cursor).map(|cursor| cursor.snapshot)
    }
}

fn client_page_expired(message: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::GONE,
        code: "page-cursor-expired".into(),
        message: message.into(),
        details: Box::default(),
    }
}

fn client_page(
    state: &AppState,
    snapshot: &ClientSnapshot,
    collection: &str,
    items: Vec<Value>,
    query: &ClientListQuery,
) -> Result<ClientResourcePage, ApiError> {
    client_page_read(state, snapshot, collection, items, query, false)
}

/// Preserve complete timeline entries while reserving the existing page-envelope budget.
fn client_timeline_page_end(
    items: &[Value],
    offset: usize,
    limit: usize,
) -> Result<usize, ApiError> {
    struct EncodedSize(usize);
    impl std::io::Write for EncodedSize {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // Match mission-card paging's reserve for the response, cursor and fleet notice.
    let budget = client_v0::TIMELINE_PAGE_BYTE_BUDGET;
    let mut used = 2usize; // JSON array delimiters.
    let mut end = offset.min(items.len());
    for item in items.iter().skip(offset).take(limit) {
        let mut encoded = EncodedSize(0);
        serde_json::to_writer(&mut encoded, item).map_err(ApiError::internal)?;
        let bytes = encoded.0.saturating_add(usize::from(end > offset));
        if used.saturating_add(bytes) > budget {
            if end == offset {
                // A lone entry above the target is served alone; one above the hard ceiling
                // is cut visibly when the page is cut from the items.
                end += 1;
            }
            break;
        }
        used += bytes;
        end += 1;
    }
    Ok(end)
}

/// A page of `items`, or of the cached first page a cursor continues. `pinned` says the items
/// were read in `snapshot` itself, so a commit since then cannot have torn them; otherwise a
/// first page is refused once the store has moved past the snapshot.
fn client_page_read(
    state: &AppState,
    snapshot: &ClientSnapshot,
    collection: &str,
    items: Vec<Value>,
    query: &ClientListQuery,
    _pinned: bool,
) -> Result<ClientResourcePage, ApiError> {
    let (owner, terminal_state) = if collection == "terminals" {
        (query.owner.as_deref(), query.state.as_deref())
    } else {
        (None, None)
    };
    let requested_limit = query
        .limit
        .unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS)
        .clamp(1, CLIENT_MAX_PAGE_ITEMS);
    let (items, items_digest, offset, limit, expires_at_unix_ms) = if let Some(cursor) =
        &query.cursor
    {
        let cursor = decode_client_cursor(cursor)?;
        if cursor.collection != collection
            || cursor.history != query.history
            || cursor.person != query.person
            || cursor.actor != query.actor
            || cursor.owner_run != query.owner_run
            || cursor.owner.as_deref() != owner
            || cursor.state.as_deref() != terminal_state
            || cursor.status != query.status
            || cursor.native_only != query.native_only
            || query
                .limit
                .is_some_and(|limit| limit.clamp(1, CLIENT_MAX_PAGE_ITEMS) != cursor.limit)
            || cursor.snapshot.id != snapshot.id
            || cursor.snapshot.store_index != snapshot.store_index
        {
            return Err(client_page_expired(
                "the page cursor does not match this collection, snapshot, or filter",
            ));
        }
        if client_now_ms() > cursor.expires_at_unix_ms {
            return Err(client_page_expired("the page cursor expired"));
        }
        let cache = client_page_cache()
            .lock()
            .expect("client page cache poisoned");
        let cached = cache
            .iter()
            .find(|entry| {
                entry.snapshot_id == cursor.snapshot.id
                    && entry.collection == collection
                    && entry.items_digest == cursor.items_digest
                    && entry.expires_at_unix_ms == cursor.expires_at_unix_ms
            })
            .ok_or_else(|| {
                client_page_expired("the page snapshot is no longer available; restart pagination")
            })?;
        (
            cached.items.clone(),
            cursor.items_digest,
            cursor.offset,
            cursor.limit,
            cursor.expires_at_unix_ms,
        )
    } else {
        let items_digest = hex::encode(Sha256::digest(
            serde_json::to_vec(&items).map_err(ApiError::internal)?,
        ));
        let cached = client_page_cache()
            .lock()
            .expect("client page cache poisoned")
            .iter()
            .find(|entry| {
                entry.snapshot_id == snapshot.id
                    && entry.collection == collection
                    && entry.items_digest == items_digest
                    && entry.expires_at_unix_ms > client_now_ms()
            })
            .map(|entry| (entry.items.clone(), entry.expires_at_unix_ms));
        let (items, expires_at_unix_ms) = cached.unwrap_or_else(|| {
            (
                Arc::new(items),
                client_now_ms().saturating_add(CLIENT_PAGE_TTL_MS),
            )
        });
        (items, items_digest, 0, requested_limit, expires_at_unix_ms)
    };
    let end = if collection.starts_with("timeline/") {
        client_timeline_page_end(&items, offset, limit)?
    } else {
        offset.saturating_add(limit).min(items.len())
    };
    let end = if collection == "arrangements" {
        client_v0::arrangements::window_end(&items, offset, end)?
    } else {
        end
    };
    let mut page_items = items.get(offset..end).unwrap_or_default().to_vec();
    if collection.starts_with("timeline/")
        && let [only] = page_items.as_mut_slice()
    {
        let size = serde_json::to_vec(&*only).map_err(ApiError::internal)?.len();
        if size.saturating_add(2) > CLIENT_MAX_RESPONSE_BYTES - 128_000 {
            client_v0::cut_oversized_timeline_entry(only, size);
        }
    }
    let has_more = end < items.len();
    if query.cursor.is_none() && has_more {
        let mut cache = client_page_cache()
            .lock()
            .expect("client page cache poisoned");
        cache.retain(|entry| entry.expires_at_unix_ms > client_now_ms());
        if !cache.iter().any(|entry| {
            entry.snapshot_id == snapshot.id
                && entry.collection == collection
                && entry.items_digest == items_digest
                && entry.expires_at_unix_ms == expires_at_unix_ms
        }) {
            while cache.len() >= CLIENT_PAGE_CACHE_CAPACITY {
                cache.pop_front();
            }
            cache.push_back(CachedClientPage {
                snapshot_id: snapshot.id.clone(),
                collection: collection.into(),
                items_digest: items_digest.clone(),
                items: items.clone(),
                expires_at_unix_ms,
            });
        }
    }
    let next_cursor = if has_more {
        Some(encode_client_cursor(&ClientPageCursor {
            snapshot: snapshot.clone(),
            collection: collection.into(),
            offset: end,
            limit,
            history: query.history,
            person: query.person.clone(),
            actor: query.actor.clone(),
            owner_run: query.owner_run.clone(),
            status: query.status.clone(),
            owner: owner.map(str::to_owned),
            state: terminal_state.map(str::to_owned),
            native_only: query.native_only,
            items_digest,
            before_index: None,
            after_key: None,
            expires_at_unix_ms,
        })?)
    } else {
        None
    };
    Ok(ClientResourcePage {
        kind: "page".into(),
        collection: collection.into(),
        filters: client_page_filters(collection, query),
        items: page_items,
        page: ClientPageInfo {
            limit,
            has_more,
            next_cursor,
            cursor_expires_at: has_more.then(|| client_timestamp(expires_at_unix_ms)),
        },
        sync: client_sync_notice(state),
        replicated: None,
    })
}

/// The filters a page names: the ones its query applied.
fn client_page_filters(collection: &str, query: &ClientListQuery) -> BTreeMap<String, String> {
    let mut filters = BTreeMap::new();
    if query.history {
        filters.insert("history".into(), "all".into());
    }
    for (name, value) in [
        ("person", query.person.as_ref()),
        ("actor", query.actor.as_ref()),
        ("owner_run", query.owner_run.as_ref()),
        ("status", query.status.as_ref()),
    ] {
        if let Some(value) = value {
            filters.insert(name.into(), value.clone());
        }
    }
    if collection == "terminals" {
        for (name, value) in [
            ("owner", query.owner.as_ref()),
            ("state", query.state.as_ref()),
        ] {
            if let Some(value) = value {
                filters.insert(name.into(), value.clone());
            }
        }
    }
    if query.native_only {
        filters.insert("native_only".into(), "true".into());
    }
    filters
}

/// A page of `collection` whose first page `read` computes inside one SQLite snapshot. The page
/// names that snapshot, so a commit that lands while it reads neither tears it nor refuses it;
/// a later page comes from the first page's cache, as every cached page does.
async fn client_snapshot_page<F>(
    state: &AppState,
    snapshot: ClientSnapshot,
    collection: &'static str,
    query: &ClientListQuery,
    read: F,
) -> Result<ClientPageResponse, ApiError>
where
    F: FnOnce(&AppState, &ClientSnapshot) -> anyhow::Result<Vec<Value>> + Send + 'static,
{
    if query.cursor.is_some() {
        let page = client_page(state, &snapshot, collection, Vec::new(), query)?;
        return Ok((Extension(snapshot), Json(page)));
    }
    let reader = state.clone();
    let (snapshot, items) = blocking_store(move || {
        reader.store.clone().read_snapshot(|index| {
            let snapshot = client_snapshot_at(&reader, index);
            let items = reader
                .store
                .with_owned_set_snapshot_reads(|| read(&reader, &snapshot))?;
            Ok((snapshot, items))
        })
    })
    .await?;
    let page = client_page_read(state, &snapshot, collection, items, query, true)?;
    Ok((Extension(snapshot), Json(page)))
}

/// A host catching up with a peer can show early history as current, and a host whose graph
/// diverged from a peer's can show it wrong, so each page it serves says so and with whom.
fn client_sync_notice(state: &AppState) -> Option<ClientSyncNotice> {
    // Naming the peers reads the fleet view, so skip it on the usual page read.
    if !state.store.replication_catching_up() && !state.store.replication_diverged() {
        return None;
    }
    let peers = state
        .store
        .replication_peer_sync(&replication_peer_names(state))
        .into_iter()
        .filter(|(_, sync)| sync.catching_up || sync.diverged)
        .map(|(peer, sync)| {
            let (success, failure) = state.store.replication_peer_transport(&peer);
            let transport = |record: smallclaims::replication::ReplicationExchangeTransport| {
                crate::model::ClientExchangeTransport {
                    at: client_timestamp(record.at_unix_ms),
                    carrier: record.carrier.as_str().into(),
                    fabric_route: record.fabric_route,
                }
            };
            ClientSyncPeer {
                last_successful_exchange: success.map(transport),
                last_failed_exchange: failure.map(transport),
                host_id: client_host_id(&peer),
                peer_only_envelopes: sync.peer_only_envelopes,
                local_only_envelopes: sync.local_only_envelopes,
                last_exchange_at: state
                    .store
                    .replication_peer_last_success(&peer)
                    .ok()
                    .flatten()
                    .map(client_timestamp),
                estimated_catch_up_seconds: sync.estimated_catch_up_seconds.filter(|seconds| {
                    *seconds <= smallclaims::replication::MAX_SAFE_DURATION_SECONDS
                }),
                diverged_since: sync
                    .diverged
                    .then_some(sync.graph_differs_since_unix_ms)
                    .flatten()
                    .map(client_timestamp),
            }
        })
        .collect::<Vec<_>>();
    let sync_state = if peers.iter().any(|peer| peer.diverged_since.is_some()) {
        "diverged"
    } else {
        "catching-up"
    };
    (!peers.is_empty()).then(|| ClientSyncNotice {
        state: sync_state.into(),
        peers,
    })
}

fn client_detail_id(kind: &str, id: &str) -> String {
    if id.starts_with(&format!("{kind}/")) {
        id.to_owned()
    } else {
        format!("{kind}/{id}")
    }
}

fn client_detail(items: Vec<Value>, kind: &str, id: &str) -> Result<Json<Value>, ApiError> {
    let id = if items
        .iter()
        .any(|item| item.get("id").and_then(Value::as_str) == Some(id))
    {
        id.to_owned()
    } else {
        client_detail_id(kind, id)
    };
    items
        .into_iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id.as_str()))
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("{kind} `{id}` does not exist")))
}

async fn client_capabilities(
    State(state): State<AppState>,
    Extension(_snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<client_v0::ClientSession>,
) -> Result<Json<Value>, ApiError> {
    // The request snapshot may predate a checkpoint or legacy conversion. Advertise the
    // frontier and floor from one current read so the SDK's first event request can resume.
    let store = state.store.clone();
    let (oldest, newest) =
        blocking_store(move || store.read_snapshot(|_| store.event_bounds())).await?;
    let oldest = oldest.saturating_sub(1);
    let cursor = format!("event-cursor/{}/{newest}", state.node);
    let capabilities = client_v0::capabilities(&state, &session);
    // The longest message body this daemon takes, so a composer shows a limit only when told.
    let max_message_bytes = message_body::MAX_BYTES;
    Ok(Json(json!({
        "kind": "capabilities",
        "machine_version": st_drivers::version::machine_version(),
        "session_actor": session.actor,
        "transport": session.transport,
        "capabilities": capabilities,
        "limits": {
            "max_page_items": CLIENT_MAX_PAGE_ITEMS,
            "max_event_items": 500,
            "max_assistant_response_bytes": 16384,
            "default_assistant_response_bytes": 4096,
            "max_response_bytes": CLIENT_MAX_RESPONSE_BYTES,
            "max_wait_ms": 30_000,
            "max_message_bytes": max_message_bytes,
            "max_glass_body_bytes": st3_schema::glasses::MAX_BODY_BYTES,
            "max_glasses": st3_schema::glasses::MAX_GLASSES,
            "max_glass_depth": st3_schema::glasses::MAX_DEPTH,
            "max_glass_nodes": st3_schema::glasses::MAX_NODES,
            "max_arrangement_body_bytes": st3_schema::arrangements::MAX_BODY_BYTES,
            "max_arrangement_resource_bytes": st3_schema::arrangements::MAX_RESOURCE_BYTES,
            "max_arrangements": st3_schema::arrangements::MAX_ARRANGEMENTS,
            "max_arrangement_name_bytes": st3_schema::arrangements::MAX_NAME_BYTES,
            "max_arrangement_key_bytes": st3_schema::arrangements::MAX_KEY_BYTES,
            "max_arrangement_operations": st3_schema::arrangements::MAX_OPERATIONS,
            "max_arrangement_folders": st3_schema::arrangements::MAX_FOLDERS,
            "max_arrangement_placements": st3_schema::arrangements::MAX_PLACEMENTS
        },
        "event_cursor": cursor,
        "oldest_event_cursor": format!("event-cursor/{}/{oldest}", state.node),
        "schemas": [
            "../client-v0/schemas/client-v0.schema.json",
            "../client-v0/schemas/operations.json"
        ]
    })))
}

fn client_work_resources(
    store: &Store,
    actor: Option<&str>,
    history: bool,
    snapshot_unix_ms: u128,
    snapshot_index: u64,
) -> anyhow::Result<Vec<Value>> {
    if !history {
        store.count_direct_work_read();
    }
    let mut work = if history {
        store.client_work_history_at_snapshot(actor, snapshot_unix_ms)?
    } else {
        store.client_work_at_snapshot(actor, false, snapshot_unix_ms)?
    };
    if history {
        work.sort_by(|left, right| {
            right
                .updated_at_unix_ms
                .cmp(&left.updated_at_unix_ms)
                .then_with(|| left.subject.cmp(&right.subject))
        });
    } else {
        // An agent's own list shows its ready work in the same seat-queue order
        // that chooses its next work and wake.
        let seat_ready = match actor {
            Some(actor) => {
                let seat = if actor.contains('/') {
                    actor.to_owned()
                } else {
                    format!("agent/{actor}")
                };
                let order = store.seat_run_order(&seat)?;
                let steps = work
                    .iter()
                    .map(crate::seat_queue::SeatStep::from)
                    .collect::<Vec<_>>();
                crate::seat_queue::select(&seat, &steps, &order)
                    .ready
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            }
            None => Vec::new(),
        };
        let seat_rank = |subject: &str| {
            seat_ready
                .iter()
                .position(|ready| ready == subject)
                .unwrap_or(usize::MAX)
        };
        work.sort_by(|left, right| {
            let priority = |status: &str| match status {
                "ready" => 0,
                "claimed" | "working" => 1,
                "verifying" => 2,
                "blocked" => 3,
                "pending" | "waiting" => 4,
                _ => 5,
            };
            priority(&left.status)
                .cmp(&priority(&right.status))
                .then_with(|| seat_rank(&left.subject).cmp(&seat_rank(&right.subject)))
                .then_with(|| left.readiness_epoch.cmp(&right.readiness_epoch))
                .then_with(|| left.step.cmp(&right.step))
                .then_with(|| left.subject.cmp(&right.subject))
        });
    }
    // Only the seats these steps own, not every subject the fleet has ever declared.
    let desired = store.desired_subjects_for_owner_steps(
        &work
            .iter()
            .map(|step| step.subject.clone())
            .collect::<Vec<_>>(),
    )?;
    client_work_values(store, work, &desired, snapshot_index)
}

/// One page of the work history, newest update first, and whether more follow. Rendering the
/// whole history to show a page of it enriched every step the store had ever run: seconds on
/// a busy host's store.
#[cfg(test)]
fn client_work_history_page(
    store: &Store,
    actor: Option<&str>,
    snapshot_unix_ms: u128,
    snapshot_index: u64,
    offset: usize,
    limit: usize,
) -> anyhow::Result<(Vec<Value>, bool)> {
    client_work_history_page_after(
        store,
        actor,
        snapshot_unix_ms,
        snapshot_index,
        offset,
        limit,
        None,
    )
}

fn client_work_history_page_after(
    store: &Store,
    actor: Option<&str>,
    snapshot_unix_ms: u128,
    snapshot_index: u64,
    offset: usize,
    limit: usize,
    after: Option<&(u128, String)>,
) -> anyhow::Result<(Vec<Value>, bool)> {
    let (work, has_more) =
        store.client_work_history_page_after(actor, snapshot_unix_ms, offset, limit, after)?;
    let desired = store.desired_subjects_for_owner_steps(
        &work
            .iter()
            .map(|step| step.subject.clone())
            .collect::<Vec<_>>(),
    )?;
    Ok((
        client_work_values(store, work, &desired, snapshot_index)?,
        has_more,
    ))
}

/// One work item, read and rendered alone. Rendering the whole history to pick one item
/// enriched every step the store had ever run: seconds on a busy host's store.
fn client_work_item(
    store: &Store,
    id: &str,
    actor: Option<&str>,
    snapshot_unix_ms: u128,
    snapshot_index: u64,
) -> anyhow::Result<Option<Value>> {
    let Some(work) = store.client_work_item_at_snapshot(id, actor, snapshot_unix_ms)? else {
        return Ok(None);
    };
    let desired = store.desired_subjects_for_owner_step(&work.subject)?;
    Ok(client_work_values(store, vec![work], &desired, snapshot_index)?.pop())
}

/// Client resources for `work`, with the usage of the seats in `desired` that its steps own.
fn client_work_values(
    store: &Store,
    work: Vec<crate::model::StepRunView>,
    desired: &[crate::model::DesiredSubject],
    snapshot_index: u64,
) -> anyhow::Result<Vec<Value>> {
    let work_subjects = work
        .iter()
        .map(|step| step.subject.as_str())
        .collect::<BTreeSet<_>>();
    let usage_subjects = desired
        .iter()
        .filter(|seat| {
            seat.owner_step
                .as_deref()
                .is_some_and(|step| work_subjects.contains(step))
        })
        .map(|seat| seat.subject.clone())
        .collect::<Vec<_>>();
    let usage_summaries = store.usage_summaries_at(&usage_subjects, Some(snapshot_index))?;
    let mut usage_by_step = BTreeMap::<&str, Vec<&crate::model::UsageSummary>>::new();
    for seat in desired {
        if let (Some(step), Some(usage)) = (
            seat.owner_step.as_deref(),
            usage_summaries.get(&seat.subject),
        ) {
            usage_by_step.entry(step).or_default().push(usage);
        }
    }
    let agentless_runs = work
        .iter()
        .filter(|item| item.agentless)
        .map(|item| item.run.clone())
        .collect::<Vec<_>>();
    let step_specs = store
        .mission_specs_for_runs(&agentless_runs)?
        .into_iter()
        .map(|(run, mission)| (run, mission.steps))
        .collect::<BTreeMap<_, _>>();
    let work_annotations = store.work_annotations(&work)?;
    let run_missions = store.run_missions(
        &work
            .iter()
            .map(|item| item.run.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>(),
    )?;
    work.into_iter()
        .map(|work| {
            let operational = work_annotations
                .get(&work.subject)
                .expect("every work item has an annotation");
            let state = client_work_state(&work.status);
            let usage = aggregate_usage_values(
                usage_by_step
                    .get(work.subject.as_str())
                    .into_iter()
                    .flat_map(|summaries| summaries.iter().copied()),
            );
            let gate_kind = if work.agentless {
                let spec = step_specs
                    .get(&work.run)
                    .and_then(|steps| steps.get(&work.step));
                match spec {
                    Some(spec) if spec.after_run.is_some() => Some("run"),
                    Some(spec) if spec.gates.is_empty() => Some("watch"),
                    Some(spec)
                        if spec.gates.iter().all(|gate| {
                            matches!(gate, crate::model::GateSpec::Mechanical { .. })
                        }) =>
                    {
                        Some("command")
                    }
                    Some(spec)
                        if spec
                            .gates
                            .iter()
                            .all(|gate| matches!(gate, crate::model::GateSpec::Llm { .. })) =>
                    {
                        Some("llm")
                    }
                    Some(spec)
                        if spec
                            .gates
                            .iter()
                            .all(|gate| matches!(gate, crate::model::GateSpec::Human { .. })) =>
                    {
                        Some("human")
                    }
                    Some(spec)
                        if spec.gates.iter().all(|gate| {
                            !matches!(
                                gate,
                                crate::model::GateSpec::Mechanical { .. }
                                    | crate::model::GateSpec::Llm { .. }
                                    | crate::model::GateSpec::Human { .. }
                            )
                        }) =>
                    {
                        Some("predicate")
                    }
                    Some(_) => Some("mixed"),
                    None => None,
                }
            } else {
                None
            };
            Ok(json!({
                "id": work.subject,
                "kind": "work",
                "revision": work.definition_hash,
                "updated_at": client_timestamp(work.updated_at_unix_ms),
                "mission_id": run_missions.get(&work.run),
                "mission_run_id": work.run,
                "generation_id": work.generation,
                "definition_id": work.definition_hash,
                "path": work.step,
                "title": work.title,
                "assigned_to": work.assigned_to,
                "last_progress": work.progress_summary,
                "progress_at": work.progress_at_unix_ms.map(client_timestamp),
                "state": state,
                "agentless": work.agentless,
                "gate_kind": gate_kind,
                "attempt": work.attempt,
                "readiness_epoch": work.readiness_epoch,
                "claimant": work.claimant,
                "claim_incarnation": work.claim_incarnation,
                "submitted_incarnation": work.submitted_incarnation,
                "claim_expires_at_unix_ms": work.claim_expires_at_unix_ms,
                "execution_started_at_unix_ms": work.execution_started_at_unix_ms,
                "nudged_at_unix_ms": work.nudged_at_unix_ms,
                "execution_elapsed_ms": work.execution_elapsed_ms,
                "timeout_ms": work.timeout_ms,
                "goals": work.goals,
                "constraints": work.constraints,
                "person_answers": work.person_answers,
                "blocked_reason": work.blocked_reason,
                "blockers": work.blockers,
                "usage": usage,
                "operational": operational
            }))
        })
        .collect()
}

#[cfg(test)]
fn aggregate_usage<'a>(
    store: &Store,
    subjects: impl Iterator<Item = &'a str>,
    at_index: Option<u64>,
) -> anyhow::Result<Option<crate::model::UsageSummary>> {
    let usages = subjects
        .map(|subject| store.usage_summary_at(subject, None, at_index))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(aggregate_usage_values(usages.iter().flatten()))
}

fn aggregate_usage_values<'a>(
    usages: impl Iterator<Item = &'a crate::model::UsageSummary>,
) -> Option<crate::model::UsageSummary> {
    let mut aggregate = None::<crate::model::UsageSummary>;
    for usage in usages {
        let total = aggregate.get_or_insert_with(|| crate::model::UsageSummary {
            aggregation: "cumulative-per-incarnation-else-response-deltas".into(),
            ..crate::model::UsageSummary::default()
        });
        total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
        total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
        total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
        total.cached_tokens = total.cached_tokens.saturating_add(usage.cached_tokens);
        total.cache_write_tokens = total
            .cache_write_tokens
            .saturating_add(usage.cache_write_tokens);
        total.incarnation_count = total
            .incarnation_count
            .saturating_add(usage.incarnation_count);
        if let Some(cost) = usage.cost {
            total.cost = Some(total.cost.unwrap_or_default() + cost);
        }
        total.currency = total.currency.clone().or(usage.currency.clone());
    }
    aggregate
}

#[cfg(test)]
fn aggregate_usage_for_step(
    store: &Store,
    desired: &[crate::model::DesiredSubject],
    step: &str,
    at_index: Option<u64>,
) -> anyhow::Result<Option<crate::model::UsageSummary>> {
    aggregate_usage(
        store,
        desired
            .iter()
            .filter(|subject| subject.owner_step.as_deref() == Some(step))
            .map(|subject| subject.subject.as_str()),
        at_index,
    )
}

#[cfg(test)]
fn aggregate_usage_for_runs(
    store: &Store,
    desired: &[crate::model::DesiredSubject],
    runs: &BTreeSet<&str>,
    at_index: Option<u64>,
) -> anyhow::Result<Option<crate::model::UsageSummary>> {
    aggregate_usage(
        store,
        desired
            .iter()
            .filter(|subject| {
                subject
                    .owner_run
                    .as_deref()
                    .is_some_and(|run| runs.contains(run))
            })
            .map(|subject| subject.subject.as_str()),
        at_index,
    )
}

fn client_agent_resources(
    store: &Store,
    history: bool,
    at: &str,
    snapshot_index: u64,
) -> anyhow::Result<Vec<Value>> {
    let mut items = client_agent_resources_cached(store, history, snapshot_index)?;
    overlay_agent_resources(store, &mut items, at)?;
    Ok(items)
}

fn client_agent_resources_cached(
    store: &Store,
    history: bool,
    snapshot_index: u64,
) -> anyhow::Result<Vec<Value>> {
    store.cached_agent_resources(snapshot_index, history, |changed| {
        let mut items = client_agent_resources_selected(store, history, snapshot_index, changed)?;
        add_agent_todos(store, &mut items, snapshot_index)?;
        Ok(items)
    })
}

// Freeze membership, ordering and the inexpensive declaration/queue metadata. The expensive
// status, usage, fault and activity reductions are needed only for the returned page.
fn client_agent_page_refs(store: &Store, history: bool, index: u64) -> anyhow::Result<Vec<Value>> {
    store.cached_agent_page_refs(index, history, || client_agent_page_refs_uncached(store, history, index))
}

fn client_agent_page_refs_uncached(store: &Store, history: bool, index: u64) -> anyhow::Result<Vec<Value>> {
    let connection = store.readers.get();
    let (subjects, desired) = crate::otel::roster_phase("membership", || -> anyhow::Result<_> {
        let mut subjects = connection
            .prepare_cached(crate::store::RANGE_SUBJECTS)?
            .query_map(rusqlite::params![index, "agent/", "agent0"], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<BTreeSet<_>, _>>()?;
        if !history {
            subjects = store.current_view_candidates(&connection, subjects, index, true)?;
        }
        let names = subjects.iter().cloned().collect::<Vec<_>>();
        let desired = store
            .desired_subjects_named(&names)?
            .into_iter()
            .map(|desired| (desired.subject.clone(), desired))
            .collect::<BTreeMap<_, _>>();
        Ok((subjects, desired))
    })?;
    crate::otel::roster_phase("queue_selection", || -> anyhow::Result<Vec<Value>> {
        let queues = store.agent_work_queues()?;
        let steps = queues
            .values()
            .flat_map(|queue| {
                queue
                    .current_work_ids
                    .iter()
                    .chain(queue.next_work_id.iter())
                    .chain(queue.upcoming_work_ids.iter())
                    .cloned()
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let labels = store.step_labels(&steps)?;
        let label = |id: &String| {
            labels.get(id).map(|step| json!({
            "id":id, "mission_id":step.mission, "mission_run_id":step.run,
            "path":step.path, "title":step.title, "goal":step.goal,
            "state":client_work_state(&step.status), "since":client_timestamp(step.updated_at_unix_ms),
        }))
        };
        let mut refs = subjects.into_iter().map(|id| {
            let declaration = desired.get(&id);
            let name = crate::model::effective_agent_name(&id, declaration.map(|d| &d.desired));
            let queue = queues.get(&id).cloned().unwrap_or_default();
            let mut reference = json!({
                "id":id, "name":name,
                "host_id":declaration.and_then(|d| d.member.as_ref()).map(|m| client_host_id(&m.host)),
                "current_work_ids":queue.current_work_ids, "active_work_count":queue.active_work_count,
                "next_work_id":queue.next_work_id, "upcoming_work_ids":queue.upcoming_work_ids,
                "queued_work_count":queue.queued_work_count,
                "current_work":queue.current_work_ids.iter().filter_map(label).collect::<Vec<_>>(),
                "next_work":queue.next_work_id.as_ref().and_then(label),
                "upcoming_work":queue.upcoming_work_ids.iter().filter_map(label).collect::<Vec<_>>(),
            });
            if let Some(lifecycle) = crate::model::declared_agent_lifecycle(declaration.map(|d| &d.desired)) {
                reference["lifecycle"] = json!(lifecycle);
            }
            reference
        }).collect::<Vec<_>>();
        refs.sort_by(|a, b| {
            a["name"]
                .as_str()
                .cmp(&b["name"].as_str())
                .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
        });
        Ok(refs)
    })
}

fn client_agent_cards_for_page(
    store: &Store,
    history: bool,
    index: u64,
    refs: &[Value],
    at: &str,
) -> Result<Vec<Value>, ApiError> {
    let selected = refs
        .iter()
        .filter_map(|r| r["id"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    let cards = client_agent_cards_selected(store, history, index, &selected, false)
        .map_err(ApiError::internal)?;
    client_agent_cards_from_cached(store, cards, refs, at)
}

/// The cards of `selected` agents at `index`, folding only those not already cached there.
/// A `chunk` of a roster assembled in short folds refolds no card outside `selected`.
fn client_agent_cards_selected(
    store: &Store,
    history: bool,
    index: u64,
    selected: &BTreeSet<String>,
    chunk: bool,
) -> anyhow::Result<Vec<Value>> {
    let build = |changed: Option<(&BTreeSet<String>, &[Value])>| {
        let (subjects, previous) = changed.expect("a selected page always names its missing cards");
        // Delta metadata is current and already diffed; only cold pages need shallow refs.
        // Frozen continuation refs remain response metadata, never shared cache inputs.
        let metadata = if previous.is_empty() {
            Some(if index < store.index()? {
                agent_queue_metadata(store, subjects)?
            } else {
                client_agent_page_refs(store, history, index)?
            })
        } else { None };
        let mut cards = client_agent_resources_selected(
            store, history, index, Some((subjects, metadata.as_deref().unwrap_or(previous))),
        )?;
        add_agent_todos(store, &mut cards, index)?;
        Ok(cards)
    };
    if chunk {
        store.cached_agent_resources_chunk(index, history, selected, build)
    } else {
        store.cached_agent_resources_for(index, history, Some(selected), build)
    }
}

fn client_agent_cards_from_cached(
    store: &Store,
    mut cards: Vec<Value>,
    refs: &[Value],
    at: &str,
) -> Result<Vec<Value>, ApiError> {
    if cards.len() != refs.len() {
        return Err(client_page_expired(
            "agent page membership is no longer available; restart pagination",
        ));
    }
    let mut ordered = Vec::with_capacity(refs.len());
    for reference in refs {
        let position = cards
            .iter()
            .position(|card| card["id"] == reference["id"])
            .ok_or_else(|| {
                client_page_expired(
                    "agent page membership is no longer available; restart pagination",
                )
            })?;
        let mut card = cards.swap_remove(position);
        // The original cut's ordering and host metadata must survive later declaration changes.
        card["name"] = reference["name"].clone();
        card["host_id"] = reference["host_id"].clone();
        for field in [
            "current_work_ids", "active_work_count", "next_work_id", "upcoming_work_ids",
            "queued_work_count", "current_work", "next_work", "upcoming_work",
        ] {
            card[field] = reference[field].clone();
        }
        ordered.push(card);
    }
    overlay_agent_resources(store, &mut ordered, at).map_err(ApiError::internal)?;
    Ok(ordered)
}

fn add_agent_todos(store: &Store, items: &mut [Value], index: u64) -> anyhow::Result<()> {
    crate::otel::roster_phase("card_presentation", || -> anyhow::Result<()> {
        let subjects = items
            .iter()
            .filter_map(|item| item["id"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        let observations = store.agent_todo_observations_for(&subjects, index)?;
        for item in items {
            let claims = observations.get(item["id"].as_str().unwrap_or_default());
            item["todo"] = client_v0::agent_todo_value(
                claims.and_then(|claims| claims.get("harness.todo.observed")),
                claims.and_then(|claims| claims.get("harness.session-file")),
                item["incarnation_id"].as_str(),
            );
        }
        Ok(())
    })
}

/// The card's `reason` while rival hosts both claim a seat's runtime. A sentence, not a code:
/// clients may print it as it is.
pub(crate) const AUTHORITY_INDETERMINATE_REASON: &str =
    "st cannot tell which host runs this agent; its processes may be running";

/// Live presentation of already folded cards: freshness, delivery presence, subagents, and the
/// agent presentation that reads them (docs/clients/ui-contract.md, Agent).
fn overlay_agent_resources(store: &Store, items: &mut [Value], at: &str) -> anyhow::Result<()> {
    crate::otel::roster_phase("card_presentation", || -> anyhow::Result<()> {
        let local_host = client_host_id(store.origin());
        for item in items.iter_mut() {
            if item.get("updated_at").and_then(Value::as_str) == Some("") {
                item["updated_at"] = Value::String(at.to_owned());
            }
            let source = item
                .as_object_mut()
                .unwrap()
                .remove("_status_source")
                .unwrap_or(Value::Null);
            let launch_since = item.as_object_mut().unwrap().remove("_launch_since_ms").and_then(|ms| ms.as_u64());
            let harness: Option<crate::model::CurrentHarnessView> = serde_json::from_value(source)?;
            // Freshness is approximate presentation. Use the card's already reduced observation;
            // querying diagnostic and local-observation history here made every read grow with it.
            let report_age_ms = harness
                .as_ref()
                .map(|harness| {
                    u64::try_from(client_now_ms().saturating_sub(harness.observed_at_unix_ms))
                        .unwrap_or(u64::MAX)
                });
            let observation = match report_age_ms {
                None => "missing",
                Some(age) if age > 90_000 => "stale",
                Some(_) => "current",
            };
            let reported = item["harness_state"].as_str().map(str::to_owned);
            item["observation"] = json!(observation);
            // A turn can run for hours without a new harness claim, so only a claim that says the
            // seat is waiting for input lapses; a `working` one stays until the harness says otherwise.
            if observation == "stale" && matches!(item["harness_state"].as_str(), Some("ready" | "idle")) {
                item["harness_state"] = json!("indeterminate");
                if item["state"] == "running" {
                    item["state"] = json!("waiting");
                }
            }
            overlay_down(item, launch_since, report_age_ms, reported.as_deref(), client_now_ms() as u64);
            overlay_delivery_presence(item, &local_host);
            // The presentation reads the facts settled just above, so it is settled with them. A
            // row it cannot read goes without one; it never fails the read for every client.
            match st3_ui_model::agents::Facts::deserialize(&*item) {
                Ok(facts) => {
                    item["presentation"] = serde_json::to_value(st3_ui_model::agents::present(&facts))?;
                }
                Err(error) => eprintln!("st3: WARN agent {} has no presentation: {error}", item["id"]),
            }
        }
        overlay_subagents(store, items)?;
        Ok(())
    })
}

/// How long a declared seat may take to show a running session before it is called down.
const LAUNCH_WINDOW_MS: u64 = 120_000;

/// How old a harness report may be and still show that the seat's driver is alive. A driver at an
/// empty prompt reports every five minutes, so its report is older than the 90 s freshness window
/// for most of every interval; two intervals and a minute of slack never call such a seat down.
const HEARTBEAT_MS: u64 = 661_000;

/// Settles a seat whose runtime row says it is not running yet. Every app shows the result and
/// none re-derives it.
///
/// A harness that reported within a heartbeat is alive, whatever a runtime row stuck at `pending`
/// says: a seat can outlive its row. It reads as any other live seat (running, or waiting when its
/// idle report has lapsed). With no such report, a seat with a declaration and no session after its
/// launch window is `down`, since the moment it began waiting, and the steps it holds say so
/// (their progress was written by a holder that is not running).
fn overlay_down(
    item: &mut Value,
    launch_since: Option<u64>,
    report_age_ms: Option<u64>,
    reported: Option<&str>,
    now_ms: u64,
) {
    if !matches!(item["state"].as_str(), Some("desired" | "starting")) {
        return;
    }
    // A `starting` report is written at launch and proves nothing about a session; `ended` and
    // `failed` say the harness is gone. Any other report comes from a driver that is up.
    if report_age_ms.is_some_and(|age| age <= HEARTBEAT_MS)
        && !matches!(reported, None | Some("starting" | "ended" | "failed"))
    {
        let lapsed = report_age_ms.is_some_and(|age| age > 90_000);
        item["state"] = json!(match reported {
            Some("working") => "running",
            Some("ready" | "idle") if !lapsed => "running",
            _ => "waiting",
        });
        return;
    }
    let Some(since) = launch_since.filter(|since| now_ms.saturating_sub(*since) > LAUNCH_WINDOW_MS) else {
        return;
    };
    // A seat that spoke after it launched has had no session since it last spoke, not since it
    // launched: days of earlier reports are not time spent down.
    let since = report_age_ms
        .map(|age| now_ms.saturating_sub(age))
        .filter(|heard| *heard > since)
        .unwrap_or(since);
    let since = client_timestamp(u128::from(since));
    item["state"] = json!("down");
    item["down_since"] = json!(since);
    // A harness report that predates the loss of the session says nothing about now; leaving
    // `working` beside `down` would make the two fields disagree.
    item["harness_state"] = json!("indeterminate");
    // The activity of an earlier incarnation is not activity of a seat that has no session.
    item["silent_since"] = Value::Null;
    if let Some(work) = item["current_work"].as_array_mut() {
        for label in work {
            label["holder_down_since"] = json!(since);
        }
    }
}

/// A seat's latest suspend or resume as client-v0 shows it.
fn client_suspension(suspension: &crate::suspension::Suspension) -> Value {
    json!({
        "action": suspension.action,
        "phase": suspension.phase,
        "operation_id": suspension.operation_id,
        "harness": suspension.harness,
        "native_session_id": suspension.native_session_id,
        "incarnation_id": suspension.incarnation_id,
        "suspended_at": suspension.suspended_at_unix_ms.map(client_timestamp),
        "updated_at": client_timestamp(suspension.updated_at_unix_ms),
        "code": suspension.code,
        "reason": suspension.reason,
        "blocking": suspension.blocking,
        "source_host": suspension.source_host,
        "host": suspension.host,
    })
}

/// Each seat's running subagents: open, with a lease that runs past this read. A lease runs out
/// without a claim, so this is read per request rather than cached with the agents.
fn overlay_subagents(store: &Store, items: &mut [Value]) -> anyhow::Result<()> {
    let mut running = BTreeMap::<String, Vec<Value>>::new();
    let subagents = crate::otel::roster_phase("subagent_leases", || {
        store.running_subagents(client_now_ms() as u64)
    })?;
    for subagent in subagents {
        running
            .entry(subagent.agent.clone())
            .or_default()
            .push(json!({
                "id": subagent.subagent_id,
                "subagent_type": subagent.subagent_type,
                "description": subagent.description,
                "driver": subagent.driver,
                "session_id": subagent.session_id,
                "work_id": subagent.step_run,
                "started_at": (subagent.started_at_unix_ms > 0)
                    .then(|| client_timestamp(u128::from(subagent.started_at_unix_ms))),
                "lease_expires_at": client_timestamp(u128::from(subagent.lease_expires_at_unix_ms)),
            }));
    }
    for item in items {
        let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
        let subagents = running.remove(id).unwrap_or_default();
        item["subagents"] = Value::Array(subagents);
    }
    Ok(())
}

/// Delivery presence is independent of harness readiness: a local native seat waiting on a human
/// still has a transport to assess. A running seat with a stale path becomes `waiting`; an
/// already-waiting seat retains its harness block and ask details.
fn overlay_delivery_presence(item: &mut Value, local_host: &str) {
    const NATIVE_DRIVERS: [&str; 5] = ["claude", "codex", "opencode", "pi", "omp"];
    let Some(driver) = item
        .get("driver")
        .and_then(Value::as_str)
        .filter(|driver| NATIVE_DRIVERS.contains(driver))
        .map(str::to_owned)
    else {
        return;
    };
    let local = item.get("host_id").and_then(Value::as_str) == Some(local_host);
    let live = matches!(
        item.get("state").and_then(Value::as_str),
        Some("running" | "waiting")
    );
    if !local || !live {
        return;
    }
    let Some(recipient) = item.get("id").and_then(Value::as_str) else {
        return;
    };
    let assessment = delivery_presence::assess_current(recipient, &driver, item["incarnation_id"].as_str());
    if assessment.stale() {
        item["state"] = Value::String("waiting".into());
    }
    item["delivery"] = assessment.to_value();
}

#[cfg(test)]
fn client_agent_resources_uncached(
    store: &Store,
    history: bool,
    snapshot_index: u64,
) -> anyhow::Result<Vec<Value>> {
    let status = store.status_for_subject_prefix_at(
        "agent/",
        Some(snapshot_index),
        history,
        crate::store::ClaimsReduction::Summary,
    )?;
    client_agent_resources_from_status(store, history, snapshot_index, None, status)
}

fn client_agent_resources_selected(
    store: &Store,
    history: bool,
    snapshot_index: u64,
    changed: Option<(&BTreeSet<String>, &[Value])>,
) -> anyhow::Result<Vec<Value>> {
    let status = crate::otel::roster_phase("status_fold", || store.agent_card_status_at(
        changed.map(|(subjects, _)| subjects), snapshot_index, history,
    ))?;
    client_agent_resources_from_status(store, history, snapshot_index, changed, status)
}

/// Every agent card at `snapshot_index` of a quiet store, folded as the roster folds them but
/// outside a read snapshot: on a store just opened, nothing is kept yet, as at a daemon start.
/// For load measurements; it returns how many cards it folded.
#[doc(hidden)]
pub fn cold_agent_roster_rebuild(store: &Store, snapshot_index: u64) -> anyhow::Result<usize> {
    let mut cards = client_agent_resources_selected(store, false, snapshot_index, None)?;
    add_agent_todos(store, &mut cards, snapshot_index)?;
    Ok(cards.len())
}

fn client_agent_resources_from_status(
    store: &Store,
    history: bool,
    snapshot_index: u64,
    changed: Option<(&BTreeSet<String>, &[Value])>,
    status: StatusResponse,
) -> anyhow::Result<Vec<Value>> {
    // The delta builder supplies either unchanged cards or freshly diffed queue metadata.
    // Both let the card fold reuse queues without a second fleet scan.
    let retain_queues = changed.is_some_and(|(subjects, previous)| {
        subjects.iter().all(|subject| {
            previous
                .iter()
                .any(|item| item["id"].as_str() == Some(subject.as_str()))
        })
    });
    let queues = crate::otel::roster_phase("queue_selection", || -> anyhow::Result<_> {
        let queues = if retain_queues {
            BTreeMap::new()
        } else {
            store.agent_work_queues()?
        };
        let queued_steps = queues
            .values()
            .flat_map(|queue| {
                queue
                    .current_work_ids
                    .iter()
                    .chain(queue.next_work_id.iter())
                    .chain(queue.upcoming_work_ids.iter())
                    .cloned()
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let labels = store.step_labels(&queued_steps)?;
        Ok(CardQueues {
            retained: changed.filter(|_| retain_queues).map(|(_, previous)| previous),
            queues,
            labels,
        })
    })?;
    crate::otel::roster_phase("card_presentation", || {
        client_agent_cards_presented(store, history, snapshot_index, status, &queues, changed.map(|(names, _)| names))
    })
}

/// Queue fields for folded cards: retained from unchanged previous cards, or freshly selected.
struct CardQueues<'a> {
    retained: Option<&'a [Value]>,
    queues: BTreeMap<String, crate::store::AgentWorkQueue>,
    labels: BTreeMap<String, crate::store::StepLabel>,
}

fn client_agent_cards_presented(
    store: &Store,
    history: bool,
    snapshot_index: u64,
    status: StatusResponse,
    queues: &CardQueues<'_>,
    selected: Option<&BTreeSet<String>>,
) -> anyhow::Result<Vec<Value>> {
    let agent_subjects = status
        .subjects
        .iter()
        .filter(|subject| {
            (subject.subject.starts_with("agent/") || subject.kind.as_deref() == Some("agent"))
                && (history || subject.projection.layer == "current")
        })
        .map(|subject| subject.subject.clone())
        .collect::<Vec<_>>();
    // Declarations, usage and faults of the listed agents only, not of every subject.
    let desired_agents = store
        .desired_subjects_named(&agent_subjects)?
        .into_iter()
        .filter_map(|desired| {
            let member = desired.member?;
            let checkout = crate::checkout::Checkout::from_desired(&desired.desired).map(|checkout| json!({
                "repository": checkout.repository, "base": checkout.base, "branch": checkout.branch
            }));
            Some((desired.subject, (client_host_id(&member.host), member.workspace, checkout)))
        })
        .collect::<BTreeMap<_, _>>();
    let usage_summaries = store.usage_summaries_at(&agent_subjects, Some(snapshot_index))?;
    let member_faults = store.member_reconcile_faults_for(&agent_subjects, snapshot_index)?;
    let mailbox_faults = match store.mailbox_faults_for(&agent_subjects, snapshot_index) {
        Ok(faults) => Some(faults),
        Err(error) if error.downcast_ref::<St3Error>().is_some_and(|error| error.code == "read-deadline") => None,
        Err(error) => return Err(error),
    };
    store.record_agent_roster_fault_read(history, &agent_subjects, mailbox_faults.is_none(), selected);
    // An optional fault read cannot discard current status, usage, queues or other card fields.
    // Retain even a known null fault; a card never published before states that it is unknown.
    let previous_faults = if mailbox_faults.is_none() {
        store.previous_agent_faults(snapshot_index, history, &agent_subjects)
    } else {
        BTreeMap::new()
    };
    let stop_deferral_faults = store.stop_deferral_faults_for(&agent_subjects, snapshot_index)?;
    // Harnesses, activity, and who can have a suspension or rollout, for many agents at once.
    // Each status names the actual-state claim it selected, the newest runtime observation
    // whenever the agent has one.
    let actual_claims = status
        .subjects
        .iter()
        .map(|subject| (subject.subject.clone(), subject.actual_claim.clone()))
        .collect();
    let mut card_reads = store.agent_card_reads(&agent_subjects, snapshot_index, &actual_claims)?;
    // Cards without a harness need only their actual claim's acceptance time, not its body.
    // Keep the existing per-claim fallback if the bulk metadata read cannot be completed.
    let actual_claim_times = store.claim_acceptance_times(
        &status
            .subjects
            .iter()
            .flat_map(|subject| {
                // A seat that never reported has only its declaration to say how long it has waited.
                subject.actual_claim.as_deref().into_iter().chain(subject.desired_token.as_deref())
            })
            .collect::<Vec<_>>(),
    );
    let mut agents = status
        .subjects
        .into_iter()
        .filter(|subject| {
            subject.subject.starts_with("agent/") || subject.kind.as_deref() == Some("agent")
        })
        .filter(|subject| history || subject.projection.layer == "current")
        .map(|mut subject| -> anyhow::Result<(String, Value)> {
            subject.harness = card_reads.take_harness(store, &subject.subject)?;
            let member_fault = member_faults.get(&subject.subject);
            let mailbox_fault = mailbox_faults.as_ref().and_then(|faults| faults.get(&subject.subject));
            let fault = member_fault.or(mailbox_fault)
                .or_else(|| stop_deferral_faults.get(&subject.subject));
            let fields = subject
                .actual
                .as_ref()
                .map(|actual| actual.get("fields").unwrap_or(actual));
            let observed = fields
                .and_then(|fields| fields.get("status"))
                .and_then(Value::as_str);
            let declared_provider = subject.desired.as_ref().and_then(desired_harness_driver);
            subject.harness = agent_harness::eligible(
                declared_provider.as_deref(),
                fields.and_then(|fields| fields.get("incarnation_id")).and_then(Value::as_str),
                subject.harness.take(),
            );
            subject.harness = agent_harness::availability(subject.harness.take(), mailbox_fault.map(String::as_str));
            let driver = declared_provider.or_else(|| subject.harness.as_ref().and_then(|harness| harness.driver.clone()));
            let harness_state = subject.harness.as_ref().map(|harness| harness.state.clone());
            let last_activity_at = card_reads.last_activity_at(
                store,
                &subject.subject,
                subject
                    .harness
                    .as_ref()
                    .map(|harness| harness.incarnation_id.as_str()),
            )?;
            let silent_since = if harness_state.as_deref() == Some("working") {
                let working_since = match subject.harness.as_ref() {
                    Some(harness) => store
                        .agent_working_since(
                            &subject.subject,
                            &harness.incarnation_id,
                            snapshot_index,
                        )?
                        .or(Some(harness.observed_at_unix_ms)),
                    None => None,
                };
                match (last_activity_at, working_since) {
                    (Some(activity), Some(start)) => Some(activity.max(start)),
                    (Some(activity), None) => Some(activity),
                    (None, start) => start,
                }
            } else {
                None
            };
            // A live wrapper is necessary but not sufficient for a running agent. Native
            // harnesses only become running once the current runtime incarnation has produced a
            // ready observation; an ended or indeterminate harness must never be painted green
            // merely because its wrapper process still has a running observation.
            let state = match (
                observed,
                driver.as_deref(),
                harness_state.as_deref(),
                subject.reachability.as_str(),
            ) {
                (Some("running" | "ready" | "working" | "idle"), _, _, reachability)
                    if reachability != "reachable" =>
                {
                    "waiting"
                }
                (
                    Some("running" | "ready" | "working" | "idle"),
                    Some(_),
                    Some("ready" | "working" | "idle"),
                    _,
                ) => {
                    if subject.harness.as_ref().is_some_and(|harness| {
                        harness.blocked_on.as_deref() == Some("human")
                    }) {
                        "waiting"
                    } else {
                        "running"
                    }
                }
                (
                    Some("running" | "ready" | "working" | "idle"),
                    Some(_),
                    Some("ended" | "failed"),
                    _,
                ) => "failed",
                // A harness fenced at a login or trust prompt waits on a person.
                (
                    Some("running" | "ready" | "working" | "idle"),
                    Some(_),
                    Some("indeterminate" | "unknown" | "unauthenticated" | "needs-login" | "blocked"),
                    _,
                ) => "waiting",
                (Some("running" | "ready" | "working" | "idle"), Some(_), _, _) => "starting",
                (Some("running" | "ready" | "working" | "idle"), None, _, _) => "running",
                (Some("starting" | "pending"), _, _, _) => "starting",
                (Some("waiting"), _, _, _) => "waiting",
                (Some("failed"), _, _, _) => "failed",
                (Some("stopped" | "exited" | "absent"), _, _, _) => "stopped",
                _ if subject.desired.is_some() => "desired",
                _ => "stopped",
            };
            // When a seat that has no running session began waiting for one.
            let launch_since_ms = (matches!(state, "desired" | "starting")
                && !matches!(observed, Some("running" | "ready" | "working" | "idle")))
            .then(|| {
                let claim_time = |claim: &str| match &actual_claim_times {
                    Ok(times) => times.get(claim).copied(),
                    Err(_) => store.claim_by_id(claim).ok().flatten().map(|claim| claim.accepted_at_unix_ms),
                };
                subject
                    .actual_claim
                    .as_deref()
                    .and_then(claim_time)
                    .or_else(|| subject.desired_token.as_deref().and_then(claim_time))
            })
            .flatten();
            let handoff = subject.desired_token.as_deref()
                .map(|token| crate::placement::handoff(store, &subject.subject, token, snapshot_index))
                .transpose()?.flatten();
            let moving = handoff.as_ref().is_some_and(|h| h.phase != "running");
            let state = if member_fault.is_some() { "failed" } else if mailbox_fault.is_some() || moving { "waiting" } else { state };
            // A session is observed but st cannot tell which host owns it (rival origins claim
            // the runtime): the seat is neither running nor stopped as far as st can say.
            let authority_unknown = member_fault.is_none()
                && mailbox_fault.is_none()
                && !moving
                && subject.reachability == "indeterminate"
                && matches!(observed, Some("running" | "ready" | "working" | "idle"));
            let suspension = if card_reads.may_have_suspension(&subject.subject) {
                crate::suspension::current(store, &subject.subject)?
            } else {
                None
            };
            // A suspended seat has no process by design: it is neither stopped nor failed.
            let state = match suspension.as_ref().map(|item| item.phase.as_str()) {
                Some("suspended") if fault.is_none() => "suspended",
                Some("snapshotting" | "fencing-source" | "transferring" | "restoring") if state == "stopped" => "suspended",
                _ => state,
            };
            let state = if subject.desired.as_ref().is_some_and(|desired| desired.get("retirement").is_some()) {
                "retired"
            } else { state };
            let runtime_id = fields
                .and_then(|fields| fields.get("runtime_id"))
                .and_then(Value::as_str);
            let runtime_ids = runtime_id
                .map(|runtime| vec![format!("runtime/{runtime}")])
                .unwrap_or_default();
            let incarnation_id = fields
                .filter(|_| !moving)
                .and_then(|fields| fields.get("incarnation_id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let current_session_id = incarnation_id
                .as_deref()
                .or(runtime_id)
                .map(|identity| managed_session_id(&subject.subject, identity));
            let updated_at = subject
                .harness
                .as_ref()
                .map(|harness| client_timestamp(harness.observed_at_unix_ms))
                .or_else(|| {
                    subject.actual_claim.as_deref().and_then(|claim| {
                        match &actual_claim_times {
                            Ok(times) => times.get(claim).copied(),
                            Err(_) => store
                                .claim_by_id(claim)
                                .ok()
                                .flatten()
                                .map(|claim| claim.accepted_at_unix_ms),
                        }
                        .map(client_timestamp)
                    })
                })
                .unwrap_or_default();
            let name = crate::model::effective_agent_name(
                &subject.subject, subject.desired.as_ref(),
            ).to_owned();
            let revision = subject
                .desired_revision
                .clone()
                .or_else(|| subject.claims_summary.latest.clone())
                .unwrap_or_else(|| format!("agent/{}", subject.subject));
            let queue = queues.queues
                .get(&subject.subject)
                .cloned()
                .unwrap_or_default();
            let usage = usage_summaries.get(&subject.subject);
            let mut value = json!({
                "id": subject.subject,
                "kind": "agent",
                "revision": revision,
                "updated_at": updated_at,
                "name": name,
                "state": state,
                "reachability": subject.reachability,
                "runtime_ids": runtime_ids,
                "owner_run_id": subject.owner_run,
                "driver": driver,
                "harness_state": harness_state.as_deref().map(|state| if state == "needs-login" { "unauthenticated" } else { state }),
                "harness_error_state": (harness_state.as_deref() == Some("needs-login")).then_some("needs-login"),
                "since": subject.harness.as_ref().map(|harness| client_timestamp(harness.since_unix_ms)),
                "_status_source": subject.harness,
                "_launch_since_ms": launch_since_ms.map(|ms| ms as u64),
                "blocked_on": subject.harness.as_ref().and_then(|harness| harness.blocked_on.as_deref()),
                "ask": subject.harness.as_ref().and_then(|harness| harness.ask.as_deref()),
                "reason": subject.harness.as_ref().and_then(|harness| harness.reason.as_deref())
                    .or_else(|| authority_unknown.then_some(AUTHORITY_INDETERMINATE_REASON)),
                // What the seat is doing that is neither work nor a wait on anyone: a status,
                // never an alert.
                "activity": subject
                    .harness
                    .as_ref()
                    .filter(|harness| harness.reason.as_deref() == Some("compaction"))
                    .map(|_| "compacting"),
                "host_id": desired_agents.get(&subject.subject).map(|(host, _, _)| host),
                "workspace": desired_agents.get(&subject.subject).map(|(_, workspace, _)| workspace),
                "checkout": desired_agents.get(&subject.subject).and_then(|(_, _, checkout)| checkout.as_ref()),
                "last_activity_at": last_activity_at.map(client_timestamp),
                "silent_since": silent_since.map(client_timestamp),
                "fault": fault,
                "incarnation_id": incarnation_id,
                "current_session_id": current_session_id,
                "usage": usage,
                "under": subject.under.into_iter().map(|relationship| json!({
                    "agent_id": relationship.agent,
                    "reason": relationship.reason
                })).collect::<Vec<_>>(),
                "operational": subject.projection,
                "suspension": suspension.as_ref().map(client_suspension),
                "handoff": handoff,
                "rollout": if card_reads.may_have_rollout(&subject.subject) {
                    crate::rollout::status(store, &subject.subject)?
                } else {
                    None
                },
            });
            if mailbox_faults.is_none() && fault.is_none() {
                value["fault"] = previous_faults.get(&subject.subject).cloned()
                    .unwrap_or_else(|| json!("fault status unknown"));
            }
            if let Some(lifecycle) = crate::model::declared_agent_lifecycle(subject.desired.as_ref()) {
                value["lifecycle"] = json!(lifecycle);
            }
            if let Some(previous) = queues.retained
                && let Some(old) = previous.iter().find(|item| item["id"] == value["id"]) {
                for field in AGENT_QUEUE_FIELDS {
                    value[field] = old[field].clone();
                }
            } else if let Value::Object(fields) = agent_queue_fields(&queue, &queues.labels) {
                value.as_object_mut().unwrap().extend(fields);
            }
            Ok((name, value))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    agents.sort_by(|(left_name, left), (right_name, right)| {
        left_name
            .cmp(right_name)
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(agents.into_iter().map(|(_, value)| value).collect())
}

fn desired_harness_driver(desired: &Value) -> Option<String> {
    desired
        .get("children")?
        .as_array()?
        .iter()
        .find(|child| child.get("name").and_then(Value::as_str) == Some("harness"))?
        .get("arguments")?
        .as_array()?
        .first()?
        .as_str()
        .map(str::to_owned)
}

fn managed_session_id(owner: &str, identity: &str) -> String {
    let digest = hex::encode(Sha256::digest(format!("{owner}:{identity}").as_bytes()));
    format!("session/{}", &digest[..24])
}

type ManagedSessionOwner = (String, Option<String>, Option<String>);

fn managed_session_owner_at(
    store: &Store,
    snapshot_index: u64,
    session_id: &str,
) -> anyhow::Result<Option<ManagedSessionOwner>> {
    crate::performance::task("conversation/owner", || {
        let owners = store.conversation_owners_at(snapshot_index)?;
        for owner in owners.values() {
            let Some(identity) = owner.incarnation.as_deref().or(owner.runtime.as_deref()) else {
                continue;
            };
            if managed_session_id(&owner.subject, identity) == session_id {
                return Ok(Some((owner.subject.clone(), owner.incarnation.clone(), owner.origin.clone())));
            }
        }
        Ok(None)
    })
}

fn managed_session_owner_for_subject_at(
    store: &Store,
    snapshot_index: u64,
    session_id: &str,
    owner: &str,
) -> anyhow::Result<Option<ManagedSessionOwner>> {
    crate::performance::task("conversation/owner", || {
        let owners = store.conversation_owners_at(snapshot_index)?;
        let Some(owner) = owners.get(owner) else {
            return Ok(None);
        };
        let Some(identity) = owner.incarnation.as_deref().or(owner.runtime.as_deref()) else {
            return Ok(None);
        };
        if managed_session_id(&owner.subject, identity) != session_id {
            return Ok(None);
        }
        Ok(Some((owner.subject.clone(), owner.incarnation.clone(), owner.origin.clone())))
    })
}


fn client_session_resources(
    store: &Arc<Store>,
    history: bool,
    at: &str,
    snapshot_index: u64,
    native_session_home: Option<&Path>,
    native_only: bool,
) -> anyhow::Result<Vec<Value>> {
    type ManagedSessions = (Arc<Store>, u64, bool, Vec<Value>, BTreeSet<String>);
    static MANAGED_CACHE: OnceLock<Mutex<Option<ManagedSessions>>> = OnceLock::new();
    let cache = MANAGED_CACHE.get_or_init(|| Mutex::new(None));
    let cached = cache
        .lock()
        .expect("managed session cache mutex poisoned")
        .as_ref()
        .filter(|(cached_store, index, all, _, _)| {
            Arc::ptr_eq(cached_store, store) && *index == snapshot_index && *all == history
        })
        .map(|(_, _, _, sessions, managed)| (sessions.clone(), managed.clone()));
    let (mut sessions, managed_native_sessions) = if native_only {
        (
            Vec::new(),
            managed_native_session_ids(store, snapshot_index)?,
        )
    } else if let Some(cached) = cached {
        cached
    } else {
        let fresh = managed_session_resources(store, history, at, snapshot_index)?;
        *cache.lock().expect("managed session cache mutex poisoned") = Some((
            store.clone(),
            snapshot_index,
            history,
            fresh.0.clone(),
            fresh.1.clone(),
        ));
        fresh
    };
    let mut external = crate::external_sessions::discover(native_session_home, history)?;
    external
        .sessions
        .retain(|session| !managed_native_sessions.contains(&session.native_id));
    for session in external.sessions {
        sessions.push(external_session_resource(session));
    }
    for unresolved in external.unresolved_processes {
        sessions.push(unresolved_session_resource(unresolved, at));
    }
    sessions.sort_by(|left, right| {
        right["updated_at"]
            .as_str()
            .cmp(&left["updated_at"].as_str())
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(sessions)
}

fn external_session_resource(session: crate::external_sessions::ExternalSession) -> Value {
    let running = session.process.is_some();
    let process = session.process.as_ref().map(|process| {
        json!({
            "pid": process.pid,
            "started_at": crate::external_sessions::timestamp(process.started_at_unix_ms),
            "fingerprint": process.fingerprint,
            "exact_session": process.exact_session
        })
    });
    json!({
        "id": session.id,
        "kind": "session",
        "revision": session.revision,
        "updated_at": crate::external_sessions::timestamp(session.updated_at_unix_ms),
        "owner_id": format!("external-session/{}/{}", session.driver.as_str(), session.native_id),
        "state": if running { "running" } else { "completed" },
        "started_at": crate::external_sessions::timestamp(session.started_at_unix_ms),
        "ended_at": if running { None } else { Some(crate::external_sessions::timestamp(session.updated_at_unix_ms)) },
        "timeline_cursor": format!("timeline-cursor/{}/latest", session.id.trim_start_matches("session/")),
        "usage": null,
        "managed": false,
        "driver": session.driver.as_str(),
        "native_session_id": session.native_id,
        "workspace": session.cwd.map(|path| path.display().to_string()),
        "title": session.title,
        "importable": true,
        "import_reason": null,
        "process": process
    })
}

fn unresolved_session_resource(
    unresolved: crate::external_sessions::UnresolvedProcess,
    at: &str,
) -> Value {
    json!({
        "id": unresolved.id,
        "kind": "session",
        "revision": unresolved.revision,
        "updated_at": crate::external_sessions::timestamp(snapshot_time_ms(at)),
        "owner_id": format!("external-process/{}/{}", unresolved.driver.as_str(), unresolved.process.pid),
        "state": "running",
        "started_at": crate::external_sessions::timestamp(unresolved.process.started_at_unix_ms),
        "ended_at": null,
        "timeline_cursor": format!("timeline-cursor/process-{}/latest", unresolved.process.pid),
        "usage": null,
        "managed": false,
        "driver": unresolved.driver.as_str(),
        "native_session_id": null,
        "workspace": unresolved.process.cwd.map(|path| path.display().to_string()),
        "title": null,
        "importable": false,
        "import_reason": "a running harness in this workspace does not expose its exact native session ID; select a saved session and explicitly confirm this PID before takeover",
        "process": {
            "pid": unresolved.process.pid,
            "started_at": crate::external_sessions::timestamp(unresolved.process.started_at_unix_ms),
            "fingerprint": unresolved.process.fingerprint,
            "exact_session": false
        }
    })
}

fn managed_session_resources(
    store: &Store,
    history: bool,
    at: &str,
    snapshot_index: u64,
) -> anyhow::Result<(Vec<Value>, BTreeSet<String>)> {
    let status = store.status_for_subject_prefix_at(
        "agent/",
        Some(snapshot_index),
        history,
        crate::store::ClaimsReduction::Summary,
    )?;
    let mut sessions = Vec::new();
    for subject in status
        .subjects
        .into_iter()
        .filter(|subject| {
            subject.subject.starts_with("agent/") || subject.kind.as_deref() == Some("agent")
        })
        .filter(|subject| history || subject.projection.actionable)
    {
        let fields = subject
            .actual
            .as_ref()
            .map(|actual| actual.get("fields").unwrap_or(actual));
        let incarnation = fields
            .and_then(|fields| fields.get("incarnation_id"))
            .and_then(Value::as_str)
            .or(subject.projection.runtime_incarnation.as_deref());
        let runtime = fields
            .and_then(|fields| fields.get("runtime_id"))
            .and_then(Value::as_str);
        let Some(identity) = incarnation.or(runtime) else {
            continue;
        };
        let session_id = managed_session_id(&subject.subject, identity);
        let timeline_cursor = format!(
            "timeline-cursor/{}/0",
            session_id.trim_start_matches("session/")
        );
        let observed = fields
            .and_then(|fields| fields.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("waiting");
        let state = match observed {
            "running" | "ready" | "working" | "idle" => "running",
            "starting" | "pending" | "waiting" => "waiting",
            "failed" => "failed",
            "cancelled" => "cancelled",
            "stopped" | "exited" | "absent" => "completed",
            _ => "waiting",
        };
        // Keep the old page bound only for runtime-only identities, which scan subject history.
        // Incarnation spans are index-bounded and must include incarnations after that page.
        // The summary's boundary is set exactly while the count passes the window.
        let through_claim = subject
            .claims_summary
            .session_boundary
            .as_deref()
            .filter(|_| incarnation.is_none());
        let mut accepted_times = store
            .runtime_claim_span_at(
                &subject.subject,
                incarnation,
                runtime,
                snapshot_index,
                through_claim,
            )?
            .map(|(first, last)| vec![first, last])
            .unwrap_or_default();
        if let Some(incarnation) = incarnation
            && let Some(observed) =
                store.latest_local_timeline_at(&subject.subject, incarnation, snapshot_index)?
        {
            accepted_times.push(observed);
        }
        let started = fields
            .and_then(|fields| fields.get("started_at_unix_ms"))
            .and_then(|value| value.as_u64().map(u128::from))
            .or_else(|| accepted_times.iter().min().copied());
        // Client v0 requires a timestamp. Without session evidence, the actual observation's
        // acceptance time is the closest durable date, not the time someone reads the session.
        let started = match started {
            Some(started) => client_timestamp(started),
            None => subject
                .actual_claim
                .as_deref()
                .or(subject.claims_summary.first.as_deref())
                .map(|id| store.claim_by_id(id))
                .transpose()?
                .flatten()
                .map_or_else(
                    || client_timestamp(0),
                    |claim| client_timestamp(claim.accepted_at_unix_ms),
                ),
        };
        let updated = accepted_times
            .iter()
            .max()
            .copied()
            .map(client_timestamp)
            .unwrap_or_else(|| at.to_owned());
        let usage = store.usage_summary_at(&subject.subject, incarnation, Some(snapshot_index))?;
        sessions.push(json!({
            "id": session_id,
            "kind": "session",
            "revision": identity,
            "updated_at": updated.clone(),
            "owner_id": subject.subject,
            "state": state,
            "started_at": started,
            "ended_at": if state == "completed" || state == "failed" || state == "cancelled" { Some(updated) } else { None },
            "timeline_cursor": timeline_cursor,
            "runtime_incarnation": incarnation,
            "usage": usage,
            "operational": subject.projection
        }));
    }
    let managed_native_sessions = managed_native_session_ids(store, snapshot_index)?;
    Ok((sessions, managed_native_sessions))
}

fn managed_native_session_ids(
    store: &Store,
    snapshot_index: u64,
) -> anyhow::Result<BTreeSet<String>> {
    Ok(store
        .claims_for_kind_at(
            "harness.session-file",
            snapshot_index.checked_add(1),
            true,
            10_000,
        )?
        .claims
        .into_iter()
        .filter_map(|claim| {
            claim
                .body
                .get("fields")
                .unwrap_or(&claim.body)
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>())
}

fn snapshot_time_ms(timestamp: &str) -> u128 {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|value| value.timestamp_millis().max(0) as u128)
        .unwrap_or_default()
}

/// Describe the current source targets beside a failure.
fn insert_attention_target_states(
    store: &Store,
    resource: &mut serde_json::Map<String, Value>,
    targets: &[String],
) -> anyhow::Result<()> {
    let states = store
        .attention_target_states(targets)?
        .into_iter()
        .map(|state| {
            let mut value = json!({ "id": state.id, "state": state.state });
            if let Some(since) = state.since_unix_ms {
                value["since"] = Value::String(client_timestamp(since));
            }
            value
        })
        .collect::<Vec<_>>();
    if !states.is_empty() {
        resource.insert("target_states".into(), Value::Array(states));
    }
    Ok(())
}

fn client_attention_resources(
    store: &Store,
    person: Option<&str>,
    _history: bool,
) -> anyhow::Result<Vec<Value>> {
    client_attention_resources_at(store, person, _history, client_now_ms())
}

pub(crate) fn client_attention_resources_at(
    store: &Store,
    person: Option<&str>,
    _history: bool,
    at_unix_ms: u128,
) -> anyhow::Result<Vec<Value>> {
    let current = crate::performance::task("attention_collection/snapshot", || {
        store.attention_snapshot(person, at_unix_ms)
    })?;
    client_attention_resources_from_items(store, current)
}

fn client_attention_resources_fenced_at(
    store: &Store,
    person: Option<&str>,
    at_unix_ms: u128,
) -> anyhow::Result<(Vec<Value>, Option<u128>)> {
    let (current, valid_until) = crate::performance::task("attention_collection/snapshot", || {
        store.attention_collection_snapshot(person, at_unix_ms)
    })?;
    Ok((client_attention_resources_from_items(store, current)?, valid_until))
}

fn client_attention_resources_from_items(
    store: &Store,
    current: Vec<crate::model::AttentionItemView>,
) -> anyhow::Result<Vec<Value>> {
    let mut resources = Vec::new();
    for item in current {
        let id = client_attention_id(&item.subject, &item.person, &item.episode)?;
        let alert = item.is_alert();
        let conversations = item.conversations();
        let mut resource = json!({
            "id": id, "kind": "attention", "attention_kind": item.kind,
            "source_id": item.subject, "source_kind": item.kind, "episode": item.episode,
            "person_id": item.person, "revision": item.episode,
            "updated_at": client_timestamp(item.requested_at_unix_ms),
            "title": item.title, "detail": item.detail, "priority": item.priority,
            "state": "open", "requested_at": client_timestamp(item.requested_at_unix_ms),
            "targets": item.targets, "actions": client_attention_actions(&item.kind, item.review_mode.as_deref()),
            "operational": {"layer": "current", "actionable": true, "reasons": []},
            "alert": alert,
        });
        if !conversations.is_empty() {
            resource["conversation_ids"] = json!(conversations);
        }
        for (name, value) in [
            ("conversation_id", item.conversation),
            ("requester_id", item.requester_id),
            ("launch_id", item.launch_id),
            ("variant_id", item.variant_id),
            ("message_id", item.message_id),
            ("mission_id", item.mission),
            ("mission_run_id", item.mission_run),
            ("step_run_id", item.step),
        ] {
            if let Some(value) = value {
                resource[name] = json!(value);
            }
        }
        if let Some(mode) = item.review_mode {
            resource["review_mode"] = json!(mode);
        }
        // A prompt st can answer offers its answers as one typed action.
        if !item.answers.is_empty() {
            resource["actions"] = json!(["prompt.respond"]);
            // `call_id` names the exact call a native prompt asks about, or null when its
            // harness did not name it: a client offers to allow only that call, so a null one is
            // allowed in the terminal. Daemons before it omit the field.
            resource["action_parameters"] = json!({"prompt.respond": {
                "target_id": item.subject, "episode": item.episode, "answers": item.answers,
                "call_id": item.prompt_call}});
        }
        if item.kind.starts_with("custom.")
            && let Some(source) = store.custom_subject(&item.subject)?
        {
            resource["source_kind"] = json!("custom");
            resource["revision"] = source["revision"].clone();
            resource["custom_form"] = source["attention"]["reply"].clone();
            resource["action_parameters"] = json!({"custom.reply":{"target_id":item.subject,"registration":source["registration"],"revision":source["revision"],"episode":item.episode}});
        }
        if item.kind == "person-step" {
            resource["action_parameters"] =
                json!({"work.done": {"target_id": item.subject, "episode": item.episode}});
            // An ask a mission step made carries its mission and the step that waits on it.
            if let Some(context) = store.person_ask_context(&item.subject)? {
                if let Some(mission) = context["mission_id"].as_str() {
                    resource["mission_id"] = json!(mission);
                }
                resource["blocked"] = context["blocked"].clone();
            }
            match item.request {
                // An update asks nothing, so it is not a `request`: a client that predates
                // updates shows a free-text card, and any response to it reads it.
                Some(update) if update["type"] == "update" => {
                    resource["action_parameters"]["work.done"]["summary"] = json!("Read");
                    resource["action_parameters"]["work.done"]["answer"] = json!({"id": "read"});
                    resource["update"] = update;
                }
                Some(request) => resource["request"] = crate::person_request::client_view(&request),
                None => {}
            }
        }
        if item.kind == "fault" {
            resource["what"] = json!(item.title);
            resource["because"] = json!(item.detail);
            let targets = vec![item.subject.clone()];
            insert_attention_target_states(store, resource.as_object_mut().unwrap(), &targets)?;
        }
        resources.push(resource);
    }
    Ok(resources)
}

// Older client-v0 models use closed enums for attention kinds and action names.
fn client_attention_compatibility(items: &mut [Value], custom_forms: bool) {
    if custom_forms {
        return;
    }
    for item in items {
        if item["kind"] == "attention" && item["source_kind"] == "custom" {
            item["custom_attention_kind"] = item["attention_kind"].clone();
            item["attention_kind"] = json!("agent-request");
            item["actions"] = json!([]);
            let p = &item["action_parameters"]["custom.reply"];
            let command = [
                "st", "subject", "reply", item["source_id"].as_str().unwrap_or_default(),
                "--registration", p["registration"].as_str().unwrap_or_default(),
                "--revision", p["revision"].as_str().unwrap_or_default(),
                "--episode", p["episode"].as_str().unwrap_or_default(),
                "--fields-file", "REPLY.json", "--idempotency-key", "REPLY-KEY",
                "--as", item["person_id"].as_str().unwrap_or_default(),
            ].map(crate::gate_report::shell_quote).join(" ");
            item["detail"] = json!(format!("{}\n\nReply with {command}", item["detail"].as_str().unwrap_or_default()));
        }
    }
}

fn client_attention_resources_with_previews(
    state: &AppState,
    person: Option<&str>,
    history: bool,
) -> anyhow::Result<Vec<Value>> {
    let mut items = client_attention_resources(&state.store, person, history)?;
    for item in &mut items {
        if item["attention_kind"] != "launch-approval" {
            continue;
        }
        let Some(launch_id) = item["launch_id"]
            .as_str()
            .and_then(|id| id.strip_prefix("launch/"))
        else {
            continue;
        };
        let Some(variant_id) = item["variant_id"].as_str() else {
            continue;
        };
        let Some(session) = state.store.planning_session(launch_id)? else {
            continue;
        };
        if let Some(variant) = client_launch_variant_resources(state, &session)?
            .into_iter()
            .find(|variant| variant["id"] == variant_id)
        {
            item["preview"] = variant["preview"].clone();
            item["preview_token"] = variant["preview_token"].clone();
        }
    }
    Ok(items)
}

/// A message's attachment as clients see it. `blob` names it in `message.send` and
/// `GET /v1/client/blobs/{sha256}`; read it with the message that carries it.
fn client_attachment(attachment: &crate::model::MessageAttachment) -> Value {
    json!({
        "blob": format!("blob/{}", attachment.sha256),
        "sha256": attachment.sha256,
        "media_type": attachment.media_type,
        "name": attachment.name,
        "size": attachment.size,
        "origin": attachment.origin,
    })
}

fn client_message_resources(
    store: &Store,
    person: Option<&str>,
    history: bool,
    peer: Option<&str>,
) -> anyhow::Result<Vec<Value>> {
    let current = store.operational_messages(person, false)?;
    let current_ids = current
        .iter()
        .map(|message| message.subject.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let messages = if history {
        store.operational_messages(person, true)?
    } else {
        current
    };
    let mut resources = Vec::new();
    for message in messages {
        if peer.is_some_and(|peer| message.from != peer && message.to != peer) {
            continue;
        }
        let claims = store.claims_for(&message.subject, None)?;
        let first = claims.first();
        let last = claims.last();
        let sent_at = first
            .map(|claim| claim.accepted_at_unix_ms)
            .unwrap_or_default();
        let updated_at = last
            .map(|claim| claim.accepted_at_unix_ms)
            .unwrap_or(sent_at);
        let session_id = first
            .and_then(|claim| {
                claim
                    .body
                    .get("fields")
                    .unwrap_or(&claim.body)
                    .get("session_id")
            })
            .cloned()
            .unwrap_or(Value::Null);
        let mut reasons = Vec::new();
        if message.status == "closed" {
            reasons.push("closed");
        } else if !current_ids.contains(&message.subject) {
            reasons.push("superseded");
        }
        let current = reasons.is_empty();
        let mut resource = json!({
            "id": message.subject,
            "kind": "message",
            "revision": last.map(|claim| claim.id.as_str()).unwrap_or("message/unknown"),
            "updated_at": client_timestamp(updated_at),
            "from": message.from,
            "to": message.to,
            "title": message.title,
            "content": message.content,
            "state": message.status,
            "delivery": message_delivery_for(store, &message, sent_at, client_now_ms())?,
            "sent_at": client_timestamp(sent_at),
            "session_id": session_id,
            "in_reply_to": message.in_reply_to,
            "tags": message.tags,
            "attachments": message.attachments.iter().map(client_attachment).collect::<Vec<_>>(),
            "operational": { "layer": if current { "current" } else { "history" }, "actionable": current, "reasons": reasons }
        });
        // Only a long message names its body, so every other resource reads as it always has.
        if let (Some(reference), Some(bytes)) = (&message.body_ref, message.body_bytes) {
            resource["body_ref"] = json!(reference);
            resource["body_bytes"] = json!(bytes);
        }
        resources.push(resource);
    }
    resources.sort_by(|left, right| {
        right["sent_at"]
            .as_str()
            .cmp(&left["sent_at"].as_str())
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(resources)
}

/// Native acceptance and recipient read are separate evidence. A pending read never expires,
/// and its age remains visible even on the sender's node, which cannot inspect a remote process.
fn message_delivery_value(store: &Store, to: &str, status: &str, sent_at: u128, now: u128) -> anyhow::Result<Value> {
    let read = matches!(status, "read" | "closed");
    let age_ms = now.saturating_sub(sent_at);
    let path = delivery_presence::known(to);
    let blocked = !read && path.as_ref().is_some_and(|path| path.stale());
    // Only mail still outside the native transport can be held. Use observed evidence,
    // not work-derived readiness; the store fold fences it to the running incarnation.
    let held = if matches!(status, "sent" | "staged") {
        let index = store.index()?;
        let harness = store.observed_harness_at(to, index)?;
        harness.as_ref().is_some_and(|harness| harness.blocked_on.as_deref() == Some("human"))
            && store.seat_observation_at(to, harness.as_ref(), index, now)? == "current"
    } else {
        false
    };
    let reason = if read {
        "the recipient has read the message"
    } else if held {
        "held outside the native harness while the recipient waits for a human answer or approval"
    } else if blocked {
        path.as_ref()
            .and_then(|path| path.reason.as_deref())
            .unwrap_or("the delivery path is stale")
    } else {
        match status {
            "sent" => "waiting for delivery; the durable message remains queued",
            "staged" => "waiting for native handoff; failed handoffs retry automatically",
            "delivered" => "the native transport accepted the message; waiting for recipient read",
            _ => "waiting for recipient read",
        }
    };
    Ok(json!({
        "state": if read { "read" } else if held || blocked || age_ms > 10_000 { "waiting" } else { "pending" },
        "phase": status, "reason": reason, "age_ms": age_ms,
        "recipient_delivery": path.map(|path| path.to_value()),
    }))
}

fn message_delivery_for(store: &Store, message: &MessageView, sent_at: u128, now: u128) -> anyhow::Result<Value> {
    let mut delivery = message_delivery_value(store, &message.to, &message.status, sent_at, now)?;
    if !matches!(message.status.as_str(), "sent" | "staged") { return Ok(delivery); }
    let Some(runtime) = store.latest_claim(&message.to, Some("runtime.observed"))? else {
        return Ok(delivery);
    };
    let runtime = runtime.body.get("fields").unwrap_or(&runtime.body);
    if runtime.get("status").and_then(Value::as_str) != Some("running") { return Ok(delivery); }
    let Some(incarnation) = runtime.get("incarnation_id").and_then(Value::as_str) else {
        return Ok(delivery);
    };
    // Evidence scopes this seat diagnostic to one mail item. Query only the
    // running incarnation: late old failures and clears cannot mask its state.
    let diagnostic = store.claude_preparation_diagnostic(&message.to, &message.subject, incarnation)?
        .filter(|(_, accepted)| *accepted >= sent_at);
    if let Some((body, _)) = diagnostic {
        let fields = body.get("fields").unwrap_or(&body);
        if fields.get("status").and_then(Value::as_str) == Some("waiting") {
            delivery["state"] = json!("waiting");
            // Diagnostics retain operator detail; delivery exposes only the safe
            // preparation/FIFO condition, never error strings or filesystem paths.
            delivery["reason"] = json!(crate::claude_channel::PREPARATION_WAIT_REASON);
        }
    }
    Ok(delivery)
}

fn launch_session_id(id: &str) -> &str {
    id.strip_prefix("launch/")
        .or_else(|| id.strip_prefix("planning-session/"))
        .unwrap_or(id)
}

fn client_launch_decision_resources(
    store: &Store,
    session: &PlanningSessionView,
) -> anyhow::Result<Vec<Value>> {
    let claims = store.claims_for(&session.subject, None)?;
    let answers = claims
        .iter()
        .filter(|claim| claim.kind == "planning-session.question-answered")
        .filter_map(|claim| {
            let id = claim.body.pointer("/fields/decision_id")?.as_str()?;
            Some((id.to_owned(), claim))
        })
        .collect::<BTreeMap<_, _>>();
    let mut decisions = claims
        .iter()
        .filter(|claim| claim.kind == "planning-session.question-requested")
        .filter_map(|claim| {
            let fields = claim.body.get("fields")?;
            let id = fields.get("decision_id")?.as_str()?;
            let answer = answers.get(id);
            Some(json!({
                "id": id,
                "kind": "launch-decision",
                "revision": answer.map(|answer| answer.id.as_str()).unwrap_or(claim.id.as_str()),
                "updated_at": client_timestamp(answer.map(|answer| answer.accepted_at_unix_ms).unwrap_or(claim.accepted_at_unix_ms)),
                "launch_id": format!("launch/{}", session.id),
                "question": fields.get("question").cloned().unwrap_or(Value::Null),
                "state": if answer.is_some() { "answered" } else { "open" },
                "requested_at": client_timestamp(claim.accepted_at_unix_ms),
                "decision_type": fields.get("decision_type").cloned().unwrap_or(Value::Null),
                "options": fields.get("options").cloned().unwrap_or_else(|| json!([])),
                "response": answer.and_then(|answer| answer.body.pointer("/fields/response")).cloned(),
                "explanation": answer.and_then(|answer| answer.body.pointer("/fields/explanation")).cloned(),
                "operational": { "layer": if answer.is_some() { "history" } else { "current" }, "actionable": answer.is_none(), "reasons": if answer.is_some() { vec!["answered"] } else { Vec::<&str>::new() } }
            }))
        })
        .collect::<Vec<_>>();
    decisions.sort_by(|left, right| {
        left["requested_at"]
            .as_str()
            .cmp(&right["requested_at"].as_str())
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(decisions)
}

fn launch_diagnostics(blockers: &[String], warnings: &[String]) -> Vec<Value> {
    blockers
        .iter()
        .map(|message| json!({"code": "preview-blocker", "severity": "error", "message": message, "path": null}))
        .chain(warnings.iter().map(|message| {
            json!({"code": "preview-warning", "severity": "warning", "message": message, "path": null})
        }))
        .collect()
}

fn client_launch_diagnostics(preview: &crate::model::PlanningPreviewView) -> Vec<Value> {
    launch_diagnostics(&preview.mission.blockers, &preview.mission.warnings)
}

fn client_safe_json(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("declarations_kdl");
            object.remove("kdl");
            object.remove("markdown");
            for value in object.values_mut() {
                client_safe_json(value);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(client_safe_json),
        _ => {}
    }
}

fn canonical_json(value: &Value, output: &mut String) -> anyhow::Result<()> {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => output.push_str(&serde_json::to_string(value)?),
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                canonical_json(value, output)?;
            }
            output.push(']');
        }
        Value::Object(object) => {
            output.push('{');
            let mut fields = object.iter().collect::<Vec<_>>();
            fields.sort_by(|left, right| left.0.cmp(right.0));
            for (index, (name, value)) in fields.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(name)?);
                output.push(':');
                canonical_json(value, output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

fn launch_preview_token(
    session: &PlanningSessionView,
    variant: &crate::model::PlanningVariantView,
    normalized_mission: &Value,
    diagnostics: &[Value],
) -> anyhow::Result<String> {
    launch_preview_token_values(
        &session.id,
        &variant.name,
        variant.candidate.revision,
        session.source_generation.as_deref(),
        normalized_mission,
        diagnostics,
    )
}

fn launch_preview_token_values(
    launch_id: &str,
    variant: &str,
    candidate_revision: u32,
    target_generation: Option<&str>,
    normalized_mission: &Value,
    diagnostics: &[Value],
) -> anyhow::Result<String> {
    let input = json!({
        "api_version": CLIENT_API_VERSION,
        "launch_id": format!("launch/{launch_id}"),
        "variant_id": format!("launch-variant/{launch_id}/{variant}"),
        "candidate_revision": candidate_revision,
        "target_generation": target_generation,
        "normalized_mission": normalized_mission,
        "diagnostics": diagnostics,
    });
    let mut canonical = String::new();
    canonical_json(&input, &mut canonical)?;
    Ok(format!(
        "lpv0:{}",
        hex::encode(Sha256::digest(canonical.as_bytes()))
    ))
}

fn launch_visualization(
    mission: &crate::model::MissionSpec,
    session: &PlanningSessionView,
    preview: &crate::model::PlanningPreviewView,
    decisions: &[Value],
) -> Value {
    let nodes = mission.display_order.iter().filter_map(|id| mission.steps.get(id)).map(|step| {
        json!({
            "id": format!("step/{}", step.path),
            "kind": "step",
            "label": step.title.as_deref().unwrap_or(&step.id),
            "path": step.path,
            "goals": step.goals,
            "constraints": step.constraints,
            "assignment": step.work_selector,
            "timeout_ms": step.timeout_ms,
            "retry": step.retry,
            "gates": step.gates,
            "loop": step.loop_spec,
            "resources": step.documents,
            "source_references": step.documents,
            "runtime": { "attempt": null, "lease": null, "progress": null, "blockers": [], "attention": [], "errors": [], "cursor": null }
        })
    }).collect::<Vec<_>>();
    let edges = mission
        .steps
        .values()
        .flat_map(|step| {
            step.dependencies
                .iter()
                .map(move |dependency| match dependency {
                    crate::model::DependencySpec::Step {
                        step: dependency, ..
                    } => json!({
                        "id": format!("edge/{}/{}", dependency, step.path),
                        "kind": "dependency",
                        "from": format!("step/{dependency}"),
                        "to": format!("step/{}", step.path),
                        "gate": dependency,
                    }),
                    crate::model::DependencySpec::Predicate { .. } => json!({
                        "id": format!("edge/predicate/{}", step.path),
                        "kind": "gate",
                        "from": null,
                        "to": format!("step/{}", step.path),
                        "gate": dependency,
                    }),
                })
        })
        .collect::<Vec<_>>();
    let timeline = mission
        .display_order
        .iter()
        .enumerate()
        .filter_map(|(ordinal, id)| mission.steps.get(id).map(|step| (ordinal, step)))
        .map(|(ordinal, step)| {
            json!({
                "id": format!("timeline/{}", step.path),
                "node": format!("step/{}", step.path),
                "ordinal": ordinal,
                "dependencies": step.dependencies,
                "timeout_ms": step.timeout_ms,
            })
        })
        .collect::<Vec<_>>();
    let mut lanes = BTreeMap::<String, Vec<String>>::new();
    for step in mission.steps.values() {
        let assignment = step
            .work_selector
            .as_ref()
            .map(|selector| serde_json::to_string(selector).unwrap_or_else(|_| "assigned".into()))
            .unwrap_or_else(|| "unassigned".into());
        lanes
            .entry(assignment)
            .or_default()
            .push(format!("step/{}", step.path));
    }
    json!({
        "version": "st3.visualization.v0",
        "views": ["graph", "timeline", "swimlane", "revision", "risk", "live-progress"],
        "mission": format!("mission/{}", mission.id),
        "nodes": nodes,
        "edges": edges,
        "groups": mission.steps.values().filter(|step| step.queue.is_some() || step.nested_mission.is_some()).map(|step| json!({
            "id": step.queue.clone().unwrap_or_else(|| step.path.clone()),
            "kind": if step.queue.is_some() { "queue" } else { "nested-mission" },
            "members": [format!("step/{}", step.path)]
        })).collect::<Vec<_>>(),
        "timeline": { "entries": timeline },
        "swimlanes": lanes.into_iter().enumerate().map(|(ordinal, (assignment, nodes))| json!({
            "id": format!("lane/{ordinal}"), "assignment": assignment, "nodes": nodes
        })).collect::<Vec<_>>(),
        "goals": mission.goals,
        "constraints": mission.constraints,
        "gates": mission.gates,
        "resources": mission.products,
        "decisions": decisions,
        "revision": { "candidate": mission.revision, "target_generation": session.source_generation },
        "diffs": [{ "changes": preview.mission.changes, "predicted_actions": preview.mission.predicted_actions }],
        "risk": { "blockers": preview.mission.blockers, "warnings": preview.mission.warnings, "gates": mission.gates },
        "live_progress": { "state": session.status, "cursor": null, "updated_at": client_timestamp(session.updated_at_unix_ms) }
    })
}

fn launch_compact_preview(
    state: &AppState,
    session: &PlanningSessionView,
    intent: &crate::model::NormalizedIntent,
    mission: &crate::model::MissionSpec,
    diagnostics_count: usize,
) -> anyhow::Result<Value> {
    let steps = mission
        .display_order
        .iter()
        .filter_map(|id| mission.steps.get(id))
        .map(|step| {
            let assignee = match step
                .work_selector
                .as_ref()
                .or(mission.work_selector.as_ref())
            {
                Some(crate::model::WorkSelector::Assigned { agent }) => Some(agent.as_str()),
                _ => None,
            };
            json!({
                "path": step.path,
                "title": step.title.as_deref().unwrap_or(&step.id),
                "assignee": assignee,
                "depends": step.dependencies.iter().filter_map(|dependency| match dependency {
                    crate::model::DependencySpec::Step { step, .. } => Some(step.as_str()),
                    _ => None,
                }).collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();
    let assigned_agents = mission
        .steps
        .values()
        .map(|step| {
            step.work_selector
                .as_ref()
                .or(mission.work_selector.as_ref())
        })
        .flat_map(|selector| match selector {
            Some(crate::model::WorkSelector::Assigned { agent }) => vec![agent.clone()],
            Some(crate::model::WorkSelector::Available { agents }) => agents.clone(),
            _ => Vec::new(),
        })
        .collect::<BTreeSet<_>>();
    let mut agents_by_id = state
        .store
        .desired_subjects_named(&assigned_agents.iter().cloned().collect::<Vec<_>>())?
        .into_iter()
        .filter(|subject| subject.kind == "agent" && assigned_agents.contains(&subject.subject))
        .map(|subject| (subject.subject.clone(), subject))
        .collect::<BTreeMap<_, _>>();
    agents_by_id.extend(
        intent
            .subjects
            .values()
            .filter(|subject| subject.kind == "agent")
            .map(|subject| (subject.subject.clone(), subject.clone())),
    );
    let agents = agents_by_id
        .values()
        .map(|subject| {
            json!({
                "id": subject.subject,
                "harness": subject.member.as_ref().and_then(|member| member.driver.as_deref()),
                "host": subject.member.as_ref().map(|member| member.host.as_str()),
                "worktree": subject.member.as_ref().map(|member| member.workspace.as_str())
            })
        })
        .collect::<Vec<_>>();
    let gates = mission
        .gates
        .iter()
        .chain(mission.steps.values().flat_map(|step| step.gates.iter()));
    let (human, automated) = gates.fold((0, 0), |(human, automated), gate| {
        if matches!(gate, crate::model::GateSpec::Human { .. }) {
            (human + 1, automated)
        } else {
            (human, automated + 1)
        }
    });
    let request_excerpt = session
        .request
        .rsplit_once('@')
        .and_then(|(name, hash)| state.store.get_document(name, hash).ok().flatten())
        .map(|bytes| {
            String::from_utf8_lossy(&bytes)
                .chars()
                .take(300)
                .collect::<String>()
        })
        .unwrap_or_default();
    Ok(json!({
        "goal": mission.goals.first().cloned().unwrap_or_default(),
        "steps": steps,
        "agents": agents,
        "gates": { "human": human, "automated": automated },
        "diagnostics_count": diagnostics_count,
        "request_excerpt": request_excerpt
    }))
}

fn client_launch_variant_resources(
    state: &AppState,
    session: &PlanningSessionView,
) -> anyhow::Result<Vec<Value>> {
    let mut resources = Vec::new();
    for (ordinal, variant) in session.variants.iter().enumerate() {
        let (normalized, diagnostics, visualization, structured_diff, compact_preview) =
            if let Some(preview) = &variant.preview {
                let intent = parse_intent(&preview.mission.resolved_intent.kdl, &state.node)
                    .map_err(|error| anyhow::anyhow!(error.message))?;
                let mission = intent
                    .missions
                    .get(&session.mission)
                    .ok_or_else(|| anyhow::anyhow!("preview mission is missing"))?;
                let mut normalized = serde_json::to_value(mission)?;
                if let Some(provenance) = preview.mission.mission_provenance.get(&session.mission) {
                    normalized["provenance"] = serde_json::to_value(provenance)?;
                }
                client_safe_json(&mut normalized);
                let diagnostics = client_launch_diagnostics(preview);
                let compact_preview =
                    launch_compact_preview(state, session, &intent, mission, diagnostics.len())?;
                (
                    normalized,
                    diagnostics,
                    launch_visualization(
                        mission,
                        session,
                        preview,
                        &client_launch_decision_resources(&state.store, session)?,
                    ),
                    json!({"changes": preview.mission.changes, "predicted_actions": preview.mission.predicted_actions}),
                    Some(compact_preview),
                )
            } else {
                (
                    json!({}),
                    Vec::new(),
                    json!({"version": "st3.visualization.v0", "views": [], "nodes": [], "edges": [], "groups": [], "timeline": {"entries": []}, "swimlanes": [], "goals": [], "constraints": [], "gates": [], "resources": [], "decisions": [], "revision": {}, "diffs": [], "risk": {}, "live_progress": {}}),
                    json!({"changes": [], "predicted_actions": []}),
                    None,
                )
            };
        let preview_token = variant
            .preview
            .as_ref()
            .map(|_| launch_preview_token(session, variant, &normalized, &diagnostics))
            .transpose()?;
        let blocked = diagnostics
            .iter()
            .any(|diagnostic| diagnostic["severity"] == "error");
        let approved = session.status == "approved"
            && session.candidate.as_ref().is_some_and(|candidate| {
                candidate.variant == variant.name
                    && candidate.revision == variant.candidate.revision
            });
        resources.push(json!({
            "id": format!("launch-variant/{}/{}", session.id, variant.name),
            "kind": "launch-variant",
            "revision": format!("launch-variant-revision/{}/{}/{}", session.id, variant.name, variant.candidate.revision),
            "updated_at": client_timestamp(variant.preview.as_ref().map(|preview| preview.created_at_unix_ms).unwrap_or(variant.candidate.submitted_at_unix_ms)),
            "launch_id": format!("launch/{}", session.id),
            "ordinal": ordinal,
            "candidate_revision": variant.candidate.revision,
            "status": if approved { "approved" } else if blocked { "blocked" } else if variant.preview.is_some() { "previewable" } else { "draft" },
            "target_generation": session.source_generation,
            "normalized_mission": normalized,
            "diagnostics": diagnostics,
            "preview_token": preview_token,
            "preview": compact_preview,
            "structured_diff": structured_diff,
            "visualization": visualization,
            "operational": { "layer": if approved { "history" } else { "current" }, "actionable": !approved && !blocked, "reasons": if blocked { vec!["blocked"] } else if approved { vec!["approved"] } else { Vec::<&str>::new() } }
        }));
    }
    Ok(resources)
}

fn client_launch_resource(
    state: &AppState,
    session: &PlanningSessionView,
) -> anyhow::Result<Value> {
    let store = &state.store;
    let phase = match session.status.as_str() {
        "planning" | "revision-requested" => "authoring",
        "review" => "review",
        "approved" => "approved",
        "cancelled" => "cancelled",
        _ => "failed",
    };
    let historical = matches!(phase, "approved" | "cancelled" | "failed");
    let target = match (&session.target_mission_run, &session.source_generation) {
        (Some(run), Some(generation)) => json!({
            "type": "mission-run",
            "mission_run_id": run,
            "generation_id": generation
        }),
        _ => json!({ "type": "new-mission" }),
    };
    let decisions = client_launch_decision_resources(store, session)?;
    let latest_variant = client_launch_variant_resources(state, session)?
        .into_iter()
        .rev()
        .next();
    let visualization = latest_variant
        .as_ref()
        .and_then(|variant| variant.get("visualization").cloned());
    let preview = latest_variant
        .as_ref()
        .and_then(|variant| variant.get("preview").cloned());
    let preview_token = latest_variant
        .as_ref()
        .and_then(|variant| variant.get("preview_token").cloned());
    let approval_ids = store
        .claims_for(&session.subject, None)?
        .into_iter()
        .filter(|claim| claim.kind == "planning-session.approved")
        .filter_map(|claim| {
            claim
                .body
                .pointer("/fields/candidate_revision")
                .and_then(Value::as_u64)
                .map(|revision| format!("launch-approval/{}/{revision}", session.id))
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "id": format!("launch/{}", session.id),
        "kind": "launch",
        "revision": format!("launch/{}", session.updated_at_unix_ms),
        "updated_at": client_timestamp(session.updated_at_unix_ms),
        "title": session.mission,
        "phase": phase,
        "request": session.request,
        "planner": session.planner,
        "planner_config": session.planner_config,
        "target": target,
        "variants": session.variants.iter().map(|variant| format!("launch-variant/{}/{}", session.id, variant.name)).collect::<Vec<_>>(),
        "decisions": decisions.iter().filter_map(|decision| decision["id"].as_str()).collect::<Vec<_>>(),
        "approvals": approval_ids,
        "visualization": visualization,
        "preview": preview,
        "preview_token": preview_token,
        "operational": { "layer": if historical { "history" } else { "current" }, "actionable": !historical, "reasons": if historical { vec![phase] } else { Vec::<&str>::new() } }
    }))
}

fn client_launch_resources(state: &AppState, history: bool) -> anyhow::Result<Vec<Value>> {
    let mut resources = state
        .store
        .planning_sessions(history)?
        .iter()
        .map(|session| client_launch_resource(state, session))
        .collect::<anyhow::Result<Vec<_>>>()?;
    resources.sort_by(|left, right| {
        right["updated_at"]
            .as_str()
            .cmp(&left["updated_at"].as_str())
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(resources)
}

/// Select a visible native session before rendering. A native ID may itself start with
/// `launch/`; only if it is absent from this listing does the public-ID fallback apply.
fn client_launch_detail_at(
    state: &AppState,
    id: &str,
    history: bool,
) -> anyhow::Result<Option<Value>> {
    for native_id in std::iter::once(id).chain(id.strip_prefix("launch/")) {
        // Strip exactly this synthetic prefix in Store::planning_session, preserving a
        // native ID that itself begins with `planning-session/`.
        let Some(session) = state
            .store
            .planning_session(&format!("planning-session/{native_id}"))?
        else {
            continue;
        };
        if !history && matches!(session.status.as_str(), "approved" | "cancelled" | "failed") {
            continue;
        }
        return client_launch_resource(state, &session).map(Some);
    }
    Ok(None)
}

/// How long a fresh current-work read waits for a publication at least as new as itself.
const WORK_LIST_FRESH_WAIT: Duration = Duration::from_secs(2);

/// Where a published current-work page's cursor continues: the publication its first page
/// served and the actor's rows in it, kept as the page cache keeps a first page's items.
struct PublishedWorkPage {
    digest: String,
    /// The publication's identity: its list, generation and number. A continuation is served
    /// only while that list is still the store's and in that generation.
    id: crate::store::published_list::PublicationId,
    /// What the first page issued, which a cursor, being the client's to edit, must repeat.
    contract: PublishedWorkContract,
    /// Only the rows the page lists, in order: the actor's, or every row for a fleet page,
    /// sharing the publication's rendered values. Not the publication, its memo or its seats.
    rows: Arc<Vec<Arc<Value>>>,
    expires_at_unix_ms: u128,
}

/// Drop the entries that expired, and this list's entries of a generation a forget replaced.
/// Another store's entries are left to their own list. Lazy: run at each cache access.
fn purge_published_work_pages(
    pages: &mut VecDeque<PublishedWorkPage>,
    list: &crate::store::published_list::PublishedList<crate::store::work_list::WorkRows>,
) {
    let now = client_now_ms();
    let (instance, generation) = (list.instance(), list.generation());
    pages.retain(|page| {
        page.expires_at_unix_ms > now && (page.id.list != instance || page.id.generation == generation)
    });
}

/// A published work page's issued limit, snapshot and filters.
#[derive(PartialEq, Eq)]
struct PublishedWorkContract {
    limit: usize,
    /// The whole issued snapshot, each field of it, as the cursor carries it.
    snapshot: String,
    person: Option<String>,
    actor: Option<String>,
    owner_run: Option<String>,
    status: Option<String>,
    native_only: bool,
}

impl PublishedWorkContract {
    fn of(cursor: &ClientPageCursor) -> Self {
        Self {
            limit: cursor.limit,
            snapshot: serde_json::to_string(&cursor.snapshot).expect("a client snapshot serializes"),
            person: cursor.person.clone(),
            actor: cursor.actor.clone(),
            owner_run: cursor.owner_run.clone(),
            status: cursor.status.clone(),
            native_only: cursor.native_only,
        }
    }
}

static PUBLISHED_WORK_PAGES: OnceLock<Mutex<VecDeque<PublishedWorkPage>>> = OnceLock::new();

fn published_work_pages() -> &'static Mutex<VecDeque<PublishedWorkPage>> {
    PUBLISHED_WORK_PAGES.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// How many published work page entries a list's publications hold, for tests: the cache is
/// shared by every store in the process.
#[cfg(test)]
fn published_work_pages_of(list: u64) -> usize {
    published_work_pages()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|page| page.id.list == list)
        .count()
}

/// The first `items_digest` segment of a cursor that continues a published work page.
const PUBLISHED_WORK_DIGEST: &str = "published-work:";

/// A published work page's identity: the host, the publication (its list, generation and
/// number, unique within the process) and the actor, hex-encoded so no actor can spell
/// another's.
fn published_work_digest(
    state: &AppState,
    id: crate::store::published_list::PublicationId,
    actor: Option<&str>,
) -> String {
    let actor = actor.map_or_else(|| "-".to_owned(), |actor| hex::encode(actor.as_bytes()));
    format!(
        "{PUBLISHED_WORK_DIGEST}{}:{}:{}:{}:{actor}",
        hex::encode(state.node.as_bytes()),
        id.list,
        id.generation,
        id.sequence
    )
}

/// A first page of `publication`, under the snapshot of its own cut. An actor's rows come from
/// its order in the publication, built on its first read. The page is emitted only if no
/// forget replaced the projections since `id` was acquired.
fn client_work_published_page(
    state: &AppState,
    publication: &Arc<crate::store::published_list::Publication<crate::store::work_list::WorkRows>>,
    id: crate::store::published_list::PublicationId,
    query: &ClientListQuery,
) -> Result<ClientPageResponse, ApiError> {
    let started = Instant::now();
    let limit = query.limit.unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS).clamp(1, CLIENT_MAX_PAGE_ITEMS);
    let mut built = false;
    let indexes = query.actor.as_deref().map(|actor| {
        let (indexes, fresh) = publication.rows.actor_rows(actor);
        built = fresh;
        indexes
    });
    let (items, has_more) = publication.rows.page_of(indexes.as_deref().map(Vec::as_slice), 0, limit);
    // The final fence: emit nothing a forget replaced since `id` was acquired.
    let list = state.store.published_work_list();
    if list.instance() != id.list || list.generation() != id.generation {
        return Err(published_lists::work_list_not_ready());
    }
    let snapshot = roster_snapshot(state, publication.cut, publication.published_at_unix_ms);
    let digest = published_work_digest(state, id, query.actor.as_deref());
    let mut expires_at_unix_ms = 0;
    let next_cursor = if has_more {
        let mut cursor = published_work_cursor(&snapshot, query, items.len(), limit, digest, 0);
        let contract = PublishedWorkContract::of(&cursor);
        let mut pages = published_work_pages().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        purge_published_work_pages(&mut pages, list);
        // The same page asked again reuses its entry and its original expiry: polling the same
        // first page neither adds an entry nor evicts another's.
        expires_at_unix_ms = match pages
            .iter()
            .find(|page| page.digest == cursor.items_digest && page.contract == contract)
        {
            Some(page) => page.expires_at_unix_ms,
            None => {
                while pages.len() >= CLIENT_PAGE_CACHE_CAPACITY {
                    pages.pop_front();
                }
                let expires_at_unix_ms = client_now_ms().saturating_add(CLIENT_PAGE_TTL_MS);
                pages.push_back(PublishedWorkPage {
                    digest: cursor.items_digest.clone(),
                    id,
                    contract,
                    rows: Arc::new(publication.rows.selected_rows(indexes.as_deref().map(Vec::as_slice))),
                    expires_at_unix_ms,
                });
                expires_at_unix_ms
            }
        };
        cursor.expires_at_unix_ms = expires_at_unix_ms;
        Some(encode_client_cursor(&cursor)?)
    } else {
        None
    };
    let page = ClientResourcePage {
        kind: "page".into(),
        collection: "work".into(),
        filters: client_page_filters("work", query),
        items,
        page: ClientPageInfo {
            limit,
            has_more,
            next_cursor,
            cursor_expires_at: has_more.then(|| client_timestamp(expires_at_unix_ms)),
        },
        sync: client_sync_notice(state),
        replicated: None,
    };
    let stage = if built { request_latency::WorkStage::ActorBuild } else { request_latency::WorkStage::Page };
    record_work_stage(stage, started.elapsed());
    Ok((Extension(snapshot), Json(page)))
}

fn published_work_cursor(
    snapshot: &ClientSnapshot,
    query: &ClientListQuery,
    offset: usize,
    limit: usize,
    items_digest: String,
    expires_at_unix_ms: u128,
) -> ClientPageCursor {
    ClientPageCursor {
        snapshot: snapshot.clone(),
        collection: "work".into(),
        offset,
        limit,
        history: false,
        person: query.person.clone(),
        actor: query.actor.clone(),
        owner_run: query.owner_run.clone(),
        status: query.status.clone(),
        owner: None,
        state: None,
        native_only: query.native_only,
        items_digest,
        before_index: None,
        after_key: None,
        expires_at_unix_ms,
    }
}

fn agent_roster_not_ready() -> ApiError {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "agent-roster-not-ready".into(),
        message: "the agents roster is still being prepared; retry shortly".into(),
        details: Box::default(),
    }
}

/// A successful wait cannot certify a later page after a publication reset or replacement.
fn checked_agent_roster_page(
    page: Option<ClientPageResponse>,
    min_cut: Option<u64>,
) -> Result<Option<ClientPageResponse>, ApiError> {
    if let Some(wanted) = min_cut
        && page.as_ref().is_none_or(|page| page.0.0.store_index < wanted)
    {
        return Err(agent_roster_cut_not_ready(wanted));
    }
    Ok(page)
}

fn agent_roster_cut_not_ready(wanted: u64) -> ApiError {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "agent-roster-cut-not-ready".into(),
        message: format!("the agents roster has not published cut {wanted}; retry shortly"),
        details: Box::new(serde_json::Map::from_iter([("min_cut".into(), json!(wanted))])),
    }
}

/// How many agents one short snapshot reduces while the refresher warms the roster.
const AGENT_ROSTER_WARM_CHUNK: usize = 250;

/// Reduce every agent's current-view and card status in short snapshots of at most
/// [`AGENT_ROSTER_WARM_CHUNK`] agents. These per-agent reductions stay cached until a claim
/// they depend on arrives, so the folds that follow at one cut read them instead of the log.
fn warm_agent_roster(store: &Store) -> anyhow::Result<()> {
    let subjects = store.read_snapshot(|index| {
        let connection = store.readers.get();
        crate::otel::roster_phase("membership", || -> anyhow::Result<Vec<String>> {
            Ok(connection
                .prepare_cached(crate::store::RANGE_SUBJECTS)?
                .query_map(rusqlite::params![index, "agent/", "agent0"], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?)
        })
    })?;
    for chunk in subjects.chunks(AGENT_ROSTER_WARM_CHUNK) {
        let names = chunk.iter().cloned().collect::<BTreeSet<_>>();
        store.read_snapshot(|index| crate::otel::roster_phase("status_fold", || {
            store.agent_card_status_at(Some(&names), index, false).map(drop)
        }))?;
    }
    Ok(())
}

/// How many times a refresh folds the roster in chunks before it gives up on a roster that
/// keeps changing in ways it cannot follow card by card.
const AGENT_ROSTER_ASSEMBLY_ROUNDS: usize = 3;

/// Publish the complete roster at a new cut, never folding more than
/// [`AGENT_ROSTER_WARM_CHUNK`] cards in one snapshot. A snapshot completes the roster only once
/// the newest rows say which cards changed and at most that many are changed or missing.
/// Otherwise the cards fold in chunks, each at its own cut, refolding the already folded cards
/// whose claims changed, and completion is tried again. Readers keep the previous complete
/// roster meanwhile, with its own cut and publication time; if it cannot be assembled, the
/// refresh fails and is tried again on the next request.
fn refresh_agent_roster(store: &Store, history: bool) -> anyhow::Result<()> {
    store.begin_agent_roster_fault_retries(history);
    if store.read_snapshot(|index| store.agent_roster_current(index, history))? {
        return Ok(());
    }
    let complete = |store: &Store| store.read_snapshot(|index| {
        if let Some(reason) =
            store.agent_roster_unbounded_because(index, history, AGENT_ROSTER_WARM_CHUNK)?
        {
            return Ok(Some(reason));
        }
        client_agent_resources_cached(store, history, index).map(|_| None)
    });
    for _ in 0..AGENT_ROSTER_ASSEMBLY_ROUNDS {
        let Some(reason) = complete(store)? else {
            return Ok(());
        };
        store.note_agent_roster_chunked(&reason);
        let order = store.read_snapshot(|index| {
            Ok(client_agent_page_refs(store, history, index)?.iter()
                .filter_map(|reference| reference["id"].as_str().map(str::to_owned))
                .collect::<Vec<_>>())
        })?;
        for chunk in order.chunks(AGENT_ROSTER_WARM_CHUNK) {
            let chunk = chunk.iter().cloned().collect::<BTreeSet<_>>();
            store.read_snapshot(|index| {
                client_agent_cards_selected(store, history, index, &chunk, true).map(drop)
            })?;
        }
    }
    if complete(store)?.is_none() {
        return Ok(());
    }
    anyhow::bail!("the agents roster kept changing in ways no short fold can follow; keeping the previous one")
}

/// Every current agent's refs, and the cards of as many as the largest window or page shows.
fn client_agent_roster_head(store: &Store, index: u64) -> anyhow::Result<()> {
    let refs = client_agent_page_refs(store, false, index)?;
    let head = refs.iter().take(CLIENT_MAX_PAGE_ITEMS + 1)
        .filter_map(|reference| reference["id"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>();
    client_agent_cards_selected(store, false, index, &head, false).map(drop)
}

/// The exit taken by the existing bounded fresh wait, including its unsuccessful exits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RosterFreshWaitOutcome {
    Published,
    Unchanged,
    RefreshFailed,
    Disconnected,
    Timeout,
    ReadFailed,
}

impl RosterFreshWaitOutcome {
    fn name(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Unchanged => "unchanged",
            Self::RefreshFailed => "refresh_failed",
            Self::Disconnected => "disconnected",
            Self::Timeout => "timeout",
            Self::ReadFailed => "read_failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RosterFreshWait {
    // This is obtained when the wait starts, and can be later than the request snapshot.
    wanted: Option<u64>,
    outcome: RosterFreshWaitOutcome,
}

/// For a read that asked to see what was written before it: wait, briefly, for the refresher
/// to publish a roster at or after the current cut, unless no claim since the newest
/// publication changes a card, so that a refresh would fold nothing. The read itself folds
/// nothing. The exit is reported separately from proof of the publication later served.
async fn wait_for_agent_roster(store: &Arc<Store>, history: bool) -> RosterFreshWait {
    let Ok(wanted) = store.index() else {
        return RosterFreshWait {
            wanted: None,
            outcome: RosterFreshWaitOutcome::ReadFailed,
        };
    };
    let publications = store.subscribe_agent_roster();
    let outcome = wait_for_agent_roster_publications(store, history, wanted, publications).await;
    RosterFreshWait {
        wanted: Some(wanted),
        outcome,
    }
}

// A separate receiver lets a private control exercise the existing closed-channel exit.
// Production always uses the Store receiver above; wait policy and deadline stay unchanged.
async fn wait_for_agent_roster_publications(
    store: &Arc<Store>,
    history: bool,
    wanted: u64,
    mut publications: tokio::sync::watch::Receiver<u64>,
) -> RosterFreshWaitOutcome {
    let published = |store: &Store| {
        let index = store.index().ok()?;
        store
            .published_agent_roster(index, history)
            .map(|(cut, _, _)| cut)
            .or_else(|| {
                (!history)
                    .then(|| store.published_agent_roster_head(index, 1))
                    .flatten()
                    .map(|(cut, _, _, _)| cut)
            })
    };
    tokio::time::timeout(AGENT_ROSTER_READ_WAIT, async {
        while published(store).is_none_or(|cut| cut < wanted) {
            store.request_fresh_agent_roster(history);
            if published(store).is_some() && store.agent_roster_refresh_failed() {
                return RosterFreshWaitOutcome::RefreshFailed;
            }
            // Claims on unrelated subjects move the cut all the time; the paced refresher
            // would publish the same cards. One bounded range read tells.
            let reader = Arc::clone(store);
            if let Ok(true) = blocking_store(move || {
                reader.published_agent_roster_unchanged_through(wanted, history)
            })
            .await
            {
                return RosterFreshWaitOutcome::Unchanged;
            }
            if publications.changed().await.is_err() {
                return RosterFreshWaitOutcome::Disconnected;
            }
        }
        RosterFreshWaitOutcome::Published
    })
    .await
    .unwrap_or(RosterFreshWaitOutcome::Timeout)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RosterUnchangedProof {
    NotChecked,
    Verified,
    Refused,
    ReadFailed,
}

impl RosterUnchangedProof {
    fn name(self) -> &'static str {
        match self {
            Self::NotChecked => "not_checked",
            Self::Verified => "verified",
            Self::Refused => "refused",
            Self::ReadFailed => "read_failed",
        }
    }
}

fn roster_fresh_served_proof(
    store: &Store,
    history: bool,
    waited: RosterFreshWait,
    served: Option<(u64, u128, &Arc<Vec<Value>>)>,
) -> RosterUnchangedProof {
    if waited.outcome != RosterFreshWaitOutcome::Unchanged {
        return RosterUnchangedProof::NotChecked;
    }
    let Some(wanted) = waited.wanted else {
        return RosterUnchangedProof::ReadFailed;
    };
    let Some((cut, published_at, cards)) = served else {
        return RosterUnchangedProof::Refused;
    };
    match store.served_agent_roster_unchanged_through(wanted, history, cut, published_at, cards) {
        Ok(true) => RosterUnchangedProof::Verified,
        Ok(false) => RosterUnchangedProof::Refused,
        Err(_) => RosterUnchangedProof::ReadFailed,
    }
}

// The wait's exit alone is not a certificate for whichever publication the later worker
// selects. Verify only the exact complete rows being served, within a read-only snapshot.
// A failure/refusal is recorded honestly and does not change the existing response policy.
fn record_roster_fresh_wait(
    store: &Store,
    history: bool,
    waited: Option<RosterFreshWait>,
    served: Option<(u64, u128, &Arc<Vec<Value>>)>,
) {
    let Some(waited) = waited else { return };
    // Like the other detailed roster attributes, proof work is opt-in for a recording
    // request. Untraced requests retain the existing read cost and response behavior.
    if !crate::otel::roster_recording() {
        return;
    }
    crate::otel::roster_attribute("st.roster.fresh_wait", waited.outcome.name());
    if let Some(wanted) = waited.wanted {
        crate::otel::roster_attribute("st.roster.fresh_wait_cut", wanted as i64);
    }
    let proof = roster_fresh_served_proof(store, history, waited, served);
    crate::otel::roster_attribute("st.roster.fresh_proof", proof.name());
    crate::otel::roster_attribute(
        "st.roster.fresh_unchanged_verified",
        proof == RosterUnchangedProof::Verified,
    );
}

/// Wait only for the caller's mutation cut. Concurrent writes never move the target.
/// A failed refresh cannot satisfy read-after-write; retain the bounded publication wait.
async fn wait_for_agent_roster_cut(store: &Arc<Store>, history: bool, wanted: u64) -> bool {
    let published = |store: &Store| {
        let index = store.index().ok()?;
        store.published_agent_roster(index, history).map(|(cut, _, _)| cut).or_else(|| {
            (!history).then(|| store.published_agent_roster_head(index, 1)).flatten()
                .map(|(cut, _, _, _)| cut)
        })
    };
    let mut publications = store.subscribe_agent_roster();
    tokio::time::timeout(AGENT_ROSTER_READ_WAIT, async {
        while published(store).is_none_or(|cut| cut < wanted) {
            store.request_fresh_agent_roster(history);
            if publications.changed().await.is_err() {
                return false;
            }
        }
        true
    }).await.unwrap_or(false)
}

/// The snapshot of a roster published at `cut`: that cut, its time, and when it was folded.
fn roster_snapshot(state: &AppState, cut: u64, published_at_unix_ms: u128) -> ClientSnapshot {
    ClientSnapshot {
        published_at: Some(client_timestamp(published_at_unix_ms)),
        ..client_snapshot_at(state, cut)
    }
}

/// A first page of the newest published roster, under the snapshot it was folded at.
/// `requested` is the request's own cut, recorded beside the publication cut it was served.
fn client_agents_published_page(
    state: &AppState,
    query: &ClientListQuery,
    requested: u64,
    fresh_wait: Option<RosterFreshWait>,
) -> Result<Option<ClientPageResponse>, ApiError> {
    let store = &state.store;
    // Keep one recording point after all ordinary returns, including errors. Retain an
    // Arc only for traced requests; the proof still refers to the selected publication.
    let recording = fresh_wait.is_some() && crate::otel::roster_recording();
    let mut served = None;
    let result = (|| {
        let current = store.index().map_err(ApiError::internal)?;
        let index = current;
        let Some((index, cards, published_at)) = store.published_agent_roster(index, query.history)
        else {
            // Before the first complete roster, an unfiltered first page can come from its head.
            if query.status.is_some() || query.history {
                return Ok(None);
            }
            let Some((index, refs, head, published_at)) =
                store.published_agent_roster_head(index, CLIENT_MAX_PAGE_ITEMS + 1)
            else {
                return Ok(None);
            };
            store.request_agent_roster_refresh();
            crate::otel::record_roster_read("published_head", requested, Some(index), query.fresh || query.min_cut.is_some());
            // A partial head is not the complete publication used by unchanged-through.
            let snapshot = roster_snapshot(state, index, published_at);
            let mut page =
                client_page_read(state, &snapshot, "agents", (*refs).clone(), query, true)?;
            page.items = client_agent_cards_from_cached(
                store,
                page_cards(&head, &page.items),
                &page.items,
                &snapshot.created_at,
            )?;
            return Ok(Some((Extension(snapshot), Json(page))));
        };
        // A roster older than the current cut, its local activity or its queue deadline needs a
        // refresh; the newest one needs none.
        if index < current
            || !store
                .agent_roster_current(current, query.history)
                .map_err(ApiError::internal)?
        {
            if query.history {
                store.request_agent_roster_history();
            } else {
                store.request_agent_roster_refresh();
            }
        }
        let snapshot = roster_snapshot(state, index, published_at);
        let path = if query.status.is_some() {
            "filtered_full"
        } else {
            "published_complete"
        };
        crate::otel::record_roster_read(path, requested, Some(index), query.fresh || query.min_cut.is_some());
        if recording {
            served = Some((index, published_at, Arc::clone(&cards)));
        }
        if let Some(status) = query.status.as_deref() {
            let mut cards = (*cards).clone();
            overlay_agent_resources(store, &mut cards, &snapshot.created_at)
                .map_err(ApiError::internal)?;
            cards.retain(|card| card["state"].as_str() == Some(status));
            let page = client_page_read(state, &snapshot, "agents", cards, query, true)?;
            return Ok(Some((Extension(snapshot), Json(page))));
        }
        // Continuations keep the same membership, order and queue metadata as the exact path's
        // refs, and read their cards from this same publication.
        let refs = crate::otel::roster_phase("membership", || {
            cards
                .iter()
                .map(|card| {
                    let mut reference = serde_json::Map::new();
                    for field in ["id", "name", "host_id"]
                        .into_iter()
                        .chain(AGENT_QUEUE_FIELDS)
                    {
                        reference.insert(field.to_owned(), card[field].clone());
                    }
                    Value::Object(reference)
                })
                .collect::<Vec<_>>()
        });
        let mut page = client_page_read(state, &snapshot, "agents", refs, query, true)?;
        page.items = client_agent_cards_from_cached(
            store,
            page_cards(&cards, &page.items),
            &page.items,
            &snapshot.created_at,
        )?;
        Ok(Some((Extension(snapshot), Json(page))))
    })();
    record_roster_fresh_wait(
        store,
        query.history,
        fresh_wait,
        served
            .as_ref()
            .filter(|_| matches!(&result, Ok(Some(_))))
            .map(|(cut, published_at, cards)| (*cut, *published_at, cards)),
    );
    result
}

/// The cards a page's refs name, from published cards.
fn page_cards(cards: &[Value], refs: &[Value]) -> Vec<Value> {
    let selected = refs.iter().filter_map(|item| item["id"].as_str()).collect::<BTreeSet<_>>();
    cards.iter()
        .filter(|card| card["id"].as_str().is_some_and(|id| selected.contains(id)))
        .cloned().collect()
}

/// Carry on a client read a peer relayed to this node because it is on the way to the owner.
/// The owner's refusal travels back as it was given; a missing route is `remote-unavailable`.
async fn forward_client_read(
    State(state): State<AppState>,
    Json(request): Json<crate::peer::ClientReadRequest>,
) -> Result<Json<Value>, ApiError> {
    let target = request
        .relay
        .as_ref()
        .map_or_else(String::new, |relay| relay.target.clone());
    let relay = state
        .client_relay
        .as_ref()
        .ok_or_else(|| remote_unavailable_for_owner(&state, &target))?;
    relay.forward(&request).await.map(Json).map_err(|error| {
        match error.downcast_ref::<crate::peer::ClientReadRejected>() {
            Some(rejected) => ApiError {
                status: StatusCode::from_u16(rejected.status).unwrap_or(StatusCode::CONFLICT),
                code: rejected.code.clone(),
                message: rejected.message.clone(),
                details: Box::new(rejected.details.clone()),
            },
            None => remote_unavailable_for_owner(&state, &target),
        }
    })
}

/// A read with no owner to send it to: this node has no relay, or no peer reaches the owner.
fn remote_unavailable(host: &str) -> ApiError {
    let mut details = serde_json::Map::new();
    details.insert("reason".into(), "no-route".into());
    details.insert("owner_host_id".into(), host.into());
    details.insert("hops".into(), 0.into());
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "remote-unavailable".into(),
        message: format!(
            "no route to owner {host}: this node cannot dial it and no peer reaches it; cached data remains usable"
        ),
        details: Box::new(details),
    }
}

/// A known dial-out owner syncs outward but has no inbound owner transport.
fn remote_unavailable_for_owner(state: &AppState, host: &str) -> ApiError {
    let dial_out = if let Some(relay) = &state.client_relay {
        relay.is_dial_out_owner(host)
    } else {
        host.strip_prefix("host/").is_some_and(|name| {
            state.store.fleet_view_sealed().is_ok_and(|view| {
                view.members.iter().any(|member| {
                    member.name == name && member.state == "current" && member.mode == "dial-out"
                })
            })
        })
    };
    if dial_out {
        remote_read_error(
            host,
            crate::peer::ClientReadRejected::dial_out_owner(host).into(),
        )
    } else {
        remote_unavailable(host)
    }
}

/// How many nodes the furthest route a read tried handed it to, from the attempts it records.
fn attempt_hops(attempts: Option<&Value>) -> u64 {
    attempts.and_then(Value::as_array).map_or(0, |attempts| {
        attempts
            .iter()
            .map(|attempt| 1 + attempt_hops(attempt.get("next")))
            .max()
            .unwrap_or(0)
    })
}

fn remote_read_error(host: &str, error: anyhow::Error) -> ApiError {
    let rejected = match error.downcast::<crate::peer::ClientReadRejected>() {
        Ok(rejected) => rejected,
        Err(error) => crate::peer::ClientReadRejected::unreachable(
            "transport-error",
            format!("the read to owner {host} failed: {error:#}"),
        ),
    };
    let mut details = rejected.details.clone();
    details
        .entry("owner_host_id")
        .or_insert_with(|| host.into());
    if rejected.code == "remote-unavailable" {
        let hops = attempt_hops(details.get("attempts"));
        details.entry("hops").or_insert_with(|| hops.into());
        let elapsed_ms = details.get("elapsed_ms").and_then(Value::as_u64);
        tracing::warn!(
            owner = host,
            reason = rejected.reason().unwrap_or("unknown"),
            hops,
            elapsed_ms,
            "a client read could not reach its owner: {}",
            rejected.message
        );
        return ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "remote-unavailable".into(),
            message: rejected.message,
            details: Box::new(details),
        };
    }
    if !matches!(
        rejected.code.as_str(),
        "read-deadline"
            | "page-cursor-expired"
            | "conversation-content-invalidated"
            | "transcript-unavailable"
            | "timeline-history-incomplete"
            | "cursor-gap"
            | "not-found"
            | "stale-fence"
            | "validation-failed"
            | "forbidden"
            | "idempotency-conflict"
    ) {
        return remote_unavailable(host);
    }
    ApiError {
        status: StatusCode::from_u16(rejected.status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
        code: rejected.code,
        message: rejected.message,
        details: Box::new(details),
    }
}

fn client_launch_session(state: &AppState, id: &str) -> Result<PlanningSessionView, ApiError> {
    if let Some(session) = state.store.planning_session(id).map_err(ApiError::internal)? {
        return Ok(session);
    }
    state
        .store
        .planning_session(launch_session_id(id))
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("launch `{id}` does not exist")))
}

/// A roster operation on a blocking worker. Its submission-to-start wait is recorded apart
/// from its execution, and its reader timing is attributed, only when `traced`.
fn roster_worker<T>(traced: bool, submitted: Option<Instant>, seed: Option<crate::otel::RosterAttributes>, work: impl FnOnce() -> T) -> T {
    if !traced {
        return work();
    }
    crate::otel::roster_scope(|| {
        crate::otel::seed_roster_attributes(seed);
        crate::otel::roster_worker_started(submitted);
        work()
    })
}

/// Blocking external work that does not borrow the live-store SQLite writer. Mutation polls
/// yield while this runs, so provider/RPC/git waits retain no writer capacity.
/// Keep this explicit: a callback that writes must use the admitted store path.
async fn blocking_external<T, F>(operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let profile = crate::profile::current();
    let cpu_kind = crate::performance::current();
    let trace = crate::otel::export_enabled().then(tracing::Span::current);
    tokio::task::spawn_blocking(move || {
        let _trace = trace.as_ref().map(tracing::Span::enter);
        let _entered = crate::profile::enter(profile.as_ref());
        crate::performance::with_charged(cpu_kind, operation)
    }).await.map_err(ApiError::internal)
}

async fn blocking_store<T, F>(operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    let profile = crate::profile::current();
    let cpu_kind = crate::performance::current();
    let trace = crate::otel::export_enabled().then(tracing::Span::current);
    let writer_wait = smallclaims::sqlite::telemetry::current_request();
    crate::api::read_deadline::spawn_blocking(move || {
        let _trace = trace.as_ref().map(tracing::Span::enter);
        let _writer_scope = smallclaims::sqlite::telemetry::enter_request(writer_wait);
        let _entered = crate::profile::enter(profile.as_ref());
        crate::performance::with_charged(cpu_kind, operation)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)
}

async fn blocking_action<T, F>(operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, St3Error> + Send + 'static,
{
    let profile = crate::profile::current();
    let cpu_kind = crate::performance::current();
    let trace = crate::otel::export_enabled().then(tracing::Span::current);
    let writer_wait = smallclaims::sqlite::telemetry::current_request();
    crate::api::read_deadline::spawn_blocking(move || {
        let _trace = trace.as_ref().map(tracing::Span::enter);
        let _writer_scope = smallclaims::sqlite::telemetry::enter_request(writer_wait);
        let _entered = crate::profile::enter(profile.as_ref());
        crate::performance::with_charged(cpu_kind, operation)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::bad)
}

/// Run a handler's store work on a blocking thread, so a write waiting for the writer's next
/// commit never holds an async worker that other requests need.
async fn blocking_api<T, F>(operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    let profile = crate::profile::current();
    let cpu_kind = crate::performance::current();
    let trace = crate::otel::export_enabled().then(tracing::Span::current);
    let writer_wait = smallclaims::sqlite::telemetry::current_request();
    crate::api::read_deadline::spawn_blocking(move || {
        let _trace = trace.as_ref().map(tracing::Span::enter);
        let _writer_scope = smallclaims::sqlite::telemetry::enter_request(writer_wait);
        let _entered = crate::profile::enter(profile.as_ref());
        crate::performance::with_charged(cpu_kind, operation)
    })
    .await
    .map_err(ApiError::internal)?
}

/// Bound both accept retries and diagnostics while the process or host has no descriptors.
#[derive(Default)]
struct AcceptFailures {
    resource_failures: u32,
    unreported: u64,
    last_report: Option<std::time::Instant>,
}

impl AcceptFailures {
    fn failed(
        &mut self,
        error: &std::io::Error,
        now: std::time::Instant,
    ) -> (Duration, Option<u64>) {
        self.unreported = self.unreported.saturating_add(1);
        let delay = if matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE)) {
            self.resource_failures = self.resource_failures.saturating_add(1);
            Duration::from_millis(100 * (1_u64 << self.resource_failures.saturating_sub(1).min(6)))
                .min(Duration::from_secs(2))
        } else {
            self.resource_failures = 0;
            Duration::from_millis(100)
        };
        let report = if self
            .last_report
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(5))
        {
            self.last_report = Some(now);
            Some(std::mem::take(&mut self.unreported))
        } else {
            None
        };
        (delay, report)
    }
}

pub async fn serve_unix_with_ready(
    socket: &Path,
    app: Router,
    ready: impl FnOnce(),
) -> anyhow::Result<()> {
    serve_unix_with_ancestor_ready(socket, None, app, false, harness_ancestor, ready).await
}

pub async fn serve_unix(socket: &Path, app: Router) -> anyhow::Result<()> {
    serve_unix_inner(socket, app, false).await
}

/// Make the daemon's first diagnostic report, which the operations collection lists, off the
/// request path. Until it is made, the collection says so instead of making a read wait for it.
/// A newly started report returns its operation audit for the delayed startup repair to consume;
/// later doctor calls and diagnostic refreshes always audit the current store themselves.
pub fn start_operation_report(
    state: &AppState,
) -> Option<tokio::sync::oneshot::Receiver<anyhow::Result<Vec<String>>>> {
    client_v0::start_operation_report(state)
}

/// Read the headers of this host's native session transcripts off the request path as the
/// daemon starts. Saved-history session lists and session reads use this background inventory
/// instead of walking the transcript trees, so a cold tree cannot hold those requests.
pub fn start_native_session_discovery(state: &AppState) {
    crate::external_sessions::start_history_inventory(state.native_session_home.as_deref());
}

/// The shortest pause between two roster refreshes. A refresh also pauses as long as it took,
/// so refreshing never takes more than about half a core however often readers ask. Reads
/// never wait for it: this bounds how stale a served roster can be, about a second plus a fold.
/// A read waiting for a fresh roster cuts it short to the refresh's own length.
const AGENT_ROSTER_REFRESH_PAUSE: Duration = Duration::from_secs(1);

/// The shortest pause a read waiting for a fresh roster can cut the refresher's pause to, so
/// that a very cheap fold cannot turn fresh reads into many refreshes a second.
const AGENT_ROSTER_FRESH_PAUSE: Duration = Duration::from_millis(100);

/// How long a legacy fresh or named-cut read waits for publication: long enough
/// for the refresher to finish a fold, pause as long, and fold again.
const AGENT_ROSTER_READ_WAIT: Duration = Duration::from_secs(2);

/// Keep the complete agents roster published off the request path. As the daemon starts it
/// folds every agent's refs and the cards the largest window shows, then the rest of the cards.
/// A roster read that misses its exact cut serves the newest publication under
/// that publication's own snapshot and wakes this one task, so readers neither fold the fleet's
/// cards nor wait behind a fold once the first has published. Each refresh reuses the previous
/// publication and refolds only the cards whose claims changed, when it can tell which.
pub fn start_agent_roster(state: &AppState) {
    let Some(wake) = state.store.start_agent_roster_refresher() else {
        return;
    };
    let store = state.store.clone();
    tokio::spawn(async move {
        let mut head = true;
        loop {
            let started = tokio::time::Instant::now();
            let mut admission = store.admit_agent_resources("refresher").await;
            let first = std::mem::take(&mut head);
            if !first {
                record_roster_stage(request_latency::RosterStage::RefreshAdmission, admission.waited());
            }
            let reader = store.clone();
            let mut warm_attributes = None;
            if first {
                // Warm without admission: nothing is published, and no reader folds.
                drop(admission);
                let reader = store.clone();
                let submitted = crate::otel::export_enabled().then(Instant::now);
                match tokio::task::spawn_blocking(move || crate::otel::roster_collect_warm(|| {
                    roster_worker(true, submitted, None, || {
                        let result = crate::performance::task("roster/warm", || warm_agent_roster(&reader));
                        (result, crate::otel::take_roster_attributes())
                    })
                })).await {
                    Ok((result, attributes)) => {
                        warm_attributes = attributes;
                        if let Err(error) = result {
                            eprintln!("st3: agents roster warm-up failed: {error:#}");
                        }
                    }
                    Err(error) => eprintln!("st3: agents roster warm-up stopped: {error}"),
                }
                admission = store.admit_agent_resources("refresher").await;
            }
            let submitted = crate::otel::export_enabled().then(Instant::now);
            let refreshed = tokio::task::spawn_blocking(move || roster_worker(true, submitted, warm_attributes, || {
                let _admission = admission;
                _admission.enter_worker();
                // The first fold publishes only what windows and first pages show; the rest
                // follows at once.
                crate::performance::task("roster/refresh", || if first {
                    reader.answer_agent_roster_requests(|| {
                        reader.read_snapshot(|index| client_agent_roster_head(&reader, index))
                    })
                } else {
                    // Nothing published to start from: every card folds again from the log.
                    let before = reader
                        .published_agent_roster(u64::MAX, false)
                        .map(|(cut, _, _)| cut);
                    let cold = before.is_none();
                    let folded = Instant::now();
                    let refreshed = reader.answer_agent_roster_requests(|| {
                        refresh_agent_roster(&reader, false)?;
                        if reader.take_agent_roster_history_request() {
                            refresh_agent_roster(&reader, true)?;
                        }
                        Ok(())
                    });
                    if let (Some(before), Some((cut, _, at))) =
                        (before, reader.published_agent_roster(u64::MAX, false))
                    {
                        record_publish_latency(&reader, "agents", before, cut, at);
                    }
                    record_roster_stage(if cold {
                        request_latency::RosterStage::RefreshCold
                    } else {
                        request_latency::RosterStage::Refresh
                    }, folded.elapsed());
                    refreshed
                })
            }))
            .await;
            match refreshed {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("st3: agents roster refresh failed: {error:#}"),
                Err(error) => eprintln!("st3: agents roster refresh stopped: {error}"),
            }
            if first {
                continue;
            }
            // Pause as long as the refresh took, so refreshing stays under about half a core
            // however often readers ask, and at least the minimum pause unless a reader waits
            // for a fresh roster: that read then waits for one fold, not the rest of the pause.
            let pause = started.elapsed().max(AGENT_ROSTER_FRESH_PAUSE);
            tokio::time::sleep(pause).await;
            tokio::select! {
                () = tokio::time::sleep(AGENT_ROSTER_REFRESH_PAUSE.saturating_sub(pause)) => {}
                () = store.fresh_agent_roster_wanted() => {}
            }
            wake.notified().await;
        }
    });
}

/// The local daemon binds a Unix peer to the harness identity inherited by that peer or one of
/// its parents. `state_socket` gives clients without the daemon's runtime environment a stable
/// address for the same listener. Test servers and the paired gateway use the unbound listener.
pub async fn serve_unix_bound(
    socket: &Path,
    state_socket: &Path,
    app: Router,
) -> anyhow::Result<()> {
    serve_unix_bound_with_ready(socket, state_socket, app, || {}).await
}

/// Readiness is published only once the initialized local API can accept requests.
pub async fn serve_unix_bound_with_ready(
    socket: &Path,
    state_socket: &Path,
    app: Router,
    ready: impl FnOnce(),
) -> anyhow::Result<()> {
    serve_unix_with_ancestor_ready(
        socket,
        Some(state_socket),
        app,
        true,
        harness_ancestor,
        ready,
    )
    .await
}

async fn serve_unix_inner(socket: &Path, app: Router, bind_harness: bool) -> anyhow::Result<()> {
    serve_unix_with_ancestor(socket, None, app, bind_harness, harness_ancestor).await
}

async fn serve_unix_with_ancestor(
    socket: &Path,
    state_socket: Option<&Path>,
    app: Router,
    bind_harness: bool,
    ancestor: fn(u32) -> Option<String>,
) -> anyhow::Result<()> {
    serve_unix_with_ancestor_ready(socket, state_socket, app, bind_harness, ancestor, || {}).await
}

async fn serve_unix_with_ancestor_ready(
    socket: &Path,
    state_socket: Option<&Path>,
    app: Router,
    bind_harness: bool,
    ancestor: fn(u32) -> Option<String>,
    ready: impl FnOnce(),
) -> anyhow::Result<()> {
    let reserved = reserve_unix_socket(socket, state_socket).await?;
    serve_reserved_unix_with_ancestor_ready(reserved, app, bind_harness, ancestor, ready).await
}

/// An address reservation, not a listener. Connect attempts remain refused until
/// the initialized application calls `serve_reserved_unix_with_ready`.
pub struct ReservedUnixSocket {
    socket: tokio::net::UnixSocket,
    // The existing connect probe protects a listening predecessor. This lock also
    // protects a new daemon's bound-but-not-listening address during startup.
    reservation: File,
}

pub fn unix_reservation_path(socket: &Path) -> std::path::PathBuf {
    // Discovery aliases and parent aliases must refer to the same reservation.
    let socket = fs::canonicalize(socket).unwrap_or_else(|_| {
        socket
            .parent()
            .and_then(|parent| fs::canonicalize(parent).ok())
            .zip(socket.file_name())
            .map(|(parent, name)| parent.join(name))
            .unwrap_or_else(|| socket.to_path_buf())
    });
    let mut path = socket.as_os_str().to_owned();
    path.push(".reservation.lock");
    path.into()
}

/// Bind the address and publish discovery without listening or queueing clients.
/// Retain the per-address reservation for the entire serving lifetime; never
/// unlink its lock file while another process could hold its inode.
pub async fn reserve_unix_socket(
    socket: &Path,
    state_socket: Option<&Path>,
) -> anyhow::Result<ReservedUnixSocket> {
    crate::config::validate_unix_socket_path(socket, "--socket or --client-gateway-socket")?;
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
    }
    let reservation = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(unix_reservation_path(socket))?;
    if unsafe { libc::flock(reservation.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!(
            "refusing to replace reserved Unix socket at {}: {}",
            socket.display(),
            std::io::Error::last_os_error()
        );
    }
    // A second daemon must never detach an active listener by unlinking its pathname.
    if tokio::net::UnixStream::connect(socket).await.is_ok() {
        anyhow::bail!(
            "refusing to replace live Unix socket listener at {}",
            socket.display()
        );
    }
    match fs::remove_file(socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = tokio::net::UnixSocket::new_stream()?;
    listener.bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    if let Some(state_socket) = state_socket {
        publish_state_socket(socket, state_socket)?;
    }
    Ok(ReservedUnixSocket {
        socket: listener,
        reservation,
    })
}

/// Only a fully initialized router may turn the reservation into a listener.
pub async fn serve_reserved_unix_with_ready(
    reserved: ReservedUnixSocket,
    app: Router,
    bind_harness: bool,
    ready: impl FnOnce(),
) -> anyhow::Result<()> {
    serve_reserved_unix_with_ancestor_ready(reserved, app, bind_harness, harness_ancestor, ready)
        .await
}

async fn serve_reserved_unix_with_ancestor_ready(
    reserved: ReservedUnixSocket,
    app: Router,
    bind_harness: bool,
    ancestor: fn(u32) -> Option<String>,
    ready: impl FnOnce(),
) -> anyhow::Result<()> {
    let ReservedUnixSocket {
        socket,
        reservation: _reservation,
    } = reserved;
    let listener = socket.listen(1024)?;
    // Only st3-fixture initializes this process-local state. Disable host ancestry while
    // retaining native-driver identification, which mailbox subscriptions require.
    #[cfg(feature = "test-support")]
    let bind_ancestry = bind_harness && crate::test_support::login_shell().is_none();
    #[cfg(not(feature = "test-support"))]
    let bind_ancestry = bind_harness;
    ready();
    let mut accept_failures = AcceptFailures::default();
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => {
                accept_failures.resource_failures = 0;
                stream
            }
            // Running out of file descriptors, or a peer that hung up before it was accepted,
            // fails one accept. It must not end the daemon: back off and keep serving.
            Err(error) => {
                let (delay, report) = accept_failures.failed(&error, std::time::Instant::now());
                if let Some(count) = report {
                    eprintln!(
                        "st3: accept a local API connection: {error} ({count} failures since last report; retry in {} ms)",
                        delay.as_millis()
                    );
                }
                tokio::time::sleep(delay).await;
                continue;
            }
        };
        // Every local connection names its caller, so request counts by client are always on.
        let mut diagnostic_connection = crate::relay_trace::Connection::capture();
        let peer_pid = local_peer_pid(&stream);
        let app = app.clone();
        tokio::spawn(async move {
            if let Some(connection) = &mut diagnostic_connection {
                connection.dispatched();
            }
            // /proc ancestry may fault in pages on a loaded host. Keep that work
            // out of the accept loop so a slow lookup delays only this peer.
            if let Some(connection) = &mut diagnostic_connection {
                connection.queued();
            }
            let (bound_agent, caller, delivery_peer, diagnostic_connection) = match peer_pid {
                Some(pid) => crate::api::read_deadline::spawn_blocking(move || {
                    if let Some(connection) = &mut diagnostic_connection {
                        connection.started();
                    }
                    let bound_agent = bind_ancestry.then(|| ancestor(pid)).flatten();
                    let caller = Some(crate::profile::Caller::of_command(
                        local_process_arguments(pid).map(|(arguments, _)| arguments),
                        bound_agent.as_deref(),
                    ));
                    let delivery_peer = bind_harness.then(|| native_delivery_peer(pid)).flatten();
                    if let Some(connection) = &mut diagnostic_connection {
                        connection.finished();
                    }
                    (bound_agent, caller, delivery_peer, diagnostic_connection)
                })
                .await
                .unwrap_or_default(),
                None => (None, None, None, diagnostic_connection),
            };
            let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                let app = app.clone();
                let bound_agent = bound_agent.clone();
                let caller = caller.clone();
                let delivery_peer = delivery_peer.clone();
                async move {
                    let mut request = request.map(Body::new);
                    if let Some(connection) = diagnostic_connection {
                        request.extensions_mut().insert(connection);
                    }
                    if let Some(pid) = peer_pid {
                        request.extensions_mut().insert(LocalPeer {
                            pid,
                            ancestor: bound_agent.clone(),
                        });
                    }
                    if let Some(peer) = delivery_peer {
                        request.extensions_mut().insert(peer);
                    }
                    if let Some(caller) = caller {
                        request.extensions_mut().insert(caller);
                    }
                    if let Some(agent) = &bound_agent {
                        request.extensions_mut().insert(BoundAgent(agent.clone()));
                    }
                    let client_request = request.uri().path().starts_with("/v1/client/");
                    let request = match guard_bound_request(request, bound_agent.as_deref()).await {
                        Ok(request) => request,
                        Err(error) => {
                            let response = if client_request {
                                let status = error.status;
                                let raw = json!({"code":error.code,"message":error.message,"details":error.details});
                                (
                                    status,
                                    Json(client_error_envelope(
                                        status,
                                        &raw,
                                        &format!("request/{}", new_request_id()),
                                    )),
                                )
                                    .into_response()
                            } else {
                                error.into_response()
                            };
                            return Ok::<_, std::convert::Infallible>(response);
                        }
                    };
                    app.oneshot(request).await
                }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
    }
}
/// Atomically replace only the discovery link, never the listener itself. On macOS (and when
/// XDG_RUNTIME_DIR is absent) the default listener already lives at this state path.
fn publish_state_socket(socket: &Path, state_socket: &Path) -> anyhow::Result<()> {
    if socket == state_socket
        || fs::canonicalize(state_socket).ok() == fs::canonicalize(socket).ok()
    {
        return Ok(());
    }
    let parent = state_socket
        .parent()
        .ok_or_else(|| anyhow::anyhow!("state socket has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let temporary = state_socket.with_extension(format!("sock.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let target = fs::canonicalize(socket)?;
    std::os::unix::fs::symlink(target, &temporary)?;
    if let Err(error) = fs::rename(&temporary, state_socket) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod gateway_listener_tests;

#[cfg(target_os = "linux")]
fn harness_ancestor(mut pid: u32) -> Option<String> {
    use std::collections::BTreeSet;
    let mut seen = BTreeSet::new();
    while pid > 1 && pid != std::process::id() && seen.insert(pid) {
        let environment = fs::read(format!("/proc/{pid}/environ")).ok()?;
        if let Some(agent) = environment.split(|byte| *byte == 0).find_map(|entry| {
            std::str::from_utf8(entry)
                .ok()?
                .strip_prefix("ST_AGENT=")
                .filter(|value| value.starts_with("agent/"))
                .map(str::to_owned)
        }) {
            return Some(agent);
        }
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        pid = stat
            .rsplit_once(") ")?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?;
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn harness_ancestor(_pid: u32) -> Option<String> {
    None
}

#[derive(Clone)]
struct NativeDeliveryPeer {
    agent: String,
    transport: &'static str,
    pid: u32,
    archives_inbox: bool,
}

fn native_delivery_peer(pid: u32) -> Option<NativeDeliveryPeer> {
    let (args, env) = local_process_arguments(pid)?;
    native_delivery_identity(pid, &args, &env)
}

fn native_delivery_identity(
    pid: u32,
    args: &[String],
    env: &[String],
) -> Option<NativeDeliveryPeer> {
    let (transport, archives_inbox) = args.windows(2).find_map(|pair| {
        if pair[0] != "driver" {
            return None;
        }
        match pair[1].as_str() {
            "omp-channel" | "omp" => Some(("omp-channel", false)),
            "pi-channel" | "pi" => Some(("pi-channel", false)),
            "claude-mcp" => Some(("claude-channel", false)),
            "claude" => Some(("claude-channel", true)),
            "codex" => Some(("app-server", true)),
            "opencode" => Some(("opencode-server", true)),
            _ => None,
        }
    })?;
    let agent = env.iter().find_map(|entry| {
        entry
            .strip_prefix("ST_AGENT=agent/")
            .map(|suffix| format!("agent/{suffix}"))
    })?;
    Some(NativeDeliveryPeer {
        agent,
        transport,
        pid,
        archives_inbox,
    })
}

#[cfg(target_os = "linux")]
fn local_peer_pid(stream: &tokio::net::UnixStream) -> Option<u32> {
    stream
        .peer_cred()
        .ok()?
        .pid()
        .and_then(|pid| u32::try_from(pid).ok())
}

#[cfg(target_os = "macos")]
fn local_peer_pid(stream: &tokio::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd as _;
    let mut pid: libc::pid_t = 0;
    let mut size = std::mem::size_of_val(&pid) as libc::socklen_t;
    // Darwin's getpeereid reports uid/gid only; LOCAL_PEERPID identifies this connection's peer.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut size,
        )
    };
    (result == 0).then(|| u32::try_from(pid).ok()).flatten()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn local_peer_pid(_stream: &tokio::net::UnixStream) -> Option<u32> {
    None
}

#[cfg(target_os = "linux")]
fn local_process_arguments(pid: u32) -> Option<(Vec<String>, Vec<String>)> {
    let split = |bytes: Vec<u8>| {
        bytes
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect()
    };
    Some((
        split(fs::read(format!("/proc/{pid}/cmdline")).ok()?),
        split(fs::read(format!("/proc/{pid}/environ")).ok()?),
    ))
}

#[cfg(target_os = "macos")]
fn local_process_arguments(pid: u32) -> Option<(Vec<String>, Vec<String>)> {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        i32::try_from(pid).ok()?,
    ];
    let mut size = 0;
    // Obtain the kernel's bounded argv/environment buffer for this same-user process.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size > 1024 * 1024
    {
        return None;
    }
    let mut bytes = vec![0u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    bytes.truncate(size);
    let count = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
    let mut tail = bytes.get(4..)?;
    tail = tail.get(tail.iter().position(|byte| *byte == 0)? + 1..)?; // executable path
    let padding = tail.iter().take_while(|byte| **byte == 0).count();
    let mut parts = tail[padding..].split(|byte| *byte == 0);
    let args = (0..usize::try_from(count).ok()?)
        .map(|_| {
            parts
                .next()
                .map(|part| String::from_utf8_lossy(part).into_owned())
        })
        .collect::<Option<Vec<_>>>()?;
    let env = parts
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    Some((args, env))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn local_process_arguments(_pid: u32) -> Option<(Vec<String>, Vec<String>)> {
    None
}

fn record_legacy_poll(
    peer: Option<&NativeDeliveryPeer>,
    recipient: Option<&str>,
    include_closed: bool,
) {
    // Older outer drivers project closed messages too, so they can archive
    // native inbox files. Their poll still proves liveness. Channel processes
    // have no archive projection; a history query from one does not count.
    if let Some(peer) = peer
        && recipient == Some(peer.agent.as_str())
        && (!include_closed || peer.archives_inbox)
    {
        delivery_presence::record_legacy(&peer.agent, peer.transport, peer.pid);
    }
}

/// The agent whose harness a local request comes from, when it comes from one.
#[derive(Clone, Debug)]
pub struct BoundAgent(pub String);

async fn guard_bound_request(
    request: Request<Body>,
    bound_agent: Option<&str>,
) -> Result<Request<Body>, ApiError> {
    let Some(bound_agent) = bound_agent else {
        return Ok(request);
    };
    if request.method() == axum::http::Method::GET
        && !client_v0::administrative_get(&request) {
        return Ok(request);
    }
    // Free mode lets an agent do what its person may do, but always as itself: a harness never
    // names a person, or another agent, as the local client-v0 actor.
    if let Some(person) = request
        .headers()
        .get(client_v0::LOCAL_PERSON_HEADER)
        .and_then(|value| value.to_str().ok())
        && person != bound_agent
    {
        return Err(ApiError::bad(St3Error::new(
            "foreign-agent-actor",
            format!("this harness is `{bound_agent}` and cannot act as `{person}`"),
        )));
    }
    let path = request.uri().path();
    // A forwarded client read carries a person's authority between fleet members. Only the
    // replication worker, which runs in no harness, hands one over.
    if path.starts_with(crate::peer::CLIENT_READ_FORWARD_PATH) {
        return Err(ApiError::bad(St3Error::new(
            "foreign-agent-actor",
            format!("this harness is `{bound_agent}` and cannot forward a person's client read"),
        )));
    }
    if ![
        "/v1/intent/apply",
        "/v1/sets/",
        "/v1/agent-queue-moves",
        "/v1/agents/rename",
        "/v1/agents/restart",
        "/v1/agents/rollout",
        "/v1/agents/start",
        "/v1/agents/source-offline",
        "/v1/agents/suspend",
        "/v1/agents/resume",
        "/v1/agents/native-session",
        "/v1/delivery/hold",
        "/v1/lane-changes",
        "/v1/work/",
        "/v1/github/",
        "/v1/attention",
        "/v1/launches",
        "/v1/mission-runs/",
        "/v1/missions/",
        "/v1/subscription-requests/",
        "/v1/reviews/",
        "/v1/claims",
        "/v1/messages/",
        "/v1/schema/registrations",
        "/v1/custom/reply",
        "/v1/diagnostic",
        "/v1/rules/",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix))
    {
        return Ok(request);
    }
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 16 * 1024 * 1024)
        .await
        .map_err(ApiError::internal)?;
    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
        for key in ["actor", "requester"] {
            if let Some(actor) = value.get(key).and_then(Value::as_str)
                && actor != bound_agent
            {
                return Err(ApiError::bad(St3Error::new(
                    "foreign-agent-actor",
                    format!("this harness is `{bound_agent}` and cannot act as `{actor}`"),
                )));
            }
        }
    }
    Ok(Request::from_parts(parts, Body::from(bytes)))
}

pub async fn serve_tcp(address: &str, app: Router) -> anyhow::Result<()> {
    let address = address.parse::<std::net::SocketAddr>()?;
    anyhow::ensure!(
        crate::fleet::transport::is_permitted_route_address(&address.ip()),
        "the peer listener must bind to a loopback or Tailscale address"
    );
    let listener = TcpListener::bind(address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}


async fn health(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({
        "status": "ready",
        "node": state.node,
        "version": env!("CARGO_PKG_VERSION"),
        "build": {"commit": st_drivers::version::build_commit()},
        "isolation": isolation_name(st_runtime::isolation_mode()),
        "store_index": state.store.index().map_err(ApiError::internal)?,
        "security": "trusted-network-no-tls-no-acls",
        "features": {"owned_sets":1,"seat_rollout":1,"seat_rollout_manual":1,"bounded_legacy_events":1,"person_directive_note":1},
    })))
}

fn isolation_name(mode: st_runtime::Isolation) -> &'static str {
    match mode {
        st_runtime::Isolation::Scope => "scope",
        st_runtime::Isolation::Detached => "detached",
        st_runtime::Isolation::DegradedDetached => "degraded-detached",
    }
}

/// A planning reserve for schema/statements, projections, tasks and allocator overhead, not
/// an enforced or measured limit. Active bursts can exceed idle retention and this envelope.
const DAEMON_MEMORY_HEADROOM_KIB: u128 = 512 * 1024;

fn reader_memory_check(
    readers: smallclaims::sqlite::ReaderUsage,
    cache_kib: usize,
    retained: usize,
    read_workers: usize,
    groups: anyhow::Result<Option<Vec<crate::memory::GroupMemory>>>,
) -> DoctorCheck {
    let targets = readers.open as u128 * cache_kib as u128;
    let envelope_kib = readers.open.max(retained) as u128 * cache_kib as u128
        + smallclaims::sqlite::WRITE_CACHE_KIB as u128
        + DAEMON_MEMORY_HEADROOM_KIB;
    let mut message = format!(
        "{} open readers ({} idle, {} active), peak {}, {} opened since startup; per-reader cache target {} KiB; current reader targets {} MiB; idle retention {}; API read-worker admission {}; planning envelope {} MiB (reader targets + 32 MiB writer + 512 MiB reserve). Raw/background reader checkout is uncapped; cache targets exclude schema, prepared statements and query results, and this envelope is not a process memory limit",
        readers.open,
        readers.idle,
        readers.open.saturating_sub(readers.idle),
        readers.peak,
        readers.opened,
        cache_kib,
        targets / 1024,
        retained,
        read_workers,
        envelope_kib / 1024,
    );
    let mut warned = false;
    match groups {
        Ok(Some(groups)) => {
            if let Some(group) = groups
                .iter()
                .filter(|group| group.max_bytes.is_some())
                .min_by_key(|group| group.max_bytes.unwrap())
            {
                let max = group.max_bytes.unwrap();
                message.push_str(&format!(
                    "; cgroup memory.max {} MiB at {}",
                    max / (1024 * 1024),
                    group.path.display()
                ));
                if u128::from(max) < envelope_kib * 1024 {
                    warned = true;
                    message.push_str(" is below the planning envelope; review the service cap and reader cache/retention settings");
                }
            } else {
                message.push_str("; cgroup memory.max is unlimited in the visible hierarchy");
            }
            for group in groups.iter().filter(|group| group.max_events > 0) {
                warned = true;
                message.push_str(&format!("; memory.events:max {} at {} (historical pressure in this cgroup, including descendants; not proof of a current OOM)", group.max_events, group.path.display()));
            }
        }
        Ok(None) => message.push_str("; cgroup memory diagnostics are Linux-only"),
        Err(error) => {
            warned = true;
            message.push_str(&format!("; cannot inspect daemon cgroup memory: {error:#}"));
        }
    }
    DoctorCheck {
        name: "reader-memory".into(),
        status: if warned { "warn" } else { "pass" }.into(),
        message,
    }
}

fn descriptor_usage_check(soft: u64, hard: u64, usage: Option<u64>) -> DoctorCheck {
    let low = soft < 1024;
    let full = usage.is_some_and(|usage| u128::from(usage) * 5 >= u128::from(soft) * 4);
    let used = usage.map_or_else(|| "usage unavailable".into(), |used| format!("{used} open"));
    DoctorCheck {
        name: "file-descriptors".into(),
        status: if low || full || usage.is_none() {
            "warn"
        } else {
            "pass"
        }
        .into(),
        message: format!(
            "{used}; soft limit {soft}, hard limit {hard}{}",
            if low {
                "; low soft limit: raise the service's descriptor limit and restart the daemon"
            } else if full {
                "; at least 80% in use: inspect descriptor growth before accepting more connections"
            } else {
                ""
            }
        ),
    }
}

fn first_readiness_scheduler_check(store: &Store, node: &str) -> DoctorCheck {
    let (status, message) = match store.open_reconcile_faults(node) {
        Ok(faults) => {
            let reasons = faults
                .into_iter()
                .filter(|((_, scope), _)| scope == crate::reconcile::FIRST_READINESS_FAULT_SCOPE)
                .map(|(_, reason)| reason)
                .collect::<Vec<_>>();
            if reasons.is_empty() {
                (
                    "pass",
                    "no unresolved first-readiness scheduler faults".into(),
                )
            } else {
                ("warn", reasons.join("; "))
            }
        }
        Err(error) => ("warn", format!("cannot read scheduler faults: {error:#}")),
    };
    DoctorCheck {
        name: "mission-first-readiness".into(),
        status: status.into(),
        message,
    }
}

/// Show what spends the GitHub budget that every observer on every host shares: the budget
/// GitHub last reported, how much of its window this host's observers spent, and each observer's
/// requests.
fn github_usage_checks(usage: &crate::resource::GithubUsageReport, now: u128) -> Vec<DoctorCheck> {
    let time = |unix_ms: u128| {
        chrono::DateTime::from_timestamp_millis(i64::try_from(unix_ms).unwrap_or(i64::MAX))
            .unwrap_or(chrono::DateTime::UNIX_EPOCH)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    };
    if usage.budgets.is_empty() && usage.spenders.is_empty() {
        return vec![DoctorCheck {
            name: "github-budget".into(),
            status: "pass".into(),
            message: "this daemon has sent no GitHub request since it started".into(),
        }];
    }
    let mut checks = Vec::new();
    for report in &usage.budgets {
        let budget = &report.budget;
        let message = if budget.reset_at_unix_ms <= now {
            format!(
                "GitHub last reported {} of {} requests left at {}, in a window that reset at {}",
                budget.remaining,
                budget.limit,
                time(budget.reported_at_unix_ms),
                time(budget.reset_at_unix_ms)
            )
        } else if budget.resource == "core" {
            format!(
                "{} of {} requests left until {}; observers on this host sent {} of the {} counted in this window, other hosts and clients of the token (gh, CI) the rest; reported {}",
                budget.remaining,
                budget.limit,
                time(budget.reset_at_unix_ms),
                report.counted_here.min(budget.used),
                budget.used,
                time(budget.reported_at_unix_ms)
            )
        } else {
            format!(
                "{} of {} requests left until {}; reported {}",
                budget.remaining,
                budget.limit,
                time(budget.reset_at_unix_ms),
                time(budget.reported_at_unix_ms)
            )
        };
        // Under a tenth left, the observers are close to backing off until the reset.
        let low = budget.reset_at_unix_ms > now
            && u128::from(budget.remaining) * 10 < u128::from(budget.limit);
        checks.push(DoctorCheck {
            name: format!("github-budget/{}", budget.resource),
            status: if low { "warn" } else { "pass" }.into(),
            message,
        });
    }
    for spender in &usage.spenders {
        checks.push(DoctorCheck {
            name: format!("github-requests/{}", spender.spender),
            status: "pass".into(),
            message: format!(
                "{} counted requests in the last hour; since the daemon started {} sent, {} not modified (free), {} refused; last sent {}",
                spender.counted_last_hour,
                spender.sent,
                spender.not_modified,
                spender.refused,
                time(spender.last_sent_at_unix_ms)
            ),
        });
    }
    checks
}

/// Every person's open attention items that have waited more than a day, oldest first.
/// Every person's attention, then every fault under the agent that owns it, so an old fault
/// that no agent took up still shows in doctor.
fn doctor_attention_items(
    store: &Store,
    now: u128,
) -> anyhow::Result<Vec<crate::model::AttentionItemView>> {
    let mut items = store.attention_snapshot(None, now)?;
    items.extend(store.fault_snapshot(now)?.into_iter().map(|fault| {
        crate::model::AttentionItemView {
            person: fault.owner.unwrap_or_else(|| "no owning agent".into()),
            ..fault.item
        }
    }));
    Ok(items)
}

fn stale_attention_check(items: &[crate::model::AttentionItemView], now: u128) -> DoctorCheck {
    const DAY_MS: u128 = 86_400_000;
    const LISTED: usize = 20;
    let mut stale = items
        .iter()
        .filter(|item| now.saturating_sub(item.requested_at_unix_ms) > DAY_MS)
        .collect::<Vec<_>>();
    if stale.is_empty() {
        return DoctorCheck {
            name: "attention-age".into(),
            status: "pass".into(),
            message: "no alert or fault has been open for more than a day".into(),
        };
    }
    stale.sort_by(|left, right| {
        (left.requested_at_unix_ms, &left.subject)
            .cmp(&(right.requested_at_unix_ms, &right.subject))
    });
    let mut listed = stale
        .iter()
        .take(LISTED)
        .map(|item| {
            let hours = now.saturating_sub(item.requested_at_unix_ms) / 3_600_000;
            format!(
                "{} for {}, open {}d {}h: {}",
                item.subject,
                item.person,
                hours / 24,
                hours % 24,
                item.title
            )
        })
        .collect::<Vec<_>>();
    if stale.len() > LISTED {
        listed.push(format!("and {} more", stale.len() - LISTED));
    }
    DoctorCheck {
        name: "attention-age".into(),
        status: "warn".into(),
        message: format!(
            "{} alerts and faults have been open for more than a day; `st alerts ls --as PERSON` shows how to close a person's alert, and a fault closes at its source: {}",
            stale.len(),
            listed.join("; ")
        ),
    }
}

fn daemon_pty(state: &AppState) -> anyhow::Result<st_runtime::PtyRuntime> {
    Ok(st_runtime::PtyRuntime::new(state.pty_root.clone())
        .with_binary(state.pty_binary.to_string_lossy())
        .with_environment(crate::environment::snapshot()?))
}

/// The references already in the graph that no longer resolve. Publication refuses new ones, so
/// each of these was published before that check, or its target was removed later.
/// Every Claude seat this host runs needs its hooks to run st3, and each running Claude session
/// needs the native-session binding its SessionStart hook writes; without it st cannot find the
/// seat's transcript.
fn claude_hooks_check(
    state: &AppState,
    desired: &[crate::model::DesiredSubject],
) -> Result<DoctorCheck, ApiError> {
    let seats = desired
        .iter()
        .filter(|subject| {
            subject.member.as_ref().is_some_and(|member| {
                member.host == state.store.origin() && member.driver.as_deref() == Some("claude")
            })
        })
        .map(|subject| subject.subject.as_str())
        .collect::<Vec<_>>();
    if seats.is_empty() {
        return Ok(DoctorCheck {
            name: "claude-hooks".into(),
            status: "pass".into(),
            message: "no Claude seat runs on this host".into(),
        });
    }
    let mut faults = Vec::new();
    let binary = crate::reconcile::st_binary_link(&state.state_dir);
    if !is_executable_file(&binary) {
        faults.push(format!(
            "the hooks run ST3_BIN={}, which is not an executable file",
            binary.display()
        ));
    }
    let set = crate::hooks::set_dir(&crate::hooks::root(&state.state_dir));
    if let Err(error) = crate::hooks::verify(&set) {
        faults.push(format!(
            "the hook set {} is not usable: {error:#}",
            set.display()
        ));
    }
    let drivers = state.state_dir.join("drivers");
    let host = st_drivers::run::detect_host();
    let mut bound = 0;
    for subject in &seats {
        let Some(observed) = state
            .store
            .latest_claim(subject, Some("harness.observed"))
            .map_err(ApiError::internal)?
        else {
            continue;
        };
        let fields = observed.body.get("fields").unwrap_or(&observed.body);
        if fields["driver"] != "claude" || matches!(fields["state"].as_str(), Some("ended")) {
            continue;
        }
        let Some(session) = fields["evidence_incarnation"].as_str() else {
            continue;
        };
        let agent_dir = crate::hooks::claude_agent_dir(&drivers, subject, &host);
        if crate::hooks::claude_binding(&agent_dir, session).is_some() {
            bound += 1;
        } else {
            faults.push(format!(
                "{subject}: Claude session {session} has no native-session binding in {}; its SessionStart hook did not run st3",
                agent_dir.display()
            ));
        }
    }
    Ok(DoctorCheck {
        name: "claude-hooks".into(),
        status: if faults.is_empty() { "pass" } else { "fail" }.into(),
        message: if faults.is_empty() {
            format!(
                "{} Claude seats; the hooks run st3 from {}; {bound} running sessions are bound to their transcripts",
                seats.len(),
                set.display()
            )
        } else {
            faults.join("; ")
        },
    })
}

fn claude_usage_check(
    state: &AppState,
    desired: &[crate::model::DesiredSubject],
) -> Result<DoctorCheck, ApiError> {
    let mut faults = Vec::new();
    let host = st_drivers::run::detect_host();
    for seat in desired.iter().filter(|seat| {
        seat.member.as_ref().is_some_and(|member| {
            member.host == state.store.origin() && member.driver.as_deref() == Some("claude")
        })
    }) {
        let Some(observed) = state
            .store
            .latest_claim(&seat.subject, Some("harness.observed"))
            .map_err(ApiError::internal)?
        else {
            continue;
        };
        let fields = observed.body.get("fields").unwrap_or(&observed.body);
        if fields["driver"] != "claude" || fields["state"] == "ended" {
            continue;
        }
        let Some(incarnation) = fields["evidence_incarnation"].as_str() else {
            continue;
        };
        let agent_dir =
            crate::hooks::claude_agent_dir(&state.state_dir.join("drivers"), &seat.subject, &host);
        if let Some(warning) =
            st_drivers::claude_session::usage_observation_warning(&agent_dir, incarnation)
        {
            faults.push(format!("{}: {warning}", seat.subject));
        }
    }
    Ok(DoctorCheck {
        name: "claude-usage-observation".into(),
        status: if faults.is_empty() { "pass" } else { "warn" }.into(),
        message: if faults.is_empty() {
            "no current Claude native usage observer reports a fault; acquisition completeness requires source reconciliation".into()
        } else {
            faults.join("; ")
        },
    })
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// How many claims' signatures verify. Verdicts are recorded, not enforced: a held or invalid
/// one warns, and unsigned claims (written before signing, or by an older build) are counted.
fn claim_signatures_check(counts: &std::collections::BTreeMap<String, u64>) -> DoctorCheck {
    let count = |verdict: &str| counts.get(verdict).copied().unwrap_or_default();
    let (held, invalid) = (count("held"), count("invalid"));
    DoctorCheck {
        name: "claim-signatures".into(),
        status: if held + invalid == 0 { "pass" } else { "warn" }.into(),
        message: format!(
            "{} verified, {} unsigned, {} not yet sealed, {held} waiting for a delegation, {invalid} invalid",
            count("verified"),
            count("unsigned"),
            count("unsealed"),
        ),
    }
}

fn graph_references_check(unresolved: &[String]) -> DoctorCheck {
    const LISTED: usize = 20;
    let mut listed = unresolved.iter().take(LISTED).cloned().collect::<Vec<_>>();
    if unresolved.len() > LISTED {
        listed.push(format!("and {} more", unresolved.len() - LISTED));
    }
    DoctorCheck {
        name: "graph-references".into(),
        status: if unresolved.is_empty() {
            "pass"
        } else {
            "warn"
        }
        .into(),
        message: if unresolved.is_empty() {
            "every reference in the ready missions, declarations and active runs resolves".into()
        } else {
            let (noun, verb) = if unresolved.len() == 1 {
                ("reference", "resolves")
            } else {
                ("references", "resolve")
            };
            format!(
                "{} {noun} no longer {verb}: {}",
                unresolved.len(),
                listed.join("; ")
            )
        },
    }
}

fn publication_evidence_check(store: &Store) -> DoctorCheck {
    match store.publication_evidence_ready() {
        Ok(true) => DoctorCheck { name:"publication-evidence".into(), status:"pass".into(),
            message:"retained runtime and incarnation launch evidence is complete".into() },
        Ok(false) => DoctorCheck { name:"publication-evidence-incomplete".into(), status:"warn".into(),
            message:"publication evidence bootstrap or pending canonical deltas are incomplete; guarded publications refuse until complete; inspect daemon bootstrap errors if progress stops".into() },
        Err(error) => DoctorCheck { name:"publication-evidence-unavailable".into(), status:"warn".into(), message:error.to_string() },
    }
}

/// Held silent mail is never lost: unread past a day, it is listed for its seat.
fn held_mail_check(store: &Store, now: u128) -> DoctorCheck {
    match store.coordination_backfill_status() {
        Ok((cursor, ceiling, false, progress))
            if now.saturating_sub(u128::from(progress)) > 15 * 60 * 1000 =>
        {
            return DoctorCheck {
                name: "held-mail".into(),
                status: "warn".into(),
                message: format!(
                    "held-mail metadata bootstrap stalled for over 15 minutes at {cursor}/{ceiling}; inspect daemon bootstrap errors and repair the source before publishing counts"
                ),
            };
        }
        Err(error) => {
            return DoctorCheck {
                name: "held-mail".into(),
                status: "warn".into(),
                message: error.to_string(),
            };
        }
        _ => {}
    }
    match store.held_mail_before(now.saturating_sub(crate::silent::HELD_TOO_LONG_MS)) {
        Ok(seats) if seats.is_empty() => DoctorCheck {
            name: "held-mail".into(),
            status: "pass".into(),
            message: if store
                .coordination_backfill_status()
                .is_ok_and(|state| !state.2)
            {
                "held-mail metadata bootstrap is progressing; this diagnostic is partial until complete".into()
            } else {
                "no seat holds silent mail unread for over a day".into()
            },
        },
        Ok(seats) => DoctorCheck {
            name: "held-mail".into(),
            status: "warn".into(),
            message: format!(
                "silent mail unread for over a day (at most 128 oldest subjects per check): {}; read it with `st conversations ls --as SEAT`",
                seats
                    .iter()
                    .map(|(seat, count, oldest)| format!(
                        "{seat}: {count}, oldest {}h",
                        now.saturating_sub(*oldest) / 3_600_000
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        },
        Err(error) => DoctorCheck {
            name: "held-mail".into(),
            status: "warn".into(),
            message: error.to_string(),
        },
    }
}

fn unread_current_seat_counts(
    store: &Store,
    recipients: &BTreeSet<&str>,
    messages: &[MessageView],
    now: u128,
) -> anyhow::Result<(usize, usize)> {
    let mut pending = 0;
    let mut accepted = 0;
    for message in messages.iter().filter(|message| {
        recipients.contains(message.to.as_str())
            && !matches!(message.status.as_str(), "read" | "closed")
            // Held mail is not late: it waits for the seat's next turn.
            && !crate::silent::waits_for_turn(message)
    }) {
        let owners = store.desired_subjects_named(std::slice::from_ref(&message.to))?;
        if let Some(host) = owners
            .first()
            .and_then(|owner| owner.member.as_ref())
            .map(|member| member.host.as_str())
            && host != store.origin()
            && !store.replication_peer_up(host)?.0
        {
            continue;
        }
        let claims = store.claims_for(&message.subject, Some("message.sent"))?;
        let sent_at = claims
            .first()
            .map(|claim| claim.accepted_at_unix_ms)
            .unwrap_or_default();
        if now.saturating_sub(sent_at) > 10_000 {
            pending += 1;
            accepted += usize::from(message.status == "delivered");
        }
    }
    Ok((pending, accepted))
}
fn terminal_exec_gates_check(store: &Store) -> anyhow::Result<DoctorCheck> {
    fn has_exit_code_gate(mission: &crate::model::MissionSpec) -> bool {
        let has_gate = |gates: &[crate::model::GateSpec]| {
            gates.iter().any(|gate| {
                matches!(gate, crate::model::GateSpec::Field { path, .. } if path == "exit_code")
            })
        };
        has_gate(&mission.gates)
            || mission.steps.values().any(|step| {
                has_gate(&step.gates)
                    || step
                        .nested_mission
                        .as_deref()
                        .is_some_and(has_exit_code_gate)
            })
    }
    let runs = store.open_mission_run_headers()?;
    let missions = store.mission_specs_for_runs(
        &runs
            .iter()
            .map(|run| run.subject.clone())
            .collect::<Vec<_>>(),
    )?;
    let mut stuck = Vec::new();
    for run in runs {
        let Some(mission) = missions.get(&run.subject) else {
            continue;
        };
        if !has_exit_code_gate(mission) {
            continue;
        }
        // Detail-free step states suffice; do not hydrate wake, timing or loop histories.
        if let Some(run) = store.mission_run_steps(&run.subject, false)? {
            stuck.extend(crate::reconcile::stuck_field_gates(store, &run, mission)?);
        }
    }
    stuck.sort();
    Ok(DoctorCheck {
        name: "terminal-exec-gates".into(),
        status: if stuck.is_empty() { "pass" } else { "warn" }.into(),
        message: if stuck.is_empty() {
            "no unresolved field gates wait on execs that ended without a restart".into()
        } else {
            format!(
                "stuck gates: {}; inspect `st missions show RUN`",
                stuck.join("; ")
            )
        },
    })
}

fn doctor_report(state: &AppState) -> Result<Json<DoctorReport>, ApiError> {
    let drift = state.store.operation_projection_drift();
    doctor_report_with_operation_drift(state, &drift)
}

fn doctor_report_with_operation_drift(
    state: &AppState,
    drift: &anyhow::Result<Vec<String>>,
) -> Result<Json<DoctorReport>, ApiError> {
    let mut checks = Vec::new();
    let auto = state.store.latest_claim(&format!("daemon/{}", state.node), Some("daemon.started"))
        .map_err(ApiError::internal)?
        .is_some_and(|claim| claim.body.pointer("/fields/features/harness_access_auto").and_then(Value::as_u64) == Some(1));
    let access_mode = if auto { "auto" } else { "bypass" };
    checks.push(DoctorCheck {
        name: "harness-access-mode".into(), status: "pass".into(),
        message: format!("{access_mode}: managed provider permissions; auto is not a security boundary: it reduces accidents, it does not isolate seats; real sandboxing is separate later work; legacy authored options remain authoritative"),
    });

    match state.store.index() {
        Ok(index) => checks.push(DoctorCheck {
            name: "claim-store".into(),
            status: "pass".into(),
            message: format!("the claim store is readable at index {index}"),
        }),
        Err(error) => checks.push(DoctorCheck {
            name: "claim-store".into(),
            status: "fail".into(),
            message: error.to_string(),
        }),
    }
    match state.store.claim_verdict_counts() {
        Ok(counts) => checks.push(claim_signatures_check(&counts)),
        Err(error) => checks.push(DoctorCheck {
            name: "claim-signatures".into(),
            status: "fail".into(),
            message: error.to_string(),
        }),
    }
    match drift {
        Ok(drift) if drift.is_empty() => checks.push(DoctorCheck {
            name: "operation-projection".into(),
            status: "pass".into(),
            message: "the operation projection matches the claim log".into(),
        }),
        Ok(drift) => checks.push(DoctorCheck {
            name: "operation-projection".into(),
            status: "fail".into(),
            message: format!("operation projection drift: {}", drift.join(", ")),
        }),
        Err(error) => checks.push(DoctorCheck {
            name: "operation-projection".into(),
            status: "fail".into(),
            message: error.to_string(),
        }),
    }
    match state.store.operational_repair_plan() {
        Ok(plan) if plan.items.is_empty() => checks.push(DoctorCheck {
            name: "operational-repair".into(),
            status: "pass".into(),
            message: "the graph has no repairable operational contradictions".into(),
        }),
        Ok(plan) => checks.push(DoctorCheck {
            name: "operational-repair".into(),
            status: "warn".into(),
            message: format!(
                "{} graph-authorized repairs are available; inspect `st repair dry-run` token {}",
                plan.items.len(),
                plan.token
            ),
        }),
        Err(error) => checks.push(DoctorCheck {
            name: "operational-repair".into(),
            status: "fail".into(),
            message: format!("could not compute the operational repair plan: {error}"),
        }),
    }
    checks.push(
        terminal_exec_gates_check(&state.store).unwrap_or_else(|error| DoctorCheck {
            name: "terminal-exec-gates".into(),
            status: "warn".into(),
            message: format!("could not inspect terminal exec gates: {error}"),
        }),
    );
    checks.push(
        state
            .store
            .account_limits_check(client_now_ms(), 3_600_000)
            .unwrap_or_else(|error| DoctorCheck {
                name: "account-limits".into(),
                status: "warn".into(),
                message: format!("could not inspect weekly account readings: {error}"),
            }),
    );
    match tempfile::Builder::new()
        .prefix(".st3-doctor-")
        .tempfile_in(&state.state_dir)
    {
        Ok(_) => checks.push(DoctorCheck {
            name: "state-directory".into(),
            status: "pass".into(),
            message: format!("{} is a writable directory", state.state_dir.display()),
        }),
        Err(error) => checks.push(DoctorCheck {
            name: "state-directory".into(),
            status: "fail".into(),
            message: format!("cannot write {}: {error}", state.state_dir.display()),
        }),
    }
    checks.push(match crate::disk::disk_space(&state.state_dir) {
        Ok(space) => DoctorCheck {
            name: "disk-space".into(),
            status: if space.is_low() { "warn" } else { "pass" }.into(),
            message: format!(
                "{} on the filesystem of {}",
                space.describe(),
                state.state_dir.display()
            ),
        },
        Err(error) => DoctorCheck {
            name: "disk-space".into(),
            status: "warn".into(),
            message: format!(
                "cannot read free space for {}: {error}",
                state.state_dir.display()
            ),
        },
    });
    let desired = state.store.desired_subjects().map_err(ApiError::internal)?;
    for subject in &desired {
        if subject.kind != "stop"
            && !subject
                .member
                .as_ref()
                .is_some_and(|member| member.host == state.store.origin())
        {
            continue;
        }
        if let Some(fault) = state
            .store
            .member_reconcile_fault(&subject.subject, None)
            .map_err(ApiError::internal)?
        {
            checks.push(DoctorCheck {
                name: format!("member-reconcile/{}", subject.subject),
                status: "fail".into(),
                message: fault,
            });
        }
    }
    let terminal_required = desired.iter().any(|subject| {
        subject
            .member
            .as_ref()
            .is_some_and(|member| member.terminal)
    });
    let pty_snapshot = daemon_pty(state).and_then(|runtime| runtime.snapshot());
    match &pty_snapshot {
        Ok(items) => checks.push(DoctorCheck {
            name: "pty-runtime".into(),
            status: "pass".into(),
            message: format!("the PTY runtime returned {} sessions", items.len()),
        }),
        Err(error) => checks.push(DoctorCheck {
            name: "pty-runtime".into(),
            status: if terminal_required { "fail" } else { "warn" }.into(),
            message: error.to_string(),
        }),
    }
    let isolation = st_runtime::isolation_mode();
    checks.push(DoctorCheck {
        name: "process-isolation".into(),
        status: if isolation == st_runtime::Isolation::DegradedDetached {
            "warn"
        } else {
            "pass"
        }
        .into(),
        message: match isolation {
            st_runtime::Isolation::Scope => "Linux tasks use transient systemd user scopes".into(),
            st_runtime::Isolation::Detached => {
                "tasks use detached process groups on this platform".into()
            }
            st_runtime::Isolation::DegradedDetached => {
                "systemd user scopes are unavailable; a daemon restart can stop tasks".into()
            }
        },
    });
    let mut owners = BTreeMap::<String, Vec<String>>::new();
    for subject in &desired {
        if let Some(member) = &subject.member {
            owners
                .entry(member.runtime_id.clone())
                .or_default()
                .push(subject.subject.clone());
        }
    }
    let duplicates = owners
        .into_iter()
        .filter(|(_, subjects)| subjects.len() > 1)
        .map(|(runtime, subjects)| format!("{runtime}: {}", subjects.join(", ")))
        .collect::<Vec<_>>();
    let mut ownership_problems = duplicates
        .iter()
        .map(|item| format!("duplicate runtime owners: {item}"))
        .collect::<Vec<_>>();
    let local_live_ptys = pty_snapshot
        .as_ref()
        .map(|items| {
            items
                .iter()
                .filter(|item| item.status == "running")
                .map(|item| item.name.clone())
                .collect::<std::collections::BTreeSet<_>>()
        })
        .unwrap_or_default();
    let exec_runtime =
        st_runtime::ExecRuntime::new(state.state_dir.join("exec"), state.state_dir.join("logs"));
    for subject in &desired {
        let Some(member) = subject
            .member
            .as_ref()
            .filter(|member| member.host != state.node)
        else {
            continue;
        };
        let local_live = if member.terminal {
            local_live_ptys.contains(&member.runtime_id)
        } else {
            matches!(
                exec_runtime.observe(&member.runtime_id),
                Ok(Some(st_runtime::ExecObservation::Running(_)))
            )
        };
        if local_live {
            ownership_problems.push(format!("local runtime {} for {} is placed on host/{} instead of host/{}; restore the same state directory's stable node identity before moving or restarting the seat", member.runtime_id, subject.subject, member.host, state.node));
        }
    }
    checks.push(DoctorCheck {
        name: "runtime-ownership".into(),
        status: if ownership_problems.is_empty() {
            "pass"
        } else {
            "fail"
        }
        .into(),
        message: if ownership_problems.is_empty() {
            "each desired member has a unique runtime ID and live local runtimes match their placement".into()
        } else {
            ownership_problems.join("; ")
        },
    });
    checks.push(claude_hooks_check(state, &desired)?);
    checks.push(claude_usage_check(state, &desired)?);
    checks.push(graph_references_check(
        &state
            .store
            .unresolved_graph_references()
            .map_err(ApiError::bad)?,
    ));
    let terminal_owned = state
        .store
        .terminal_owned_runtime_subjects()
        .map_err(ApiError::internal)?;
    let mut desired_runtime_ids = desired
        .iter()
        .filter(|subject| !terminal_owned.contains(&subject.subject))
        .filter(|subject| subject.member.as_ref().is_some_and(|member| member.host == state.node))
        .filter_map(|subject| {
            subject
                .member
                .as_ref()
                .map(|member| member.runtime_id.clone())
        })
        .collect::<std::collections::BTreeSet<_>>();
    for subject in &desired {
        if terminal_owned.contains(&subject.subject)
            || !subject.member.as_ref().is_some_and(|member| member.host == state.node) {
            continue;
        }
        if let Some(runtime_id) = state
            .store
            .latest_actual_value(&subject.subject)
            .map_err(ApiError::internal)?
            .as_ref()
            .map(|actual| actual.get("fields").unwrap_or(actual))
            .and_then(|fields| fields.get("runtime_id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            desired_runtime_ids.insert(runtime_id);
        }
    }
    let mut unowned = pty_snapshot
        .as_ref()
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    item.status == "running" && !desired_runtime_ids.contains(&item.name)
                })
                .map(|item| format!("PTY {}", item.name))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let exec_directory = state.state_dir.join("exec");
    let exec_runtime =
        st_runtime::ExecRuntime::new(exec_directory.clone(), state.state_dir.join("logs"));
    if let Ok(entries) = fs::read_dir(&exec_directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(runtime_id) = name.strip_suffix(".json") else {
                continue;
            };
            if desired_runtime_ids.contains(runtime_id) {
                continue;
            }
            match exec_runtime.observe(runtime_id) {
                Ok(Some(st_runtime::ExecObservation::Running(_))) => {
                    unowned.push(format!("exec {runtime_id}"));
                }
                Ok(Some(st_runtime::ExecObservation::Indeterminate(reason))) => {
                    unowned.push(format!("exec {runtime_id} ({reason})"));
                }
                Ok(Some(st_runtime::ExecObservation::Exited(_)) | None) => {}
                Err(error) => unowned.push(format!("exec {runtime_id} ({error})")),
            }
        }
    }
    checks.push(DoctorCheck {
        name: "runtime-drift".into(),
        status: if unowned.is_empty() { "pass" } else { "warn" }.into(),
        message: if unowned.is_empty() {
            "the runtime has no unowned live sessions or indeterminate records".into()
        } else {
            format!("unowned runtime state: {}", unowned.join(", "))
        },
    });
    let mut driver_gaps = Vec::new();
    for desired_subject in desired.iter().filter(|desired| {
        !terminal_owned.contains(&desired.subject)
            && desired
                .member
                .as_ref()
                .and_then(|member| member.driver.as_ref())
                .is_some()
    }) {
        let selected = state
            .store
            .status(Some(&desired_subject.subject))
            .map_err(ApiError::internal)?
            .subjects
            .into_iter()
            .next();
        let Some(subject) = selected else {
            continue;
        };
        if !subject
            .harness
            .as_ref()
            .is_some_and(crate::model::CurrentHarnessView::is_ready)
            || subject.gap.is_some()
        {
            driver_gaps.push(format!(
                "{}: {}",
                subject.subject,
                subject
                    .harness
                    .as_ref()
                    .and_then(|harness| harness.reason.as_deref())
                    .or(subject.gap.as_deref())
                    .unwrap_or("the current harness incarnation is not ready")
            ));
        }
    }
    checks.push(DoctorCheck {
        name: "driver-readiness".into(),
        status: if driver_gaps.is_empty() {
            "pass"
        } else {
            "warn"
        }
        .into(),
        message: if driver_gaps.is_empty() {
            "all desired native drivers have a ready current harness incarnation".into()
        } else {
            driver_gaps.join("; ")
        },
    });
    match state.store.replication_status_sealed(
        state.fleet_id.is_some(),
        state.fleet_id.as_deref(),
        &replication_peer_names(state),
    ) {
        Ok(replication) if !replication.configured => checks.push(DoctorCheck {
            name: "replication".into(),
            status: "pass".into(),
            message: if state.state_dir.join("left-fleet.json").is_file() {
                "this node is intentionally local-only after leaving its fleet".into()
            } else {
                "no fleet is configured; use st fleet create or st fleet join to connect this node"
                    .into()
            },
        }),
        Ok(replication) => {
            let unavailable = replication
                .peers
                .iter()
                .filter(|peer| {
                    !matches!(
                        peer.status.as_str(),
                        "up" | "last-seen" | "unknown" | "refused"
                    )
                })
                .map(|peer| format!("{}={}", peer.peer, peer.status))
                .collect::<Vec<_>>();
            // A member that listens is meant to answer, so its absence warns. A dial-out
            // member, or a config peer outside membership, can be away for hours, as a sleeping
            // laptop is; its absence is reported without a warning.
            let now = client_now_ms();
            let listening = state
                .store
                .fleet_view_sealed()
                .map(|view| {
                    view.members
                        .into_iter()
                        .filter(|member| member.state == "current" && member.mode == "listening")
                        .map(|member| member.name)
                        .collect::<BTreeSet<_>>()
                })
                .unwrap_or_default();
            let (absent, away): (Vec<_>, Vec<_>) = replication
                .peers
                .iter()
                .filter(|peer| peer.status == "last-seen")
                .partition(|peer| listening.contains(&peer.peer));
            let describe = |peers: Vec<&crate::model::ReplicationPeerStatus>| {
                peers
                    .into_iter()
                    .map(|peer| {
                        format!(
                            "{} {}{}{}",
                            peer.peer,
                            peer.last_success_at_unix_ms
                                .map(|at| format!(
                                    "has not exchanged for {}",
                                    elapsed_words(now.saturating_sub(at))
                                ))
                                .unwrap_or_else(|| "has never exchanged with this node".into()),
                            peer.sync
                                .as_ref()
                                .map(|sync| sync.local_only_envelopes
                                    + sync.added_since_measured_envelopes)
                                .filter(|unsent| *unsent != 0)
                                .map(|unsent| format!(
                                    "; this node has not sent it {unsent} envelopes"
                                ))
                                .unwrap_or_default(),
                            peer.last_error
                                .as_deref()
                                .map(|error| format!(" (last attempt: {error})"))
                                .unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let absent = describe(absent);
            let away = describe(away);
            let unresolved = replication.invalid_records;
            let diverged = replication
                .peers
                .iter()
                .filter(|peer| peer.sync.as_ref().is_some_and(|sync| sync.diverged))
                .map(|peer| peer.peer.as_str())
                .collect::<Vec<_>>();
            let differing_tables = replication
                .peers
                .iter()
                .flat_map(|peer| {
                    peer.differing_tables
                        .iter()
                        .map(|table| format!("{}/{table}", peer.peer))
                })
                .collect::<Vec<_>>();
            if !differing_tables.is_empty() {
                checks.push(DoctorCheck {
                    name: "shared-projections".into(),
                    status: "fail".into(),
                    message: format!(
                        "shared tables differ at equal inventory: {}",
                        differing_tables.join(", ")
                    ),
                });
            }
            let first_sync_failed = replication
                .first_sync
                .as_ref()
                .filter(|first| first.state == "failed");
            // A verified first sync is history. Whether this node is caught up now takes an
            // exchange since this daemon started, and one that found nothing left to fetch.
            let catching_up = replication
                .peers
                .iter()
                .filter_map(|peer| {
                    let sync = peer.sync.as_ref().filter(|sync| sync.catching_up)?;
                    Some(format!(
                        "{} has {} envelopes this node lacks",
                        peer.peer, sync.peer_only_envelopes
                    ))
                })
                .collect::<Vec<_>>();
            // A peer whose grants refuse this node never exchanges with it directly.
            let unmeasured = replication.timings.exchanges == 0
                && replication
                    .peers
                    .iter()
                    .any(|peer| peer.status != "refused");
            // A removed node is never told by status alone: its peers merely stop answering.
            let removed = crate::fleet::file::RemovalNotice::load(&state.state_dir);
            let status = if removed.is_some()
                || replication.unhealthy_projections != 0
                || !diverged.is_empty()
                || first_sync_failed.is_some()
            {
                "fail"
            } else if !unavailable.is_empty()
                || !absent.is_empty()
                || unresolved != 0
                || !catching_up.is_empty()
                || unmeasured
            {
                "warn"
            } else {
                "pass"
            };
            checks.push(DoctorCheck {
                name: "replication".into(),
                status: status.into(),
                message: format!(
                    "{}{}{}{}{}{} envelopes; {} unresolved records; {} claims waiting for a newer build; {} unhealthy projections{}; peers {}",
                    removed
                        .map(|notice| format!(
                            "{}; ",
                            notice.describe(replication.fleet_id.as_deref())
                        ))
                        .unwrap_or_default(),
                    if !catching_up.is_empty() {
                        format!("catching up: {}; ", catching_up.join(", "))
                    } else if unmeasured {
                        "no exchange with a peer since this daemon started, so whether this \
                         node is caught up is not known yet; "
                            .to_owned()
                    } else {
                        String::new()
                    },
                    absent
                        .iter()
                        .chain(&away)
                        .map(|peer| format!("{peer}; "))
                        .collect::<String>(),
                    first_sync_failed
                        .map(|first| format!(
                            "the first sync with {} failed on an earlier build: {}; it ends at \
                             the next exchange where both hold the same claims; ",
                            first.peer.as_deref().unwrap_or("a peer"),
                            first.message.as_deref().unwrap_or("no reason recorded")
                        ))
                        .unwrap_or_default(),
                    if diverged.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "graph diverged from {}: the same envelopes project a different \
                             graph, which exchanges cannot fix (st replication status); ",
                            diverged.join(", ")
                        )
                    },
                    replication.received_envelopes,
                    unresolved,
                    replication.waiting_claims,
                    replication.unhealthy_projections,
                    replication
                        .unhealthy
                        .first()
                        .map(|projection| format!(
                            " ({}: {})",
                            projection.aggregate,
                            projection
                                .error_message
                                .as_deref()
                                .unwrap_or("no reason recorded")
                        ))
                        .unwrap_or_default(),
                    replication
                        .peers
                        .iter()
                        .map(|peer| {
                            match &peer.refusal_reason {
                                Some(reason) => format!("{}: {reason}", peer.peer),
                                None => format!("{}={}", peer.peer, peer.status),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        Err(error) => checks.push(DoctorCheck {
            name: "replication".into(),
            status: "fail".into(),
            message: error.to_string(),
        }),
    }
    checks.push(first_readiness_scheduler_check(&state.store, &state.node));
    match state.store.idempotency_conflicts(5) {
        Ok((0, _)) => checks.push(DoctorCheck {
            name: "idempotency-keys".into(),
            status: "pass".into(),
            message: "no idempotency key was used for two different requests".into(),
        }),
        Ok((count, conflicts)) => checks.push(DoctorCheck {
            name: "idempotency-keys".into(),
            status: "warn".into(),
            message: format!(
                "{count} idempotency {} used for different requests on different members, as \
                 members apart during a partition can: each such claim stands, and a retry with \
                 the key is refused as idempotency-conflict. {}",
                if count == 1 { "key was" } else { "keys were" },
                conflicts
                    .iter()
                    .map(|claims| claims
                        .iter()
                        .map(|(subject, writer)| format!("{subject} by {writer}"))
                        .collect::<Vec<_>>()
                        .join(" and "))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }),
        Err(error) => checks.push(DoctorCheck {
            name: "idempotency-keys".into(),
            status: "fail".into(),
            message: error.to_string(),
        }),
    }
    let (recording, message) = crate::recorder::health(&state.state_dir);
    checks.push(DoctorCheck {
        name: "command-recorder".into(),
        status: if recording { "pass" } else { "warn" }.into(),
        message,
    });
    match (
        client_agent_resources(
            &state.store,
            false,
            "",
            state.store.index().map_err(ApiError::internal)?,
        ),
        state.store.operational_messages(None, false),
    ) {
        (Ok(agents), Ok(messages)) => {
            let current_recipients = agents
                .iter()
                .filter_map(|agent| agent["id"].as_str())
                .collect::<BTreeSet<_>>();
            let mut blocked = agents
                .iter()
                .filter(|agent| {
                    agent.pointer("/delivery/state").and_then(Value::as_str) == Some("stale")
                })
                .map(|agent| {
                    format!(
                        "{}: {}",
                        agent["id"].as_str().unwrap_or("agent"),
                        agent
                            .pointer("/delivery/reason")
                            .and_then(Value::as_str)
                            .unwrap_or("delivery is stale")
                    )
                })
                .collect::<Vec<_>>();
            let (pending, accepted) = unread_current_seat_counts(
                &state.store,
                &current_recipients,
                &messages,
                client_now_ms(),
            )
            .map_err(ApiError::internal)?;
            if pending > 0 {
                blocked.push(format!("{pending} current-seat messages older than 10s lack a recipient graph read receipt ({accepted} accepted native handoffs); this does not prove that a legacy channel failed to consume them. Inspect `st conversations status MESSAGE`"));
            }
            checks.push(DoctorCheck {
                name: "message-delivery".into(),
                status: if blocked.is_empty() { "pass" } else { "warn" }.into(),
                message: if blocked.is_empty() {
                    "no stalled local delivery paths or overdue agent messages were observed".into()
                } else {
                    blocked.join("; ")
                },
            });
        }
        (Err(error), _) | (_, Err(error)) => checks.push(DoctorCheck {
            name: "message-delivery".into(),
            status: "warn".into(),
            message: error.to_string(),
        }),
    }
    match mail_backlog::report(&state.store, client_now_ms()) {
        Ok(backlog) => checks.push(DoctorCheck {
            name: "mail-backlog".into(),
            status: if backlog.count == 0 { "pass" } else { "warn" }.into(),
            message: format!("{} unread messages older than 1h; close them with `{}`", backlog.count, backlog.cleanup_command),
        }),
        Err(error) => checks.push(DoctorCheck { name: "mail-backlog".into(), status: "warn".into(), message: error.to_string() }),
    }
    checks.push(publication_evidence_check(&state.store));
    checks.push(held_mail_check(&state.store, client_now_ms()));
    match delivery_probes::check(
        &state.store,
        client_now_ms(),
        &replication_peer_names(state),
    ) {
        Ok(Some(check)) => checks.push(check),
        Ok(None) => {}
        Err(error) => checks.push(DoctorCheck {
            name: "delivery-probes".into(),
            status: "warn".into(),
            message: error.to_string(),
        }),
    }
    // Once a node pins a fleet anchor, membership decides admission. Report what waits for a
    // signature, what is fenced, and what was admitted before this node knew better.
    match state.store.fleet_anchor() {
        Ok(None) => {}
        Ok(Some(_)) => match (
            state
                .store
                .replication_status_sealed(true, state.fleet_id.as_deref(), &[]),
            state.store.fleet_admission_residue(),
        ) {
            (Ok(holds), Ok(residue)) => {
                let mut notes = vec![format!(
                    "{} envelopes wait for their writer's signature; {} are fenced",
                    holds.unsigned_envelopes, holds.fenced_envelopes
                )];
                notes.extend(residue.iter().map(|item| {
                    format!(
                        "{} envelopes from {} were {}",
                        item.envelopes,
                        item.writer,
                        item.reason.replace('-', " ")
                    )
                }));
                checks.push(DoctorCheck {
                    name: "fleet-admission".into(),
                    status: if residue.is_empty() && holds.unsigned_envelopes == 0 {
                        "pass"
                    } else {
                        "warn"
                    }
                    .into(),
                    message: notes.join("; "),
                });
            }
            (Err(error), _) | (_, Err(error)) => checks.push(DoctorCheck {
                name: "fleet-admission".into(),
                status: "fail".into(),
                message: error.to_string(),
            }),
        },
        Err(error) => checks.push(DoctorCheck {
            name: "fleet-admission".into(),
            status: "fail".into(),
            message: error.to_string(),
        }),
    }
    let attention_now = client_now_ms();
    let attention = doctor_attention_items(&state.store, attention_now).map_err(ApiError::internal)?;
    checks.push(
        mission_eligibility_doctor::check(&state.store, &attention, attention_now)
            .map_err(ApiError::internal)?,
    );
    checks.push(stale_attention_check(&attention, attention_now));
    let report_status = if checks.iter().any(|check| check.status == "fail") {
        "fail"
    } else if checks.iter().any(|check| check.status == "warn") {
        "warn"
    } else {
        "pass"
    };
    Ok(Json(DoctorReport {
        machine_version: Some(st_drivers::version::machine_version()),
        status: report_status.into(),
        checks,
        performance: crate::performance::snapshot(),
    }))
}

fn default_true() -> bool {
    true
}

/// Count completed summary store calls, including errors, without charging nested CPU.
/// These wall times overlap the export route. JSON serialization, worker decode and CPU
/// are outside this measurement; do not add these durations to the enclosing route.
fn record_replication_summary<T>(authority_guard: bool, work: impl FnOnce() -> T) -> T {
    let label = if authority_guard {
        "replication/authority-guard"
    } else {
        "replication/summary"
    };
    let started = Instant::now();
    let result = work();
    crate::performance::record_request(label, None, started.elapsed());
    result
}

#[cfg(test)]
#[path = "api/replication_guard_tests.rs"]
mod replication_guard_tests;

// Admission is already committed here. A contended projection is pending work, not a
// failed peer delivery; the contention generation schedules the daemon's bounded retry.
fn admitted_projection_result(result: anyhow::Result<Option<bool>>) -> Result<Option<bool>, St3Error> {
    match result {
        Ok(projected) => Ok(projected),
        Err(error) if error.downcast_ref::<St3Error>().is_some_and(|error| error.is_sqlite_contention()) => Ok(None),
        Err(error) => Err(St3Error::new("internal", error.to_string())),
    }
}

/// A span such as `45s`, `12m` or `3h` for a doctor message.
fn elapsed_words(ms: u128) -> String {
    let seconds = ms / 1_000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

fn replication_receive_has_new_data(received: usize) -> bool {
    received != 0
}

/// Pure local memory update. Older daemons answer 404 rather than interpreting worker
/// progress as a failed exchange during a worker/daemon rolling restart.
async fn replication_worker_status(
    State(state): State<AppState>,
    Json(request): Json<smallclaims::replication::ReplicationWorkerStatusRequest>,
) -> Json<Value> {
    state.store.record_replication_worker(&request.peer, request.worker);
    Json(json!({ "recorded": true, "changed": false }))
}

pub use crate::model::FleetEndpointsRequest;

/// The peers a node reports on: its config peers and the current listening members it dials.
/// A dial-out member is never dialed, and an ended name is history, so neither is reported.
fn replication_peer_names(state: &AppState) -> Vec<String> {
    let view = state.store.fleet_view_sealed().unwrap_or_default();
    let mut names = state
        .configured_peers
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    names.extend(
        view.members
            .iter()
            .filter(|member| member.state == "current")
            .map(|member| member.name.clone()),
    );
    names.retain(|name| {
        let current = view.current(name);
        let ended = view.members.iter().any(|member| member.name == *name) && current.is_empty();
        *name != state.node && !ended && !view.legacy_removed.contains(name)
    });
    names.into_iter().collect()
}

#[derive(Deserialize, Serialize)]
pub struct FleetInviteRequest {
    #[serde(default)]
    pub name: Option<String>,
    pub expires_seconds: u64,
    #[serde(default)]
    pub via: Option<String>,
    #[serde(default)]
    pub migrate: bool,
    pub person: String,
}

#[derive(Deserialize, Serialize)]
pub struct FleetInviteCreated {
    pub invite: String,
    pub code: String,
    pub expires_at_unix_ms: u64,
}

#[derive(Deserialize, Serialize)]
pub struct FleetInviteRevokeRequest {
    pub invite: String,
    pub reason: String,
    pub person: String,
}

#[derive(Deserialize, Serialize)]
pub struct FleetStatus {
    pub node: String,
    pub fleet_id: Option<String>,
    pub member_key: Option<String>,
    /// Configured local transports, including when no endpoint is currently up.
    #[serde(default)]
    pub transports: Vec<String>,
    #[serde(default)]
    pub fabric_protocol: Option<String>,
    pub view: crate::fleet::FleetView,
    pub peers: Vec<crate::model::ReplicationPeerStatus>,
    pub invites: Vec<crate::store::FleetInviteView>,
    /// A member refused this node as removed or left, so it no longer syncs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed: Option<crate::fleet::file::RemovalNotice>,
}

fn concrete_person(person: &str) -> Result<(), ApiError> {
    if person.starts_with("person/") && person.matches('/').count() == 1 && person.len() > 7 {
        Ok(())
    } else {
        Err(ApiError::bad(St3Error::new(
            "person-required",
            "fleet operations need a concrete person/NAME",
        )))
    }
}

#[derive(Deserialize, Serialize)]
pub struct FleetRemoveRequest {
    pub name: String,
    pub reason: String,
    pub person: String,
}

#[derive(Deserialize, Serialize)]
pub struct FleetPersonRequest {
    pub person: String,
}

/// While this node leaves its fleet, it refuses every new write except the leave itself and
/// replication traffic, so the leave stays its writer's last batch.
async fn refuse_while_leaving(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let mutating =
        request.method() != axum::http::Method::GET && request.method() != axum::http::Method::HEAD;
    // Replication keeps running so the drain can finish; it writes no local claims while
    // leaving, because transport observations stop. Everything else that writes waits,
    // including invite redemption and endpoint announcements.
    let allowed = path.starts_with("/v1/internal/fleet/leave/")
        || matches!(
            path,
            "/v1/health"
                | "/v1/internal/replication/export"
                | "/v1/internal/replication/receive"
                | "/v1/internal/replication/peer-failure"
                | "/v1/internal/replication/worker-status"
                | "/v1/internal/replication/checkpoint"
                | "/v1/internal/replication-wake"
        );
    if mutating && !allowed && state.store.fleet_leaving().unwrap_or(false) {
        // Read the body before refusing: an answer sent while the client is still writing
        // closes the connection under it, and the client then reports a broken pipe instead
        // of this refusal.
        let _ = axum::body::to_bytes(request.into_body(), 1 << 20).await;
        return ApiError::bad(St3Error::new(
            "fleet-leaving",
            "this node is leaving its fleet and accepts no new writes; `st fleet leave --cancel` stops the leave",
        ))
        .into_response();
    }
    next.run(request).await
}

fn launch_candidate_mission<'a>(
    intent: &'a crate::model::NormalizedIntent,
    expected: &str,
) -> Result<&'a crate::model::MissionSpec, St3Error> {
    let Some(mission) = intent.missions.get(expected) else {
        return Err(St3Error::new(
            "wrong-launch-mission",
            format!(
                "a candidate must contain ready mission `{expected}` and no immediate desired state"
            ),
        ));
    };
    let published_closure = crate::mission::mission_closure_ids(mission);
    let published = intent.missions.keys().cloned().collect::<BTreeSet<_>>();
    if !intent.subjects.is_empty() || published != published_closure {
        return Err(St3Error::new(
            "wrong-launch-mission",
            format!(
                "a candidate must contain ready mission `{expected}`, its implicit mission graphs, and no immediate desired state"
            ),
        ));
    }
    Ok(mission)
}

fn valid_launch_decision_response(
    decision_type: &LaunchDecisionType,
    options: &[LaunchDecisionOption],
    response: &LaunchDecisionResponse,
) -> bool {
    let option_ids = options
        .iter()
        .map(|option| option.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    match (decision_type, response) {
        (LaunchDecisionType::Boolean, LaunchDecisionResponse::Boolean(_)) => true,
        (LaunchDecisionType::SingleChoice, LaunchDecisionResponse::SingleChoice(value)) => {
            option_ids.contains(value.as_str())
        }
        (LaunchDecisionType::MultipleChoice, LaunchDecisionResponse::MultipleChoice(values)) => {
            !values.is_empty()
                && values
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == values.len()
                && values
                    .iter()
                    .all(|value| option_ids.contains(value.as_str()))
        }
        (LaunchDecisionType::Rank, LaunchDecisionResponse::Rank(values)) => {
            values.len() == options.len()
                && values
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == values.len()
                && values
                    .iter()
                    .all(|value| option_ids.contains(value.as_str()))
        }
        _ => false,
    }
}

fn mission_source(
    state: &AppState,
    kdl: &str,
    at_index: Option<u64>,
) -> Result<(crate::model::NormalizedIntent, MissionResponse), ApiError> {
    let initial = parse_intent(kdl, &state.node).map_err(ApiError::bad)?;
    let bindings = state
        .store
        .document_bindings_at(&initial.document_refs, at_index)
        .map_err(ApiError::internal)?;
    let resolved_kdl = resolve_document_references(kdl, &bindings).map_err(ApiError::bad)?;
    let intent = parse_intent(&resolved_kdl, &state.node).map_err(ApiError::bad)?;
    let response = state
        .store
        .mission_at(
            &intent,
            crate::model::IntentInput {
                kdl: resolved_kdl,
                source_name: Some("planning candidate".into()),
            },
            at_index,
        )
        .map_err(ApiError::bad)?;
    Ok((intent, response))
}

fn send_planning_message(
    state: &AppState,
    key: &str,
    from: &str,
    to: &str,
    content: &str,
    title: &str,
) -> Result<(), ApiError> {
    let subject = format!(
        "message/{}",
        &hex::encode(Sha256::digest(key.as_bytes()))[..16]
    );
    state
        .store
        .append_claim(&ClaimInput {
            subject,
            kind: "message.sent".into(),
            actor: Some(normalize_message_party(from)),
            fields: BTreeMap::from([
                ("from".into(), Value::String(normalize_message_party(from))),
                ("to".into(), Value::String(normalize_message_party(to))),
                ("content".into(), Value::String(content.into())),
                ("status".into(), Value::String("sent".into())),
                ("title".into(), Value::String(title.into())),
                ("in_reply_to".into(), Value::Null),
                (
                    "tags".into(),
                    Value::Array(vec![Value::String("launch".into())]),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key.into()),
        })
        .map_err(ApiError::bad)?;
    Ok(())
}

/// What a publication route refuses besides its own checks: each reference in the publication
/// that does not resolve, and each render a member it declares on this host would refuse.
#[derive(Default)]
struct PublicationRefusals {
    references: Vec<String>,
    renders: Vec<String>,
}

impl PublicationRefusals {
    /// The error that refuses the publication, if anything does.
    fn error(self) -> Option<St3Error> {
        let (code, refusals) = if !self.references.is_empty() {
            ("unresolved-reference", self.references)
        } else if !self.renders.is_empty() {
            ("render-refused", self.renders)
        } else {
            return None;
        };
        Some(St3Error::new(code, refusals.join("; ")).with_detail("refusals", json!(refusals)))
    }

    /// Lists the refusals as the preview's blockers, where each replaces a warning of the same
    /// text.
    fn block(self, response: &mut MissionResponse) {
        let refusals = self
            .references
            .into_iter()
            .chain(self.renders)
            .collect::<Vec<_>>();
        response
            .warnings
            .retain(|warning| !refusals.contains(warning));
        response.blockers.extend(refusals);
        response.blockers.sort();
        response.blockers.dedup();
    }
}

async fn publication_refusals(
    state: &AppState,
    intent: &crate::model::NormalizedIntent,
) -> Result<PublicationRefusals, ApiError> {
    let store = state.store.clone();
    let node = state.node.clone();
    let intent = intent.clone();
    blocking_external(move || {
        let references = store.unresolved_references(&intent)?;
        let publication = intent.subjects.values().collect::<Vec<_>>();
        // Render reads the workspace and asks git about tracked files, so it runs only for a
        // publication that declares a member.
        let renders = if publication.iter().any(|subject| subject.member.is_some()) {
            let current = store
                .desired_subjects()
                .map_err(|error| St3Error::new("internal", error.to_string()))?;
            crate::render::publication_refusals(&store, &publication, &current, &node)
        } else {
            Vec::new()
        };
        Ok(PublicationRefusals {
            references,
            renders,
        })
    })
    .await?.map_err(ApiError::bad)
}


#[derive(Deserialize)]
pub(crate) struct AgentSuspensionRequest {
    #[serde(default)]
    pub host: Option<String>,
    pub subject: String,
    pub actor: String,
    pub idempotency_key: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// The seat a suspend or resume names: a declared service seat with no conflicting declaration.
fn suspension_target(
    state: &AppState,
    subject: &str,
) -> Result<
    (
        crate::model::SubjectStatus,
        crate::model::MemberSpec,
        String,
    ),
    ApiError,
> {
    state
        .store
        .owned_member_guard(subject)
        .map_err(ApiError::bad)?;
    if state
        .store
        .rollout(subject)
        .map_err(ApiError::bad)?
        .is_some_and(|o| o.holds_seat())
    {
        return Err(ApiError::bad(St3Error::new(
            "rollout-in-progress",
            "the seat has a rollout; inspect it or retry with st agents rollout",
        )));
    }
    let status = state
        .store
        .status(Some(subject))
        .map_err(ApiError::internal)?;
    let current = status
        .subjects
        .into_iter()
        .find(|item| item.subject == subject)
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-agent",
                format!("no seat `{subject}`"),
            ))
        })?;
    if !current.conflicts.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "suspend-conflict",
            "resolve the seat's conflicting declarations first",
        )));
    }
    let mut member = state
        .store
        .desired_subject_with_writer(subject)
        .map_err(ApiError::internal)?
        .map(|(desired, _)| desired)
        .filter(|desired| desired.kind == "agent")
        .and_then(|desired| desired.member)
        .filter(|member| member.lifecycle == crate::model::MemberLifecycle::Service)
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "suspend-not-declared",
                "only a declared, started seat can be suspended or resumed",
            ))
        })?;
    if let Some((_, incumbent)) =
        crate::rollout::pending_manual_launch(&state.store, subject).map_err(ApiError::internal)?
    {
        member = incumbent;
    }
    let token = current.desired_token.clone().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "suspend-not-declared",
            "the seat has no selected declaration",
        ))
    })?;
    Ok((current, member, token))
}

fn agent_subject(subject: String) -> String {
    if subject.starts_with("agent/") {
        subject
    } else {
        format!("agent/{subject}")
    }
}

/// The fence a client-v0 action carries: the seat's running incarnation, when it names one, and
/// its selected declaration.
pub(crate) struct SuspensionFence {
    pub incarnation: Option<String>,
    pub desired: String,
}

fn check_suspension_fence(
    fence: Option<&SuspensionFence>,
    incarnation: Option<&str>,
    token: &str,
) -> Result<(), ApiError> {
    let Some(fence) = fence else {
        return Ok(());
    };
    if fence.desired != token
        || fence
            .incarnation
            .as_deref()
            .is_some_and(|expected| Some(expected) != incarnation)
    {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "stale-fence".into(),
            message: "the seat changed since this action was prepared".into(),
            details: Box::default(),
        });
    }
    Ok(())
}

pub(crate) fn request_suspend(
    state: &AppState,
    request: AgentSuspensionRequest,
    fence: Option<SuspensionFence>,
) -> Result<ClaimRecord, ApiError> {
    let actor = person_or_agent_actor(&request.actor, "invalid-suspend-actor")?;
    let subject = agent_subject(request.subject);
    let key = format!("agent-suspend:{subject}:{}", request.idempotency_key);
    if let Some(prior) = state
        .store
        .operation_claim(&key)
        .map_err(ApiError::internal)?
    {
        return Ok(prior);
    }
    if crate::rollout::status(&state.store, &subject)
        .map_err(ApiError::internal)?
        .is_some_and(|s| s["mode"] == "manual" && s["phase"] == "pending")
    {
        return Err(ApiError::bad(St3Error::new(
            "rollout-in-progress",
            "the seat has a rollout; inspect it or retry with st agents rollout",
        )));
    }
    let (current, member, token) = suspension_target(state, &subject)?;
    if let Some(suspension) =
        crate::suspension::current(&state.store, &subject).map_err(ApiError::internal)?
        && suspension.holds_seat()
    {
        return Err(ApiError::bad(
            St3Error::new(
                "already-suspended",
                format!("the seat is already {}", suspension.phase),
            )
            .with_detail("phase", suspension.phase),
        ));
    }
    let actual = current
        .actual
        .as_ref()
        .map(|actual| actual.get("fields").unwrap_or(actual));
    let incarnation = actual
        .filter(|fields| fields.get("status").and_then(Value::as_str) == Some("running"))
        .and_then(|fields| fields.get("incarnation_id"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "suspend-not-running",
                "only a running seat can be suspended",
            ))
        })?;
    check_suspension_fence(fence.as_ref(), Some(incarnation), &token)?;
    if let Some(selector) = crate::native_resume::authored_session_selection(&member) {
        return Err(ApiError::bad(
            St3Error::new(
                "suspend-authored-session-selection",
                format!("the seat declares its own session selection ({selector}); native resume would refuse it, so suspend leaves the seat running"),
            )
            .with_detail("selector", selector),
        ));
    }
    let blocking = crate::suspension::blockers(&state.store, &subject, incarnation)
        .map_err(ApiError::internal)?;
    if !blocking.is_empty() {
        return Err(ApiError::bad(
            St3Error::new(
                "suspend-blocked",
                format!("the seat is not quiet: {}", blocking.join(", ")),
            )
            .with_detail("blocking", blocking),
        ));
    }
    let mut fields = BTreeMap::from([
        ("action".into(), Value::String("suspend".into())),
        (
            "runtime_id".into(),
            Value::String(member.runtime_id.clone()),
        ),
        ("incarnation_id".into(), Value::String(incarnation.into())),
    ]);
    if let Some(reason) = request.reason.filter(|reason| !reason.trim().is_empty()) {
        fields.insert("reason".into(), Value::String(reason));
    }
    let claim = state
        .store
        .append_claim(&ClaimInput {
            subject,
            kind: "runtime.action.requested".into(),
            actor: Some(actor),
            fields,
            evidence: vec![token],
            expected_subject: None,
            idempotency_key: Some(key),
        })
        .map_err(ApiError::bad)?;
    signal_changed(state);
    Ok(claim)
}

pub(crate) fn request_resume(
    state: &AppState,
    request: AgentSuspensionRequest,
    fence: Option<SuspensionFence>,
) -> Result<ClaimRecord, ApiError> {
    let actor = person_or_agent_actor(&request.actor, "invalid-resume-actor")?;
    let subject = agent_subject(request.subject);
    let key = format!("agent-resume:{subject}:{}", request.idempotency_key);
    if let Some(prior) = state
        .store
        .operation_claim(&key)
        .map_err(ApiError::internal)?
    {
        return Ok(prior);
    }
    let (_, member, token) = suspension_target(state, &subject)?;
    check_suspension_fence(fence.as_ref(), None, &token)?;
    let suspension = crate::suspension::current(&state.store, &subject)
        .map_err(ApiError::internal)?
        .filter(|suspension| suspension.phase == "suspended")
        .ok_or_else(|| {
            ApiError::bad(St3Error::new("not-suspended", "the seat is not suspended"))
        })?;
    let suspend = suspension.suspend_operation_id.clone().ok_or_else(|| {
        ApiError::internal(anyhow::anyhow!("a suspension without its suspend request"))
    })?;
    let host = request.host.unwrap_or_else(|| member.host.clone());
    if host.is_empty() || host.contains('/') {
        return Err(ApiError::bad(St3Error::new(
            "invalid-host",
            "resume needs a host name",
        )));
    }
    let launch_token = if let Some((incumbent, old)) =
        crate::rollout::pending_manual_launch(&state.store, &subject).map_err(ApiError::internal)?
    {
        if host != old.host {
            return Err(ApiError::bad(St3Error::new(
                "rollout-in-progress",
                "resume on the current host before applying the pending manual rollout",
            )));
        }
        incumbent
    } else {
        token.clone()
    };
    let mut fields = BTreeMap::from([
        ("action".into(), Value::String("resume".into())),
        (
            "runtime_id".into(),
            Value::String(member.runtime_id.clone()),
        ),
    ]);
    if host != member.host {
        let membership = state.store.fleet_membership().map_err(ApiError::internal)?;
        if host != state.node
            && !state.configured_peers.contains(&host)
            && !matches!(
                membership.state(&host),
                crate::fleet::MemberState::Current(_)
            )
        {
            return Err(ApiError::bad(St3Error::new(
                "unknown-host",
                "the resume target is not a current fleet member",
            )));
        }
        if !matches!(suspension.harness.as_deref(), Some("pi" | "omp")) {
            return Err(ApiError::bad(St3Error::new(
                "snapshot-harness-unsupported",
                "cross-host resume currently supports pi and omp portable transcripts",
            )));
        }
        fields.insert("host".into(), Value::String(host));
        fields.insert("source_host".into(), Value::String(member.host.clone()));
    }
    let claim = state
        .store
        .append_claim(&ClaimInput {
            subject,
            kind: "runtime.action.requested".into(),
            actor: Some(actor),
            fields,
            evidence: vec![launch_token, suspend],
            expected_subject: None,
            idempotency_key: Some(key),
        })
        .map_err(ApiError::bad)?;
    signal_changed(state);
    Ok(claim)
}

async fn apply(
    State(state): State<AppState>,
    Json(request): Json<ApplyRequest>,
) -> Result<Json<ApplyResponse>, ApiError> {
    apply_with_authority(state, request, None).await
}

async fn apply_with_authority(
    state: AppState,
    request: ApplyRequest,
    bound: Option<&str>,
) -> Result<Json<ApplyResponse>, ApiError> {
    let actor = request.actor.as_deref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "missing-publication-actor",
            "publication needs an explicit actor",
        ))
    })?;
    let intent = parse_intent(&request.intent.kdl, &state.node).map_err(ApiError::bad)?;
    if !intent.direct_message_registrations.is_empty()
        && bound.is_some_and(|bound| normalized_agent_actor(actor).as_deref() != Some(bound))
    {
        return Err(ApiError::bad(St3Error::new("foreign-agent-actor",
            "a bound harness registers a message subscription only as itself")));
    }
    for declaration in intent.mission_runs.values() {
        if let Some(creation) = &declaration.creation {
            if creation.requester == "person/requester" {
                return Err(ApiError::bad(St3Error::new(
                    "placeholder-run-requester",
                    "a mission run needs a concrete requester, not `person/requester`",
                )));
            }
            if let Some(agent) = normalized_agent_actor(actor)
                && creation.requester != agent
            {
                return Err(ApiError::bad(St3Error::new(
                    "run-requester-actor-mismatch",
                    format!(
                        "`{actor}` cannot create a run for requester `{}`",
                        creation.requester
                    ),
                )));
            }
        }
    }
    if let Some(error) = publication_refusals(&state, &intent).await?.error() {
        return Err(ApiError::bad(error));
    }
    let mut response = state
        .store
        .apply_as(
            &intent,
            &request.expected_subjects,
            &request.idempotency_key,
            Some(actor),
        )
        .map_err(ApiError::bad)?;
    response.resolved_kdl = request.intent.kdl;
    signal_changed(&state);
    Ok(Json(response))
}

/// One write a rule in audit mode would have refused.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuleAudit {
    pub rule: String,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub at_unix_ms: u128,
    pub claim: String,
}

/// Set one rule as a person.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuleSetRequest {
    pub actor: String,
    pub name: String,
    pub rule: smallclaims::rules::Rule,
}

#[derive(Deserialize)]
struct DeliveryHoldQuery {
    subject: String,
}

async fn get_delivery_hold(
    State(state): State<AppState>,
    Query(query): Query<DeliveryHoldQuery>,
) -> Result<Json<crate::delivery_hold::HoldView>, ApiError> {
    st3_schema::registry()
        .validate_subject(&query.subject)
        .map_err(|error| ApiError::bad(St3Error::new(error.code, error.message)))?;
    let store = state.store.clone();
    blocking_store(move || {
        crate::delivery_hold::view(&store, &query.subject, client_now_ms() as u64)
    })
    .await
    .map(Json)
}

async fn post_delivery_hold(
    State(state): State<AppState>,
    Json(request): Json<crate::delivery_hold::HoldRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let input =
        crate::delivery_hold::input(request, client_now_ms() as u64).map_err(ApiError::bad)?;
    let store = state.store.clone();
    let (claim, appended) = blocking_action(move || {
        if input.fields.get("held") == Some(&Value::Bool(true))
            && input.fields.get("legacy_adoption") != Some(&Value::Bool(true))
        {
            let subjects = store
                .desired_subjects_named(std::slice::from_ref(&input.subject))
                .map_err(|error| St3Error::new("internal", error.to_string()))?;
            let driver = subjects
                .first()
                .and_then(|subject| subject.member.as_ref())
                .and_then(|member| member.driver.as_deref());
            if !matches!(driver, Some("codex" | "opencode")) {
                return Err(St3Error::new(
                    "unsupported-delivery-hold",
                    "delivery holds currently require a declared Codex or OpenCode seat",
                ));
            }
        }
        store.append_claim_outcome(&input)
    })
    .await?;
    if appended {
        signal_visible_change(&state);
    }
    Ok(Json(claim))
}

// Both claim transports must publish response-usage rollups, even on replay after the original
// observation committed but a later step failed. Keep visibility wakes identical as well.
async fn finish_claim_publication(
    state: &AppState,
    kind: &str,
    response: ClaimRecord,
    appended: bool,
    harness_transition: Option<bool>,
) -> Result<Json<ClaimRecord>, ApiError> {
    // Publish only the cumulative buckets. The response detail and turn ID remain local.
    if response.kind == "harness.timeline" {
        let store = state.store.clone();
        let rollup_response = response.clone();
        if let Some(rollup) =
            blocking_store(move || store.usage_rollup_for_timeline(&rollup_response)).await?
        {
            let store = state.store.clone();
            let (_, updated) =
                blocking_action(move || store.append_client_claim_outcome(&rollup)).await?;
            if updated {
                signal_visible_change(state);
            }
        }
    }
    if appended {
        if crate::store::local_observation_position(&response).is_some() {
            // Native event admission echoes its local record, including when it published a
            // ready/idle transition. Work wakes read that observation, not just durable claims.
            if kind == "harness.observed"
                && harness_transition.unwrap_or_else(|| {
                    response.body["fields"]["status_transition"] != false
                })
            {
                crate::performance::record_wake("api", Some(kind));
                state.notify.notify_one();
            }
            signal_local_change(state);
        } else if matches!(kind, "harness.usage" | "harness.limits" | "subagent.renewed") {
            // The reconciler reads none of these. The limits policy runs on its own two-minute
            // timer, so a limits claim woke a full reconcile pass for nothing, every few seconds.
            signal_visible_change(state);
        } else if kind.starts_with("message.") {
            let store = state.store.clone();
            let subject = response.subject.clone();
            // A message this store cannot read is treated as a work wake.
            let work_wake = blocking_store(move || store.message(&subject))
                .await
                .ok()
                .flatten()
                .is_none_or(|message| is_work_wake(&message.tags));
            signal_message_changed(state, kind, work_wake);
        } else {
            signal_claim_changed(state, kind);
        }
    }
    Ok(Json(response))
}

/// Every objective and budget as one list: see `objectives`.
pub fn slo_rows(store: &crate::store::Store) -> anyhow::Result<Vec<Value>> {
    let limits = store.account_limits()?;
    let fresh_ms = crate::config::reload_daemon_limits()
        .and_then(|reloaded| reloaded.ok())
        .and_then(|config| crate::store::LimitsPolicy::from_config(&config))
        .map_or(crate::store::ACCOUNT_READING_WINDOW_MS, |policy| policy.fresh_ms);
    let mut rows = crate::objectives::rows(
        &request_latency_target_windows(),
        &limits,
        client_now_ms() as u64,
        fresh_ms,
    );
    if let Some(open) = store.open_divergences_at_last_check() {
        // Objectives come before budgets.
        let budgets = rows.iter().position(|row| row["kind"] == "budget").unwrap_or(rows.len());
        rows.insert(budgets, crate::objectives::divergence(open));
    }
    Ok(rows)
}

#[derive(Deserialize)]
struct SloHistoryQuery {
    id: String,
    since_ms: Option<u64>,
    until_ms: Option<u64>,
    /// How many equal steps to answer in; each holds its lowest reading.
    steps: Option<usize>,
}

fn default_claim_limit() -> usize {
    100
}

async fn set_delegation_policy(
    State(state): State<AppState>,
    Json(request): Json<crate::model::DelegationPolicyRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let result = state
        .store
        .set_delegation_policy(&request)
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(result))
}

/// The fields a device signs on a message it sends, in the `fields-v1` format.
pub const SIGNED_MESSAGE_FIELDS: &[&str] = &[
    "content",
    "from",
    "in_reply_to",
    "session_id",
    "tags",
    "title",
    "to",
];

/// The fields a device signs on a message past 8 KiB. The claim holds a preview, so it signs
/// that: `content` is the preview, cut as [`message_body::preview`] cuts it, and `body_sha256`
/// and `body_bytes` are the hash and length of the whole text, which every member reads from the
/// claim's `text/plain` `attachments` entry. Sorted, as the check compares them.
pub const SIGNED_LONG_MESSAGE_FIELDS: &[&str] = &[
    "body_bytes",
    "body_sha256",
    "content",
    "from",
    "in_reply_to",
    "session_id",
    "tags",
    "title",
    "to",
];

/// How far a device's signing time may be from this daemon's clock when it first accepts the
/// message. Members that receive it later check the signature, never the time.
pub const DEVICE_SIGNATURE_WINDOW_MS: u128 = 15 * 60 * 1_000;

fn device_signature_error(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::bad(St3Error::new(code, message.into()))
}

/// Check a device's signature on the message this daemon is about to write, before writing it.
fn check_device_signature(
    state: &AppState,
    signature: &smallclaims::principal::ClaimSignature,
    request: &MessageSendRequest,
    subject: &str,
    from: &str,
    fields: &BTreeMap<String, Value>,
    expected: &[&str],
) -> Result<(), ApiError> {
    use smallclaims::principal::{FIELDS_FORMAT, Judged, KeyGrant};
    let mut signed = signature.signed_fields.clone();
    signed.sort();
    if signature.format.as_deref() != Some(FIELDS_FORMAT) || signed != expected {
        return Err(device_signature_error(
            "device-signature-format",
            format!(
                "a device signs a message in {FIELDS_FORMAT} over {}",
                expected.join(", ")
            ),
        ));
    }
    if signature.signer != from || signature.on_behalf.is_some() {
        return Err(device_signature_error(
            "device-signature-signer",
            format!("a device signs as the paired person, {from}"),
        ));
    }
    if normalize_message_party(&request.to) != request.to {
        return Err(device_signature_error(
            "device-signature-noncanonical",
            format!(
                "a signed message names its recipient canonically: `{}`",
                normalize_message_party(&request.to)
            ),
        ));
    }
    let now = client_now_ms();
    if u128::from(signature.signed_at_unix_ms).abs_diff(now) > DEVICE_SIGNATURE_WINDOW_MS {
        return Err(device_signature_error(
            "device-signature-stale",
            "the device signed this message more than 15 minutes from this daemon's clock; check the device's time",
        ));
    }
    // A retry of the same send gets its first answer; any other claim may not reuse the nonce.
    let repeat = state
        .store
        .latest_claim(subject, Some("message.sent"))
        .map_err(ApiError::internal)?
        .is_some();
    if !repeat
        && state
            .store
            .signature_nonce_used(&signature.key, &signature.nonce)
            .map_err(ApiError::internal)?
    {
        return Err(device_signature_error(
            "device-signature-replayed",
            "another claim already carries this signature's nonce",
        ));
    }
    let enrolled = signature
        .chain
        .first()
        .and_then(|grant| state.store.claim_by_id(grant).ok().flatten())
        .and_then(|grant| {
            let fields = grant.body.get("fields")?;
            (grant.subject == from).then(|| KeyGrant::from_fields(fields))?
        })
        .is_some_and(|grant| grant.key == signature.key);
    if !enrolled {
        return Err(device_signature_error(
            "device-key-not-enrolled",
            "the signing key is not enrolled for this person; pair the device again",
        ));
    }
    let fields = Value::Object(fields.clone().into_iter().collect());
    let judged = Judged {
        id: "",
        subject,
        kind: "message.sent",
        actor: Some(from),
        content: String::new(),
        fields: &fields,
    };
    if !signature.verifies(&judged) {
        return Err(device_signature_error(
            "device-signature-invalid",
            "the signature does not match this message",
        ));
    }
    Ok(())
}

#[cfg(test)]
fn accept_message(
    state: &AppState,
    request: MessageSendRequest,
    session_id: Option<String>,
    device_signature: Option<smallclaims::principal::ClaimSignature>,
) -> Result<Json<MessageView>, ApiError> {
    accept_message_receipt(state, request, session_id, device_signature)
        .map(|receipt| Json(receipt.message))
}

/// Accept one message, or find the one its idempotency key already sent. The receipt says which,
/// so a client that repeats an unanswered send can say that nothing new was sent.
fn accept_message_receipt(
    state: &AppState,
    request: MessageSendRequest,
    session_id: Option<String>,
    device_signature: Option<smallclaims::principal::ClaimSignature>,
) -> Result<MessageSendReceipt, ApiError> {
    accept_message_receipt_with_upload_owner(state, request, session_id, device_signature, None, None)
}

fn accept_message_receipt_with_upload_owner(
    state: &AppState,
    request: MessageSendRequest,
    session_id: Option<String>,
    device_signature: Option<smallclaims::principal::ClaimSignature>,
    upload_owner: Option<&str>,
    assistant_authorization: Option<&crate::store::assistant_write::AssistantWriteAuthorization>,
) -> Result<MessageSendReceipt, ApiError> {
    if request.from.starts_with("external/") && upload_owner.is_none() {
        return Err(ApiError::bad(St3Error::new(
            "adapter-route-refused",
            "external sender imports require the enrolled adapter endpoint",
        )));
    }
    crate::model::refuse_person_recipient(&normalize_message_party(&request.to))
        .map_err(ApiError::bad)?;
    if request.content.trim().is_empty() && request.attachments.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "empty-message",
            "a message needs nonempty content",
        )));
    }
    let long_body = request.content.len() > message_body::INLINE_MAX_BYTES
        && !request.content.starts_with("doc/");
    if long_body && request.content.len() > message_body::MAX_BYTES {
        return Err(ApiError::bad(St3Error::new(
            "message-too-large",
            format!(
                "a message cannot be longer than {} KiB; this one is {} KiB. Shorten it or send it as several messages.",
                message_body::MAX_BYTES / 1024,
                request.content.len().div_ceil(1024)
            ),
        )));
    }
    if request.content.starts_with("doc/") {
        let (name, hash) = request.content.rsplit_once('@').ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "unpinned-document-reference",
                "a message document reference needs `doc/NAME@HASH`",
            ))
        })?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ApiError::bad(St3Error::new(
                "invalid-document-reference",
                "a message document reference has an invalid hash",
            )));
        }
        if state
            .store
            .get_document(name, hash)
            .map_err(ApiError::internal)?
            .is_none()
        {
            return Err(ApiError::bad(St3Error::new(
                "missing-document",
                format!("message document `{}` is not stored", request.content),
            )));
        }
    }
    let from = normalize_message_party(&request.from);
    let to = normalize_message_party(&request.to);
    if !from.starts_with("daemon/")
        && request
            .tags
            .iter()
            .any(|tag| crate::silent::reserved_event_tag(tag))
    {
        return Err(ApiError::bad(St3Error::new(
            "reserved-message-tag",
            "daemon event tags cannot be asserted by conversation senders",
        )));
    }
    if device_signature.is_some()
        && request
            .tags
            .iter()
            .any(|tag| crate::silent::is_remaining_tag(tag))
    {
        return Err(ApiError::bad(St3Error::new(
            "reserved-message-tag",
            "remaining-count tags are delivery metadata",
        )));
    }
    let attachments = client_blobs::resolve_attachments(
        state,
        upload_owner.unwrap_or(&from),
        &request.attachments,
    )?;
    let id = hex::encode(Sha256::digest(request.idempotency_key.as_bytes()))[..16].to_owned();
    let subject = format!("message/{id}");
    let previous = state
        .store
        .operation_claim(&request.idempotency_key)
        .map_err(ApiError::internal)?;
    let parent = request
        .in_reply_to
        .as_deref()
        .map(|parent| state.store.message(&message_subject(parent)))
        .transpose()
        .map_err(ApiError::internal)?
        .flatten();
    // A signed message stores exactly the tags its device signed; only a person signs, and a
    // person's message always wakes.
    let tags = if let Some(previous) = previous
        .as_ref()
        .filter(|claim| claim.kind == "message.sent" && claim.subject == subject)
    {
        let stored: Vec<String> = serde_json::from_value(
            previous.body["fields"]
                .get("tags")
                .cloned()
                .unwrap_or_else(|| json!([])),
        )
        .map_err(ApiError::internal)?;
        // Reuse acceptance metadata while still checking the sender's tag input.
        let same_tags = if device_signature.is_some() {
            request.tags == stored
        } else {
            crate::silent::stored_tags(&from, &request.tags)
                == crate::silent::stored_tags(&from, &stored)
        };
        if !same_tags {
            return Err(ApiError::bad(St3Error::new(
                "idempotency-mismatch",
                "the idempotency key already identifies different message tags",
            )));
        }
        stored
    } else if device_signature.is_some() {
        request.tags.clone()
    } else {
        crate::silent::stored_tags(&from, &request.tags)
    };
    let mut fields = BTreeMap::from([
        ("from".into(), Value::String(from.clone())),
        ("to".into(), Value::String(to.clone())),
        (
            "content".into(),
            Value::String(if long_body {
                message_body::preview(&request.content)
            } else {
                request.content.clone()
            }),
        ),
        ("status".into(), Value::String("sent".into())),
        (
            "title".into(),
            request
                .title
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        ),
        (
            "in_reply_to".into(),
            request
                .in_reply_to
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        ),
        (
            "tags".into(),
            Value::Array(tags.iter().cloned().map(Value::String).collect()),
        ),
    ]);
    if let Some(session_id) = session_id {
        fields.insert("session_id".into(), Value::String(session_id));
    }
    // A long body is a file on this member, named in the claim like an image: nothing of it but
    // the preview and this reference enters the database or replication. The file is staged
    // under a temporary name before the claim, so a failed write never leaves a claim without
    // its text, and renamed into place once the claim has landed, so a refusal or a failed
    // append leaves no stray file. A repeat of the same send finishes a rename that did not
    // happen, which heals a crash between the two.
    let mut body = None;
    let mut claim_attachments = attachments.clone();
    if long_body {
        message_body::bodies(&state.state_dir)
            .ensure_room(&from, &subject, request.content.len())
            .map_err(ApiError::bad)?;
        let entry = crate::model::MessageAttachment {
            sha256: hex::encode(Sha256::digest(request.content.as_bytes())),
            media_type: message_body::MEDIA_TYPE.into(),
            name: None,
            size: request.content.len() as u64,
            origin: client_host_id(&state.node),
        };
        claim_attachments.push(entry.clone());
        body = Some(entry);
    }
    if !claim_attachments.is_empty() {
        fields.insert(
            "attachments".into(),
            serde_json::to_value(&claim_attachments).map_err(ApiError::internal)?,
        );
    }
    if let Some(signature) = &device_signature {
        let mut signed = fields.clone();
        let mut expected = SIGNED_MESSAGE_FIELDS;
        if let Some(body) = &body {
            // The claim holds a preview, so the signature must cover the preview, the hash and
            // the length, which every member can read from the claim. A device that signs the
            // whole text, as it does under 8 KiB, signs something no other member can check.
            let mut names = signature.signed_fields.clone();
            names.sort();
            if names == SIGNED_MESSAGE_FIELDS {
                return Err(ApiError::bad(St3Error::new(
                    "long-message-signature-unsupported",
                    format!(
                        "this device signs messages in a form that cannot cover one over {} KiB; update it, or send a shorter message",
                        message_body::INLINE_MAX_BYTES / 1024
                    ),
                )));
            }
            signed.insert("body_sha256".into(), Value::String(body.sha256.clone()));
            signed.insert("body_bytes".into(), json!(body.size));
            expected = SIGNED_LONG_MESSAGE_FIELDS;
        }
        check_device_signature(state, signature, &request, &subject, &from, &signed, expected)?;
    }
    let staged = if body.is_some() {
        message_body::bodies(&state.state_dir)
            .stage(&from, &subject, request.content.as_bytes())
            .map_err(ApiError::internal)?
    } else {
        None
    };
    let input = ClaimInput {
        subject: subject.clone(),
        kind: "message.sent".into(),
        actor: Some(from.clone()),
        fields,
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(request.idempotency_key.clone()),
    };
    // A repeated key returns the first claim and says it appended nothing.
    let (record, appended) = match &device_signature {
        Some(signature) => state.store.append_signed_message(&input, signature),
        None => match assistant_authorization {
            Some(authorization) => state.store.append_assistant_claim_outcome(&input, authorization),
            None => state.store.append_claim_outcome(&input),
        },
    }
    .map_err(|error| {
        if let Some(staged) = staged.as_ref() {
            staged.discard();
        }
        ApiError::bad(error)
    })?;
    if let Some(staged) = staged {
        // The claim is in. If the text cannot be put in place, the message exists with its
        // preview: say so as an unconfirmed send, which a repeat of the same send finishes.
        staged.commit().map_err(|error| {
            ApiError::internal(anyhow::anyhow!(
                "the message was recorded but its full text could not be put in place on this machine ({error}); send it again to finish"
            ))
        })?;
    }
    // A concurrent sender may have accepted this key first with the same canonical input.
    // Return its accepted classification, rather than this attempt's speculative tags.
    let tags = record.body["fields"]["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|tag| !crate::silent::is_remaining_tag(tag))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut work_wake = is_work_wake(&tags);
    if let Some(reference) = request.in_reply_to.as_deref() {
        // Settling the parent writes its lifecycle claims too.
        work_wake |= parent
            .as_ref()
            .is_none_or(|message| is_work_wake(&message.tags));
        settle_answered_message(&state.store, reference, &from, &to, &subject, &record.id)?;
    }
    signal_message_changed(state, "message.sent", work_wake);
    let kind = if tags.iter().any(|tag| crate::silent::is_silent_tag(tag))
        && !crate::silent::always_wakes(&from, &tags)
    {
        "silent"
    } else {
        "wake"
    };
    Ok(MessageSendReceipt {
        kind: Some(kind.into()),
        message: MessageView {
            subject,
            from,
            to,
            content: if long_body {
                message_body::preview(&request.content)
            } else {
                request.content
            },
            status: "sent".into(),
            title: request.title,
            in_reply_to: request.in_reply_to,
            tags,
            created_index: record.store_index,
            attachments,
            body_ref: body.as_ref().map(|body| message_body::reference(&body.sha256)),
            body_bytes: body.as_ref().map(|body| body.size),
            body_origin: body.map(|body| body.origin),
            // Use the accepted claim's transaction-established provenance on replay too.
            sender_class: record.body["fields"]["sender_class"].as_str().map(str::to_owned),
            assistant_grant_id: record.body["fields"]["assistant_grant_id"].as_str().map(str::to_owned),
        },
        idempotency_key: request.idempotency_key,
        already_sent: !appended,
        sent_at: Some(client_timestamp(record.accepted_at_unix_ms)),
    })
}

/// A recipient's successful reply is durable evidence that the parent was consumed.
/// Settle it here so a restarted native seat cannot receive the same request again merely
/// because it answered before running a separate `conversations archive` command.
fn settle_answered_message(
    store: &Store,
    parent: &str,
    from: &str,
    to: &str,
    reply: &str,
    reply_claim: &str,
) -> Result<(), ApiError> {
    let parent = message_subject(parent);
    let Some(message) = store.message(&parent).map_err(ApiError::internal)? else {
        return Ok(());
    };
    // A thread participant can send a follow-up without consuming the other party's inbox.
    if message.to != from || message.from != to {
        return Ok(());
    }
    for lifecycle in ["delivered", "read", "closed"] {
        let current = store
            .message(&parent)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("message `{parent}` does not exist")))?;
        let needed = match lifecycle {
            "delivered" => matches!(current.status.as_str(), "sent" | "staged"),
            "read" => current.status == "delivered",
            "closed" => current.status == "read",
            _ => unreachable!(),
        };
        if !needed {
            continue;
        }
        store
            .append_claim(&ClaimInput {
                subject: parent.clone(),
                kind: format!("message.{lifecycle}"),
                actor: Some(from.into()),
                fields: BTreeMap::from([("status".into(), Value::String(lifecycle.into()))]),
                evidence: vec![reply_claim.into()],
                expected_subject: None,
                idempotency_key: Some(format!("reply-settled:{reply}:{lifecycle}")),
            })
            .map_err(ApiError::bad)?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct MessagesQuery {
    to: Option<String>,
    #[serde(default)]
    include_closed: bool,
}

/// One message as it is read, with who signed it when a person wrote it (see
/// `client_v0::message_provenance`).
#[derive(Serialize)]
struct MessageRead {
    #[serde(flatten)]
    message: MessageView,
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance: Option<Value>,
}

fn message_subject(value: &str) -> String {
    if value.starts_with("message/") {
        value.to_owned()
    } else {
        format!("message/{value}")
    }
}

#[derive(Deserialize)]
struct EventQuery {
    #[serde(default, alias = "after_index")]
    after: u64,
    subject: Option<String>,
    owner_run: Option<String>,
    wait: Option<bool>,
    timeout_ms: Option<u64>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct EventPage {
    items: Vec<EventRecord>,
    next_after: Option<u64>,
    has_more: bool,
    frontier: u64,
}

async fn read_event_page(state: &AppState, query: &EventQuery) -> Result<EventPage, ApiError> {
    let limit = query.limit.unwrap_or(200);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::bad(St3Error::new(
            "invalid-event-limit",
            "event page limit must be 1 through 200",
        )));
    }
    let store = state.store.clone();
    let after = query.after;
    let subject = query.subject.clone();
    let owner = query.owner_run.clone();
    blocking_store(move || {
        store.read_snapshot(|frontier| {
            let (oldest, newest) = store.event_bounds()?;
            let floor = oldest.saturating_sub(1);
            if after < floor || after > newest {
                return Ok(Err(ApiError {
                    status: StatusCode::GONE,
                    code: "cursor-gap".into(),
                    message: "the event cursor is outside the retained event range".into(),
                    details: Box::new(serde_json::Map::from_iter([
                        ("full_resync".into(), json!(true)),
                        ("resume_floor".into(), json!(floor)),
                        ("frontier".into(), json!(newest)),
                    ])),
                }));
            }
            let (items, next_after, has_more) =
                store.events_page(after, subject.as_deref(), owner.as_deref(), limit)?;
            Ok(Ok(EventPage {
                items,
                next_after,
                has_more,
                frontier,
            }))
        })
    })
    .await?
}

async fn events_page(
    State(state): State<AppState>,
    Query(query): Query<EventQuery>,
) -> Result<Json<EventPage>, ApiError> {
    // Capture registration before the frontier/empty-page read, including a migration wake.
    let mut changed = state.event_notify.subscribe();
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(query.timeout_ms.unwrap_or(30_000).clamp(10, 30_000));
    loop {
        let page = read_event_page(&state, &query).await?;
        if page.next_after.is_some() || query.wait != Some(true) {
            if query.wait == Some(true) && page.items.is_empty() {
                // A filtered page can advance through nonmatching rows during migration.
                // Keep its exact continuation, but pace wait-mode clients instead of inviting
                // an immediate empty-page repoll. The caller's timeout still bounds the wait.
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                tokio::time::sleep(remaining.min(Duration::from_millis(250))).await;
            }
            return Ok(Json(page));
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero()
            || !matches!(
                tokio::time::timeout(remaining, changed.changed()).await,
                Ok(Ok(()))
            )
        {
            return read_event_page(&state, &query).await.map(Json);
        }
    }
}

async fn quick_agent(
    state: &AppState,
    request: QuickAgentRequest,
    driver: &str,
) -> Result<QuickAgentResponse, ApiError> {
    let response_key = format!("quick-agent-response:{}", request.idempotency_key);
    if let Some(response) = state
        .store
        .cached_idempotency_response(&response_key)
        .map_err(ApiError::internal)?
    {
        return Ok(response);
    }
    let bus_id = request
        .subject
        .strip_prefix("agent/")
        .unwrap_or(&request.subject);
    let agent_subject = if bus_id.contains('.') || bus_id.contains('/') {
        format!("agent/{bus_id}")
    } else {
        format!("agent/{}.{bus_id}", state.node)
    };
    let selected_token = state
        .store
        .selected_desired_token(&agent_subject)
        .map_err(ApiError::internal)?
        .into_iter()
        .collect::<Vec<_>>();
    let historical_apply = state
        .store
        .cached_idempotency_response::<crate::model::ApplyResponse>(&request.idempotency_key)
        .map_err(ApiError::internal)?;
    if selected_token != request.expected_subject && historical_apply.is_none() {
        return Err(ApiError::bad(St3Error::new(
            "stale-subject",
            format!("the desired state for `{agent_subject}` changed"),
        )));
    }
    let mut driver_body = String::new();
    if let Some(model) = &request.model {
        driver_body.push_str(&format!("model {model:?}\n"));
    }
    if let Some(effort) = &request.effort {
        driver_body.push_str(&format!("effort {effort:?}\n"));
    }
    if !request.arguments.is_empty() {
        driver_body.push_str("args");
        for argument in &request.arguments {
            driver_body.push_str(&format!(" {argument:?}"));
        }
        driver_body.push('\n');
    }
    let kdl = format!(
        "version 2\nagent {bus_id:?} {{\n  identity {bus_id:?}\n  workspace {:?}\n  restart \"always\"\n  harness {driver:?} {{\n{driver_body}  }}\n}}\n",
        request.worktree
    );
    let intent = parse_intent(&kdl, &state.node).map_err(ApiError::bad)?;
    let planned = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: kdl.clone(),
                source_name: Some(format!("quick {driver}")),
            },
        )
        .map_err(ApiError::bad)?;
    let runtime_id = intent
        .subjects
        .get(&agent_subject)
        .and_then(|subject| subject.member.as_ref())
        .map(|member| member.runtime_id.clone())
        .ok_or_else(|| ApiError::internal("quick agent normalization lost its runtime"))?;
    let harness = state
        .store
        .current_harness(&agent_subject)
        .map_err(ApiError::internal)?;
    let ready = harness
        .as_ref()
        .is_some_and(crate::model::CurrentHarnessView::is_ready);
    let response = QuickAgentResponse {
        subject: agent_subject,
        mission: None,
        mission_run: None,
        generation: None,
        runtime_id,
        event_cursor: 0, // The transaction supplies the declaration's exact committed frontier.
        incarnation_id: harness.map(|harness| harness.incarnation_id),
        ready,
        response_reconstructed: historical_apply.is_some(),
    };
    state
        .store
        .apply_with_local_receipt(
            &intent,
            &planned.subject_tokens,
            &request.idempotency_key,
            &response_key,
            &response,
        )
        .map_err(ApiError::bad)?;
    signal_changed(state);
    state
        .store
        .cached_idempotency_response(&response_key)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal("the committed quick-agent response is missing"))
}

fn scope_eval_desired_subjects(
    intent: &mut crate::model::NormalizedIntent,
    current: &[crate::model::DesiredSubject],
    owner_run: &str,
) -> Result<(), St3Error> {
    let selected = current
        .iter()
        .map(|subject| (subject.subject.as_str(), subject))
        .collect::<BTreeMap<_, _>>();
    let mut shared = Vec::new();
    for (subject, desired) in &mut intent.subjects {
        let Some(current) = selected.get(subject.as_str()) else {
            desired.owner_run = Some(owner_run.to_owned());
            continue;
        };
        if *current == desired {
            shared.push(subject.clone());
            continue;
        }
        return Err(St3Error::new(
            "eval-desired-conflict",
            format!(
                "eval declaration `{subject}` conflicts with the selected desired state; use a run-owned declaration or an isolated daemon"
            ),
        ));
    }
    for subject in shared {
        intent.subjects.remove(&subject);
    }
    Ok(())
}

async fn start_mission_run_action(
    State(state): State<AppState>,
    Json(request): Json<MissionRunRequest>,
) -> Result<Json<MissionRunView>, ApiError> {
    let requester = request.requester.as_deref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "missing-run-requester",
            "a mission run needs an explicit requester",
        ))
    })?;
    if requester == "person/requester" {
        return Err(ApiError::bad(St3Error::new(
            "placeholder-run-requester",
            "a mission run needs a concrete requester, not `person/requester`",
        )));
    }
    let response = state
        .store
        .create_mission_run(&request)
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(response))
}



fn cached_revision_submission(
    state: &AppState,
    direct_key: &str,
    proposal_key: &str,
) -> Result<Option<RevisionSubmissionView>, ApiError> {
    if let Some(mission_run) = state
        .store
        .cached_idempotency_response::<MissionRunView>(direct_key)
        .map_err(ApiError::internal)?
    {
        return Ok(Some(RevisionSubmissionView {
            provenance: state
                .store
                .mission_provenance(&mission_run.mission, &mission_run.revision)
                .map_err(ApiError::internal)?,
            status: "applied".into(),
            mission_run,
            proposal: None,
        }));
    }
    let Some(cached) = state
        .store
        .cached_idempotency_response::<RevisionProposalView>(proposal_key)
        .map_err(ApiError::internal)?
    else {
        return Ok(None);
    };
    let proposal = state
        .store
        .revision_proposal(&cached.id)
        .map_err(ApiError::internal)?
        .unwrap_or(cached);
    let mission_run = state
        .store
        .mission_run(&proposal.run)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal("the proposal mission run is unavailable"))?;
    Ok(Some(RevisionSubmissionView {
        provenance: state
            .store
            .mission_provenance(&mission_run.mission, &proposal.candidate_revision)
            .map_err(ApiError::internal)?,
        status: proposal.status.clone(),
        mission_run,
        proposal: Some(proposal),
    }))
}

async fn get_revision_proposal(
    State(state): State<AppState>,
    AxumPath(proposal): AxumPath<String>,
) -> Result<Json<RevisionProposalView>, ApiError> {
    let store = state.store.clone();
    let proposal_for_read = proposal.clone();
    blocking_store(move || store.revision_proposal(&proposal_for_read))
        .await?
        .map(Json)
        .ok_or_else(|| {
            ApiError::not_found(format!("revision proposal `{proposal}` does not exist"))
        })
}

async fn approve_revision_proposal(
    State(state): State<AppState>,
    AxumPath(proposal): AxumPath<String>,
    Json(request): Json<RevisionApprovalRequest>,
) -> Result<Json<RevisionSubmissionView>, ApiError> {
    let result = state
        .store
        .approve_revision_proposal(
            &proposal,
            &request.actor,
            &request.preview_hash,
            &request.idempotency_key,
        )
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(result))
}

async fn cancel_revision_proposal(
    State(state): State<AppState>,
    AxumPath(proposal): AxumPath<String>,
    Json(request): Json<RevisionCancelRequest>,
) -> Result<Json<RevisionProposalView>, ApiError> {
    let result = state
        .store
        .cancel_revision_proposal(
            &proposal,
            &request.actor,
            request.reason.as_deref(),
            &request.idempotency_key,
        )
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(result))
}

#[derive(Deserialize)]
struct WorkQuery {
    actor: Option<String>,
    #[serde(default)]
    include_terminal: bool,
    #[serde(default)]
    view: WorkView,
    #[serde(default)]
    caller: WorkCaller,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum WorkView {
    #[default]
    Full,
    Renewal,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum WorkCaller {
    #[default]
    List,
    CapacityDiagnostics,
    TimelineBinding,
    WaitInterruption,
    SubagentAttribution,
}

impl WorkCaller {
    fn label(&self) -> &'static str {
        match self {
            Self::List => "work/list",
            Self::CapacityDiagnostics => "work/capacity-diagnostics",
            Self::TimelineBinding => "work/timeline-binding",
            Self::WaitInterruption => "work/wait-interruption",
            Self::SubagentAttribution => "work/subagent-attribution",
        }
    }
}

fn person_or_agent_actor(actor: &str, code: &'static str) -> Result<String, ApiError> {
    if actor.starts_with("person/") {
        return Ok(actor.to_owned());
    }
    normalized_agent_actor(actor).ok_or_else(|| {
        ApiError::bad(St3Error::new(
            code,
            format!("`{actor}` is neither a person nor an agent"),
        ))
    })
}

/// Every open lane, every declared lane with `?all=true`, or the lanes of one `?run=`.
/// The process at the other end of a local socket connection, as the kernel reports it, and
/// the agent its ancestry names.
#[derive(Clone, Debug)]
struct LocalPeer {
    pid: u32,
    ancestor: Option<String>,
}

fn normalized_agent_actor(actor: &str) -> Option<String> {
    if actor.starts_with("person/") || actor.starts_with("daemon/") || actor.starts_with("system/")
    {
        None
    } else if actor.starts_with("agent/") {
        Some(actor.to_owned())
    } else {
        Some(format!("agent/{actor}"))
    }
}

async fn work_action_response(
    state: AppState,
    action: String,
    subject: String,
    request: WorkRequest,
    extend_ms: Option<u64>,
    handoff: Option<crate::model::WorkHandoffRequest>,
) -> Result<Json<StepRunView>, ApiError> {
    let actor = request
        .actor
        .as_deref()
        .and_then(normalized_agent_actor)
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-work-actor",
                "a work action needs an exact agent actor",
            ))
        })?;
    let incarnation = request.incarnation.as_deref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "missing-work-incarnation",
            "a work action must be fenced to one exact live agent incarnation",
        ))
    })?;
    // An exact retry returns the transaction's durable response even if the provider exited after
    // committing it. The store repeats this lookup under its mutation boundary; this early read
    // only prevents the live-incarnation precondition from breaking idempotent recovery.
    if let Some(mut response) = state
        .store
        .cached_idempotency_response::<StepRunView>(&request.idempotency_key)
        .map_err(ApiError::internal)?
    {
        if let Some(input) = handoff.as_ref() {
            state.store.handoff_retry(&subject, input).map_err(ApiError::bad)?;
        }
        if action == "renew" {
            // A renewal caches its view without the fields the writer no longer folds.
            let store = state.store.clone();
            response = blocking_action(move || {
                store
                    .enrich_work_response(&mut response)
                    .map_err(|error| St3Error::new("store-read-failed", format!("{error:#}")))?;
                Ok(response)
            })
            .await?;
        }
        return Ok(Json(response));
    }
    let harness = state
        .store
        .current_harness(&actor)
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "inactive-work-incarnation",
                format!("`{actor}` has no current live harness incarnation"),
            ))
        })?;
    if harness.incarnation_id != incarnation || harness.state == "ended" {
        return Err(ApiError::bad(St3Error::new(
            "inactive-work-incarnation",
            format!("`{incarnation}` is not the current live incarnation of `{actor}`"),
        )));
    }
    let quiet_renewal = action == "renew";
    let store = state.store.clone();
    let response = blocking_action(move || {
        let mut response = if let Some(input) = handoff {
            store.handoff_work(&subject, &input)?
        } else {
            store.work_action_extending(&subject, &action, &request, extend_ms)?
        };

        work_response::enrich_under(&store, &mut response)?;
        Ok(response)
    })
    .await?;
    if quiet_renewal {
        signal_visible_change(&state);
    } else {
        signal_changed(&state);
    }
    Ok(Json(response))
}

#[derive(Clone, Debug)]
struct LiveSession {
    runtime_id: String,
    incarnation_id: String,
    owner_host_id: String,
    terminal: bool,
    driver: Option<String>,
}

fn live_session(
    state: &AppState,
    subject: &str,
    expected_incarnation: Option<&str>,
) -> Result<LiveSession, ApiError> {
    let selected = state.store.runtime_authority(subject).map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("subject `{subject}` has no live session")))?;
    if !matches!(selected.reachability.as_str(), "reachable" | "local") {
        return Err(ApiError::bad(St3Error::new(
            "runtime-authority-indeterminate",
            format!("subject `{subject}` has indeterminate runtime authority"),
        )));
    }
    let origin = selected.actual_origin.as_deref().ok_or_else(|| {
        ApiError::not_found(format!(
            "subject `{subject}` has no selected runtime origin"
        ))
    })?;
    if origin != state.store.origin() {
        return Err(ApiError::bad(St3Error::new(
            "runtime-not-local",
            format!(
                "subject `{subject}` is owned by `{}` and cannot be controlled through `{}`",
                client_host_id(origin),
                client_host_id(&state.node)
            ),
        )));
    }
    let actual = selected
        .actual
        .as_ref()
        .ok_or_else(|| ApiError::not_found(format!("subject `{subject}` has no live session")))?;
    let fields = actual.get("fields").unwrap_or(actual);
    if fields.get("status").and_then(Value::as_str) != Some("running") {
        return Err(ApiError::not_found(format!(
            "subject `{subject}` has no running session"
        )));
    }
    let runtime_id = fields
        .get("runtime_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::not_found(format!("subject `{subject}` has no runtime")))?;
    let incarnation_id = fields
        .get("incarnation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::not_found(format!("subject `{subject}` has no incarnation")))?;
    if expected_incarnation.is_some_and(|expected| incarnation_id != expected) {
        return Err(ApiError::bad(St3Error::new(
            "stale-incarnation",
            format!("subject `{subject}` changed incarnation"),
        )));
    }
    let member = state
        .store
        .desired_subject_with_writer(subject)
        .map_err(ApiError::internal)?
        .and_then(|(desired, _)| desired.member);
    let terminal = member
        .as_ref()
        .map(|member| member.terminal)
        .or_else(|| fields.get("terminal").and_then(Value::as_bool))
        .ok_or_else(|| ApiError::not_found(format!("subject `{subject}` has no runtime kind")))?;
    Ok(LiveSession {
        runtime_id: runtime_id.into(),
        incarnation_id: incarnation_id.into(),
        owner_host_id: client_host_id(origin),
        terminal,
        driver: member.and_then(|member| member.driver),
    })
}

fn default_log_limit() -> usize {
    64 * 1024
}

async fn input_session_as(
    state: &AppState,
    subject: String,
    request: SessionInputRequest,
    authority_actor: &str,
) -> Result<Json<SessionControlResponse>, ApiError> {
    let result_key = format!(
        "session-control-result:input:{subject}:{}",
        request.idempotency_key
    );
    if let Some(result) = state
        .store
        .idempotent_claim(&result_key)
        .map_err(ApiError::internal)?
    {
        return Ok(Json(session_control_response(&subject, &result)));
    }
    let session = live_session(state, &subject, Some(&request.expected_incarnation))?;
    if !session.terminal {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-capability",
            "session input requires a terminal session",
        )));
    }
    let bytes = match request.mode {
        SessionInputMode::Raw => base64::engine::general_purpose::STANDARD
            .decode(&request.value)
            .map_err(|error| {
                ApiError::bad(St3Error::new(
                    "invalid-session-input",
                    format!("raw terminal input is not valid base64: {error}"),
                ))
            })?,
        SessionInputMode::Line | SessionInputMode::Key => request.value.as_bytes().to_vec(),
    };
    let mode = match request.mode {
        SessionInputMode::Line => "line",
        SessionInputMode::Raw => "raw",
        SessionInputMode::Key => "key",
    };
    let compaction_intent = matches!(request.mode, SessionInputMode::Line)
        && request.value == "/compact"
        && session.driver.as_deref() == Some("codex");
    let request_key = format!(
        "session-control-request:input:{subject}:{}",
        request.idempotency_key
    );
    if let Some(prior) = state
        .store
        .operation_claim(&request_key)
        .map_err(ApiError::internal)?
    {
        return finish_session_control(
            state,
            &subject,
            "terminal.input.result",
            &result_key,
            &prior,
            &session,
            Err(anyhow::anyhow!(
                "the input request committed before an outcome; st will not repeat it"
            )),
        );
    }
    let mut request_fields = BTreeMap::from([
        ("mode".into(), Value::String(mode.into())),
        (
            "sha256".into(),
            Value::String(hex::encode(Sha256::digest(&bytes))),
        ),
        ("byte_count".into(), Value::from(bytes.len() as u64)),
        (
            "runtime_id".into(),
            Value::String(session.runtime_id.clone()),
        ),
        (
            "incarnation_id".into(),
            Value::String(session.incarnation_id.clone()),
        ),
    ]);
    if compaction_intent {
        request_fields.insert("intent".into(), Value::String("context-compaction".into()));
    }
    let request_claim = state
        .store
        .append_claim(&ClaimInput {
            subject: subject.clone(),
            kind: "terminal.input.requested".into(),
            actor: Some(authority_actor.into()),
            fields: request_fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(request_key),
        })
        .map_err(ApiError::bad)?;
    let effect_state = state.clone();
    let effect_session = session.clone();
    let effect = blocking_external(move || daemon_pty(&effect_state).and_then(|runtime| match request.mode {
        SessionInputMode::Line => runtime.send_line_if(
            &effect_session.runtime_id,
            &request.value,
            Some(&effect_session.incarnation_id),
        ),
        SessionInputMode::Raw => {
            runtime.send_raw_if(&effect_session.runtime_id, &bytes, Some(&effect_session.incarnation_id))
        }
        SessionInputMode::Key => runtime.send_key_if(
            &effect_session.runtime_id,
            &request.value,
            Some(&effect_session.incarnation_id),
        ),
    })).await?;
    finish_session_control(
        state,
        &subject,
        "terminal.input.result",
        &result_key,
        &request_claim,
        &session,
        effect,
    )
}

async fn signal_session_as(
    state: AppState,
    subject: String,
    request: SessionSignalRequest,
    actor: &str,
) -> Result<Json<SessionControlResponse>, ApiError> {
    let signal = match request.signal.as_str() {
        "interrupt" => libc::SIGINT,
        "terminate" => libc::SIGTERM,
        "hangup" => libc::SIGHUP,
        "user-1" => libc::SIGUSR1,
        "user-2" => libc::SIGUSR2,
        other => {
            return Err(ApiError::bad(St3Error::new(
                "invalid-session-signal",
                format!("session signal `{other}` is not registered"),
            )));
        }
    };
    let result_key = format!(
        "session-control-result:signal:{subject}:{}",
        request.idempotency_key
    );
    if let Some(result) = state
        .store
        .idempotent_claim(&result_key)
        .map_err(ApiError::internal)?
    {
        return Ok(Json(session_control_response(&subject, &result)));
    }
    let session = live_session(&state, &subject, Some(&request.expected_incarnation))?;
    let request_key = format!(
        "session-control-request:signal:{subject}:{}",
        request.idempotency_key
    );
    if let Some(prior) = state
        .store
        .operation_claim(&request_key)
        .map_err(ApiError::internal)?
    {
        return finish_session_control(
            &state,
            &subject,
            "runtime.action.succeeded",
            &result_key,
            &prior,
            &session,
            Err(anyhow::anyhow!(
                "the signal request committed before an outcome; st will not repeat it"
            )),
        );
    }
    let request_claim = state
        .store
        .append_claim(&ClaimInput {
            subject: subject.clone(),
            kind: "runtime.action.requested".into(),
            actor: Some(actor.into()),
            fields: BTreeMap::from([
                ("action".into(), Value::String("signal".into())),
                ("operation".into(), Value::String(result_key.clone())),
                ("signal".into(), Value::String(request.signal)),
                (
                    "runtime_id".into(),
                    Value::String(session.runtime_id.clone()),
                ),
                (
                    "incarnation_id".into(),
                    Value::String(session.incarnation_id.clone()),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(request_key),
        })
        .map_err(ApiError::bad)?;
    let effect_state = state.clone();
    let effect_session = session.clone();
    let effect = blocking_external(move || if effect_session.terminal {
        daemon_pty(&effect_state).and_then(|runtime| {
            runtime.signal_if(&effect_session.runtime_id, Some(&effect_session.incarnation_id), signal)
        })
    } else {
        st_runtime::ExecRuntime::new(effect_state.state_dir.join("exec"), effect_state.state_dir.join("logs"))
            .signal_if(&effect_session.runtime_id, Some(&effect_session.incarnation_id), signal)
    }).await?;
    finish_session_control(
        &state,
        &subject,
        "runtime.action.succeeded",
        &result_key,
        &request_claim,
        &session,
        effect,
    )
}

fn finish_session_control(
    state: &AppState,
    subject: &str,
    kind: &str,
    result_key: &str,
    request: &ClaimRecord,
    session: &LiveSession,
    effect: anyhow::Result<()>,
) -> Result<Json<SessionControlResponse>, ApiError> {
    let succeeded = effect.is_ok();
    let mut reason = effect.as_ref().err().map(ToString::to_string);
    let mut fields = BTreeMap::from([
        (
            "runtime_id".into(),
            Value::String(session.runtime_id.clone()),
        ),
        (
            "incarnation_id".into(),
            Value::String(session.incarnation_id.clone()),
        ),
    ]);
    match kind {
        "terminal.input.result" => {
            fields.insert(
                "result".into(),
                Value::String(if succeeded { "written" } else { "failed" }.into()),
            );
        }
        "harness.context-clear.result" => {
            let result = if succeeded { "indeterminate" } else { "failed" };
            if succeeded {
                reason = Some(
                    "the clear command was sent, but no new context epoch was observed".into(),
                );
            }
            fields.insert("result".into(), Value::String(result.into()));
        }
        _ => {
            fields.insert(
                "operation_status".into(),
                Value::String(if succeeded { "succeeded" } else { "failed" }.into()),
            );
        }
    }
    fields.insert(
        "reason".into(),
        reason.clone().map(Value::String).unwrap_or(Value::Null),
    );
    let claim_kind = if kind == "runtime.action.succeeded" && !succeeded {
        "runtime.action.failed"
    } else {
        kind
    };
    let result = state
        .store
        .append_claim(&ClaimInput {
            subject: subject.into(),
            kind: claim_kind.into(),
            actor: request.actor.clone().or_else(|| Some("requester".into())),
            fields,
            evidence: vec![request.id.clone()],
            expected_subject: None,
            idempotency_key: Some(result_key.into()),
        })
        .map_err(ApiError::bad)?;
    signal_changed(state);
    if !succeeded && let Some(reason) = reason {
        let code = if reason.contains("changed incarnation") {
            "stale-incarnation"
        } else {
            "control-action-failed"
        };
        return Err(ApiError::bad(St3Error::new(code, reason)));
    }
    Ok(Json(SessionControlResponse {
        subject: subject.into(),
        request_claim_id: request.id.clone(),
        result_claim_id: result.id,
        event_cursor: result.store_index,
    }))
}

fn session_control_response(subject: &str, result: &ClaimRecord) -> SessionControlResponse {
    SessionControlResponse {
        subject: subject.into(),
        request_claim_id: result
            .body
            .pointer("/evidence/0")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        result_claim_id: result.id.clone(),
        event_cursor: result.store_index,
    }
}

#[derive(Deserialize)]
struct AgentWorkspaceQuery {
    identity: String,
}

/// The directory a host gives a new agent that names no workspace. Another host's home is only
/// known there, so this relays to the owner under the caller's person, like a client read.
async fn host_agent_workspace(
    State(state): State<AppState>,
    AxumPath(host): AxumPath<String>,
    Query(query): Query<AgentWorkspaceQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let host_id = client_host_id(host.strip_prefix("host/").unwrap_or(&host));
    if host_id == client_host_id(&state.node) {
        let workspace =
            crate::config::default_agent_workspace(&query.identity).map_err(|error| {
                ApiError::bad(St3Error::new("validation-failed", error.to_string()))
            })?;
        return Ok(Json(json!({ "host_id": host_id, "workspace": workspace })));
    }
    let person = headers
        .get("x-st3-person")
        .and_then(|value| value.to_str().ok())
        .filter(|person| person.starts_with("person/") && person.matches('/').count() == 1)
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-person",
                format!("asking {host_id} for a workspace needs a concrete person"),
            ))
        })?;
    let relay = state
        .client_relay
        .as_ref()
        .filter(|relay| relay.reaches(&host_id))
        .ok_or_else(|| remote_unavailable_for_owner(&state, &host_id))?;
    let value = relay
        .read(
            &host_id,
            &crate::peer::ClientReadRequest {
                authority_actor: person.into(),
                relay: None,
                request: crate::peer::ClientReadOperation::AgentWorkspace {
                    identity: query.identity,
                },
            },
        )
        .await
        .map_err(|error| remote_read_error(&host_id, error))?;
    let workspace = value["workspace"]
        .as_str()
        .ok_or_else(|| ApiError::internal(format!("{host_id} returned no workspace")))?;
    Ok(Json(json!({ "host_id": host_id, "workspace": workspace })))
}

fn normalize_message_party(value: &str) -> String {
    if value == "requester" {
        "person/requester".into()
    } else if value.contains('/') {
        value.into()
    } else {
        format!("agent/{value}")
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod work_incarnation_tests;
#[cfg(test)]
mod message_bodies_tests;
