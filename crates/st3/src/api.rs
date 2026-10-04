use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
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
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{Notify, watch};
use tower::ServiceExt as _;

use crate::archive::hydrate_eval;
use crate::graph::{parse_intent, resolve_document_references};
#[cfg(test)]
use crate::model::AttentionRequest;
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
    MissionRunRequest, MissionRunView, OperationalRepairApplyRequest, OperationalRepairPlan,
    OperationalRepairResult, PlannerSpec, PlanningApprovalRequest, PlanningCancelRequest,
    PlanningCandidateSubmitRequest, PlanningProposalRequest, PlanningRevisionRequest,
    PlanningSessionStartRequest, PlanningSessionView, QuickAgentRequest, QuickAgentResponse,
    ReplicaRecordView, ReplicationExportRequest, ReplicationExportResponse, ReplicationHealAnswer,
    ReplicationHealAnswerRequest, ReplicationHealNextRequest, ReplicationHealStep,
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
mod client_v0;
mod delivery_presence;
mod delivery_probes;
mod github_watch;
mod harness_events;
mod mailbox;
mod owned_sets;
mod terminal_view;

pub(crate) use client_v0::raw_terminal::splice as raw_terminal_splice;

#[derive(Clone)]
pub struct AppState {
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
    native_only: bool,
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
    crate::performance::record_wake("api", None);
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
    crate::performance::record_wake("api", Some(kind));
    state.notify.notify_one();
    signal_visible_change(state);
}

// Usage samples and lease renewals are durable and visible, but neither can
// advance a mission on its own. Terminal child state, claim expiry deadlines,
// and harness readiness still wake the reconciler through their own paths.
// A local observation never replicates and cannot advance a mission, so it only
// wakes clients that follow this node's event feed.
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
            "stale-subject"
            | "owned-set-refused"
            | "set-managed-subject"
            | "missing-subject-token"
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
            "internal" => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        };
        Self {
            status,
            code: error.code.into(),
            message: error.message,
            details: Box::new(error.details),
        }
    }

    fn internal(error: impl std::fmt::Display + 'static) -> Self {
        let typed = &error as &dyn std::any::Any;
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
        (
            self.status,
            Json(json!({ "code": self.code, "message": self.message, "details": self.details })),
        )
            .into_response()
    }
}

pub fn router(state: AppState) -> Router {
    delivery_presence::start();
    router_for_transport(state, ClientTransportBoundary::Unix)
}

/// Build the loopback-only client gateway. Unlike the local Unix boundary, every ordinary
/// client request on this router requires a paired bearer credential.
pub fn fabric_router(state: AppState) -> Router {
    router_for_transport(state, ClientTransportBoundary::FabricLoopback)
}

fn router_for_transport(state: AppState, transport: ClientTransportBoundary) -> Router {
    let app = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/client/capabilities", get(client_capabilities))
        .route("/v1/client/sets", get(owned_sets::list))
        .route("/v1/client/sets/{*id}", get(owned_sets::get))
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
        .route("/v1/client/documents/content", get(client_v0::document_get))
        .route("/v1/client/usage", get(client_v0::usage_period))
        .route(
            "/v1/client/subject-definition",
            get(client_v0::subject_definition),
        )
        .route("/v1/client/now", get(client_v0::now))
        .route("/v1/client/machines", get(client_v0::machines))
        .route("/v1/client/hosts/{*id}", get(client_v0::host_repositories))
        .route("/v1/client/devices", get(client_v0::devices))
        .route("/v1/client/attention", get(client_attention))
        .route("/v1/client/attention/{*id}", get(client_attention_detail))
        .route("/v1/client/messages", get(client_messages))
        .route("/v1/client/messages/{*id}", get(client_messages_detail))
        .route("/v1/client/launches", get(client_launches))
        .route(
            "/v1/client/launches/{id}/variants",
            get(client_launch_variants),
        )
        .route(
            "/v1/client/launches/{id}/variants/{variant}",
            get(client_launch_variant_detail),
        )
        .route(
            "/v1/client/launches/{id}/decisions",
            get(client_launch_decisions),
        )
        .route(
            "/v1/client/launches/{id}/decisions/{decision}",
            get(client_launch_decision_detail),
        )
        .route(
            "/v1/client/launches/{id}/approvals",
            get(client_launch_approvals),
        )
        .route(
            "/v1/client/launches/{id}/approvals/{approval}",
            get(client_launch_approval_detail),
        )
        .route("/v1/client/launches/{id}", get(client_launches_detail))
        .route("/v1/client/work", get(client_work))
        .route("/v1/client/work/{*id}", get(client_work_detail))
        .route("/v1/client/agents", get(client_agents))
        .route("/v1/client/resources", get(client_v0::resources::list))
        .route("/v1/client/agents/{*id}", get(client_agents_detail))
        .route(
            "/v1/client/agent-declarations/{*id}",
            get(client_v0::agent_declaration),
        )
        .route("/v1/client/agent-queues/{*id}", get(client_v0::agent_queue))
        .route("/v1/client/lanes", get(client_v0::lanes))
        .route("/v1/client/lanes/{*id}", get(client_v0::lane_detail))
        .route("/v1/client/history", get(client_history))
        .route("/v1/client/history/{*id}", get(client_history_detail))
        .route("/v1/client/conversations/search", get(client_v0::search::search))
        .route("/v1/client/sessions", get(client_sessions))
        .route("/v1/client/sessions/{*id}", get(client_sessions_detail))
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
        .route(
            "/v1/client/blobs",
            post(client_blobs::upload).layer(DefaultBodyLimit::max(client_blobs::UPLOAD_BODY_LIMIT)),
        )
        .route("/v1/client/blobs/{id}", get(client_blobs::get))
        .route("/v1/client/blobs/{id}/chunk", get(client_blobs::chunk))
        .route("/v1/client/pairings", post(client_v0::pairing_begin))
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
        .route("/v1/intent/mission", post(mission))
        .route("/v1/gate-checks", post(start_gate_check))
        .route("/v1/gate-checks/{id}", get(read_gate_check))
        .route("/v1/sets/preview", post(owned_sets::preview))
        .route("/v1/sets/apply", post(owned_sets::apply))
        .route("/v1/intent/apply", post(apply))
        .route("/v1/agents/rename", post(rename_agent))
        .route("/v1/agents/restart", post(restart_agent))
        .route("/v1/agents/rollout", post(rollout_agent))
        .route("/v1/agents/start", post(start_mission_seat))
        .route("/v1/agents/suspend", post(suspend_agent))
        .route("/v1/agents/resume", post(resume_agent))
        .route("/v1/agents/native-session", post(report_native_session))
        .route("/v1/missions/{id}", get(get_mission))
        .route("/v1/missions/{id}/retire", post(retire_mission))
        .route("/v1/launches/{id}", get(get_planning_session))
        .route("/v1/launches/{id}/submit", post(submit_planning_candidate))
        .route(
            "/v1/launches/{id}/variants/{variant}/submit",
            post(submit_named_planning_candidate),
        )
        .route(
            "/v1/launches/{id}/preview",
            post(preview_planning_candidate),
        )
        .route(
            "/v1/launches/{id}/variants/{variant}/preview",
            post(preview_named_planning_candidate),
        )
        .route(
            "/v1/launches/{id}/variants/{left}/compare/{right}",
            get(compare_planning_variants),
        )
        .route(
            "/v1/launches/{id}/variants/{variant}/propose",
            post(propose_planning_variant),
        )
        .route("/v1/launches/{id}/approve", post(approve_planning_session))
        .route(
            "/v1/launches/{id}/approve-and-launch",
            post(approve_and_start_launch),
        )
        .route("/v1/launches/{id}/start", post(start_approved_launch))
        .route("/v1/launches/{id}/decisions", post(request_launch_decision))
        .route(
            "/v1/launches/{id}/decisions/{decision}/answer",
            post(answer_launch_decision),
        )
        .route("/v1/launches", post(start_planning_session))
        .route("/v1/launches/{id}/revise", post(revise_planning_session))
        .route("/v1/launches/{id}/cancel", post(cancel_planning_session))
        .route("/v1/documents", get(list_documents).post(put_document))
        .route("/v1/rules", get(list_rules))
        .route("/v1/rules/audit", get(list_rule_audits))
        .route("/v1/rules/set", post(set_rule))
        .route("/v1/documents/content", get(get_document))
        .route("/v1/diagnostics/harness", post(post_harness_diagnostic))
        .route("/v1/delivery/hold", get(get_delivery_hold).post(post_delivery_hold))
        .route("/v1/claims", get(list_claims).post(post_claim))
        .route("/v1/usage", get(get_usage))
        .route("/v1/claims/by-id/{id}", get(get_claim))
        .route("/v1/reviews", get(list_reviews))
        .route("/v1/reviews/{*subject}", post(post_review))
        .route("/v1/attention", get(list_attention).post(request_attention))
        .route("/v1/attention/resolve/{*subject}", post(resolve_attention))
        .route("/v1/subscription-requests", get(list_subscription_requests))
        .route(
            "/v1/subscription-requests/{decision}/{request}",
            post(decide_subscription_request),
        )
        .route(
            "/v1/attention/withdraw/{*subject}",
            post(withdraw_attention),
        )
        .route("/v1/messages", get(list_messages).post(send_message))
        .route("/v1/messages/page", get(list_messages_page))
        .route("/v1/mailbox", get(mailbox::subscribe))
        .route("/v1/harness-events", post(harness_events::publish))
        .route("/v1/mailbox/bind", post(mailbox::bind))
        .route("/v1/mailbox/receipts", post(mailbox::receipt))
        .route("/v1/messages/{message_id}/claims", post(post_message_claim))
        .route("/v1/messages/read/{*subject}", get(read_message))
        .route("/v1/messages/delivery/{*subject}", get(message_delivery))
        .route("/v1/messages/by-key", get(message_by_key))
        .route("/v1/status", get(status))
        .route("/v1/desired/{*subject}", get(get_desired))
        .route("/v1/events", get(events))
        .route("/v1/doctor", get(doctor))
        .route("/v1/repair", get(operational_repair_plan))
        .route("/v1/repair/apply", post(apply_operational_repair))
        .route("/v1/backup", get(backup_export))
        .route("/v1/replication/status", get(replication_status))
        .route("/v1/replication/records", get(replication_records))
        .route("/v1/replication/records/{*record}", get(replication_record))
        .route("/v1/replication/repair", post(repair_replication_record))
        .route("/v1/checkpoint/plan", post(checkpoint_plan))
        .route("/v1/checkpoint/status", get(checkpoint_status))
        .route("/v1/checkpoint/excuse", post(checkpoint_excuse))
        .route("/v1/checkpoint/resume", post(checkpoint_resume))
        .route(
            "/v1/internal/replication/export",
            post(replication_export).layer(DefaultBodyLimit::max(crate::peer::MAX_EXCHANGE_BYTES)),
        )
        .route(
            "/v1/internal/replication/receive",
            post(replication_receive).layer(DefaultBodyLimit::max(crate::peer::MAX_EXCHANGE_BYTES)),
        )
        .route(
            "/v1/internal/replication/peer-failure",
            post(replication_peer_failure),
        )
        .route(
            "/v1/internal/replication/checkpoint",
            post(replication_checkpoint_manifest),
        )
        .route(
            "/v1/internal/replication/checkpoint-need",
            post(replication_checkpoint_need),
        )
        .route(
            "/v1/internal/replication/checkpoint-adopt",
            post(replication_checkpoint_adopt)
                .layer(DefaultBodyLimit::max(crate::peer::MAX_MANIFEST_BYTES)),
        )
        .route(
            "/v1/internal/replication/heal/answer",
            post(replication_heal_answer)
                .layer(DefaultBodyLimit::max(crate::peer::MAX_EXCHANGE_BYTES)),
        )
        .route(
            "/v1/internal/replication/heal/next",
            post(replication_heal_next)
                .layer(DefaultBodyLimit::max(crate::peer::MAX_EXCHANGE_BYTES)),
        )
        .route("/v1/internal/fleet/membership", get(fleet_membership_view))
        .route("/v1/internal/fleet/status", get(fleet_status))
        .route(
            "/v1/internal/fleet/invites",
            get(fleet_invite_list).post(fleet_invite_create),
        )
        .route(
            "/v1/internal/fleet/invites/revoke",
            post(fleet_invite_revoke),
        )
        .route("/v1/internal/fleet/redeem", post(fleet_redeem))
        .route("/v1/internal/fleet/remove", post(fleet_remove))
        .route("/v1/internal/fleet/leave/begin", post(fleet_leave_begin))
        .route("/v1/internal/fleet/leave/cancel", post(fleet_leave_cancel))
        .route("/v1/internal/fleet/leave/claim", post(fleet_leave_claim))
        .route(
            "/v1/internal/fleet/endpoints",
            post(fleet_publish_endpoints),
        )
        .route("/v1/internal/replication-wake", post(replication_wake))
        .route(
            crate::peer::CLIENT_READ_FORWARD_PATH,
            post(forward_client_read).layer(DefaultBodyLimit::max(16_384)),
        )
        .route("/v1/evals", post(start_eval))
        .route("/v1/evals/{*run}", get(get_eval))
        .route("/v1/mission-runs", get(list_mission_runs))
        .route("/v1/mission-overview", get(mission_overview))
        .route("/v1/outcome-history", get(outcome_history))
        .route("/v1/performance", get(performance_report))
        .route(
            "/v1/mission-runs/{run}/generations",
            get(list_run_generations),
        )
        .route(
            "/v1/mission-runs/{run}/revision-proposal",
            get(get_run_revision_proposal),
        )
        .route("/v1/mission-runs/{run}/revision", post(revise_mission_run))
        .route(
            "/v1/mission-runs/{run}/outcome",
            post(set_mission_run_outcome),
        )
        .route("/v1/mission-runs/{run}", get(get_mission_run))
        .route("/v1/run-generations/{generation}", get(get_run_generation))
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
        .route("/v1/work/ask", post(ask_person))
        .route("/v1/github/watch", post(github_watch::watch))
        .route("/v1/github/unwatch", post(github_watch::unwatch))
        .route("/v1/github/watches", get(github_watch::watches))
        .route("/v1/github/comment", post(github_watch::comment))
        .route("/v1/github/own", post(github_watch::own))
        .route("/v1/work/done", post(done_person_step))
        .route("/v1/work/cancel-ask", post(cancel_person_ask))
        .route("/v1/work", get(list_work))
        .route("/v1/work-items/{*subject}", get(get_work))
        .route("/v1/work/mission/{*subject}", post(publish_work_mission))
        .route("/v1/work/wake/{*subject}", post(wake_work))
        .route("/v1/work/retry/{*subject}", post(retry_work))
        .route("/v1/work/extend/{*subject}", post(extend_work))
        .route("/v1/work/{action}/{*subject}", post(post_work_action))
        .route("/v1/gate-results", post(post_gate_result))
        .route("/v1/agent-queue-moves", post(move_agent_queue))
        .route("/v1/lanes", get(list_lanes))
        .route("/v1/lanes/{*lane}", get(get_lane))
        .route("/v1/lane-changes", post(change_lane))
        .route("/v1/sessions", get(list_sessions))
        .route("/v1/sessions/{subject}/context/clear", post(clear_context))
        .route("/v1/sessions/{subject}/signal", post(signal_session))
        .route("/v1/sessions/input/{*subject}", post(input_session))
        .route("/v1/sessions/logs/{*subject}", get(logs_session))
        .route("/v1/sessions/screen/{*subject}", get(screen_session))
        .route("/v1/sessions/attach/{*subject}", post(attach_session))
        .route("/v1/sessions/{subject}/attach", post(attach_session))
        .route(
            "/v1/sessions/local-terminal/{*subject}",
            get(local_terminal),
        )
        .route("/v1/sessions/terminal/{*subject}", get(terminal_session))
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
    State((state, transport)): State<(AppState, ClientTransportBoundary)>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let started = Instant::now();
    let request_path = request.uri().path().to_owned();
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
    let profile = crate::profile::Op::start(
        format!("{} {request_route}", request.method()),
        Some(caller.clone()),
    );
    let client_request = request.uri().path().starts_with("/v1/client/");
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
            .and_then(|cursor| decode_client_cursor(&cursor).ok())
            .map(|cursor| cursor.snapshot)
    });
    // Authentication can scan pairing claims, and creating a snapshot reads the store. Both
    // must leave the async acceptor free to admit independent requests when SQLite is busy.
    let (client_authentication, client_snapshot) = if client_request {
        let mut auth_request = Request::builder()
            .method(request.method().clone())
            .uri(request.uri().clone())
            .body(Body::empty())
            .expect("the incoming request has a valid method and URI");
        *auth_request.headers_mut() = request.headers().clone();
        let auth_state = state.clone();
        let transport = transport.as_str();
        let auth_profile = profile.clone();
        let admitted = tokio::task::spawn_blocking(move || {
            let _entered = crate::profile::enter(auth_profile.as_ref());
            let authentication = client_v0::authenticate(&auth_state, &auth_request, transport);
            let snapshot = client_request_snapshot(&auth_state, cursor_snapshot.flatten());
            (authentication, snapshot)
        })
        .await;
        match admitted {
            Ok((authentication, snapshot)) => (authentication.map(Some), Some(snapshot)),
            Err(error) => (Err(ApiError::internal(error)), None),
        }
    } else {
        (Ok(None), None)
    };
    if let Ok(Some(session)) = &client_authentication {
        request.extensions_mut().insert(session.clone());
    }
    if let Some(snapshot) = &client_snapshot {
        request.extensions_mut().insert(snapshot.clone());
    }
    let response = match (fabric_boundary_error, client_authentication) {
        (Some(error), _) | (None, Err(error)) => error.into_response(),
        // Most handlers use synchronous SQLite and filesystem APIs. Run the whole
        // handler on a blocking thread so a busy projection or replication pass cannot
        // occupy an async worker needed to accept another call. Each read on that
        // thread takes its own read connection, so it never waits for another read.
        (None, Ok(_)) if request_path == "/v1/health" => next.run(request).await,
        (None, Ok(_)) => {
            let runtime = tokio::runtime::Handle::current();
            let handler_profile = profile.clone();
            let cpu_kind = request_route.clone();
            let cpu_client = caller.clone();
            match tokio::task::spawn_blocking(move || {
                if let Some(profile) = &handler_profile {
                    profile.queued();
                }
                let _entered = crate::profile::enter(handler_profile.as_ref());
                crate::performance::with_cpu(Some(&cpu_kind), Some(&cpu_client), || {
                    runtime.block_on(next.run(request))
                })
            })
            .await
            {
                Ok(response) => response,
                Err(error) => ApiError::internal(error).into_response(),
            }
        }
    };
    if response.status() == StatusCode::SWITCHING_PROTOCOLS
        || !response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"))
    {
        record_request_latency(&request_route, &request_path, &caller, started);
        if let Some(profile) = profile {
            profile.finish();
        }
        return response;
    }
    let enveloping = Instant::now();
    let status = response.status();
    let (mut parts, body) = response.into_parts();
    // A page read inside one SQLite snapshot names that snapshot, which can be newer than the
    // one this request was admitted at.
    let client_snapshot = parts
        .extensions
        .remove::<ClientSnapshot>()
        .or(client_snapshot);
    let raw = match to_bytes(body, usize::MAX).await {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|error| {
            json!({
                "code": "invalid-server-json",
                "message": error.to_string(),
            })
        }),
        Err(error) => json!({
            "code": "response-read-failed",
            "message": error.to_string(),
        }),
    };
    let store_index = if client_request {
        client_snapshot
            .as_ref()
            .map(|snapshot| snapshot.store_index)
            .unwrap_or_default()
    } else {
        // index() is an atomic load. Health must not queue behind blocking
        // handlers just to decorate its response.
        state.store.index().unwrap_or_default()
    };
    let request_id = if client_request {
        format!("request/{}", new_request_id())
    } else {
        new_request_id()
    };
    let envelope = if client_request && status.is_success() {
        json!({
            "api_version": CLIENT_API_VERSION,
            "request_id": request_id,
            "snapshot": client_snapshot.expect("a successful client request has a snapshot"),
            "value": raw,
        })
    } else if client_request {
        client_error_envelope(status, &raw, &request_id)
    } else if status.is_success() {
        json!({
            "api_version": "st3.v1",
            "request_id": request_id,
            "snapshot_host": state.node,
            "store_index": store_index,
            "value": raw,
        })
    } else {
        json!({
            "api_version": "st3.v1",
            "request_id": request_id,
            "snapshot_host": state.node,
            "store_index": store_index,
            "code": raw.get("code").and_then(Value::as_str).unwrap_or("request-failed"),
            "message": raw.get("message").and_then(Value::as_str).unwrap_or("the request failed"),
            "details": raw.get("details").cloned().unwrap_or_else(|| json!({})),
        })
    };
    let body = serde_json::to_vec(&envelope).unwrap_or_else(|_| b"{}".to_vec());
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    record_request_latency(&request_route, &request_path, &caller, started);
    if let Some(profile) = profile {
        profile.enveloped(enveloping.elapsed(), body.len());
        profile.finish();
    }
    Response::from_parts(parts, Body::from(body))
}

#[derive(Default)]
struct RouteLatency {
    count: u64,
    recent_ms: VecDeque<u64>,
}

static REQUEST_LATENCY: OnceLock<Mutex<BTreeMap<String, RouteLatency>>> = OnceLock::new();

fn request_latency() -> &'static Mutex<BTreeMap<String, RouteLatency>> {
    REQUEST_LATENCY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn request_latency_snapshot() -> Vec<Value> {
    let routes = request_latency().lock().unwrap();
    routes
        .iter()
        .map(|(route, latency)| {
            let mut sorted = latency.recent_ms.iter().copied().collect::<Vec<_>>();
            sorted.sort_unstable();
            let percentile = |percent: usize| {
                sorted
                    .get(
                        ((sorted.len().saturating_mul(percent).saturating_add(99)) / 100)
                            .saturating_sub(1),
                    )
                    .copied()
                    .unwrap_or_default()
            };
            json!({
                "route": route,
                "count": latency.count,
                "recent_count": sorted.len(),
                "p50_ms": percentile(50),
                "p99_ms": percentile(99),
                "max_ms": sorted.last().copied().unwrap_or_default(),
            })
        })
        .collect()
}

fn record_request_latency(route: &str, path: &str, caller: &str, started: Instant) {
    let elapsed = started.elapsed();
    crate::performance::record_request(route, Some(caller), elapsed);
    {
        let mut routes = request_latency().lock().unwrap();
        if routes.len() < 256 || routes.contains_key(route) {
            let sample = routes.entry(route.to_owned()).or_default();
            sample.count = sample.count.saturating_add(1);
            if sample.recent_ms.len() == 512 {
                sample.recent_ms.pop_front();
            }
            sample.recent_ms.push_back(elapsed.as_millis() as u64);
        }
    }
    if elapsed < Duration::from_secs(1) {
        return;
    }
    // Request latency is an operational sample, not a graph change. Publishing a
    // claim here causes replication and reconciliation on every node, including
    // during the contention that made the request slow in the first place.
    eprintln!("st3: slow request {path} took {} ms", elapsed.as_millis());
}

fn new_request_id() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        let fallback = format!(
            "{}:{}:{:?}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            std::thread::current().id()
        );
        bytes.copy_from_slice(&Sha256::digest(fallback.as_bytes())[..16]);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes(bytes[0..4].try_into().expect("four bytes")),
        u16::from_be_bytes(bytes[4..6].try_into().expect("two bytes")),
        u16::from_be_bytes(bytes[6..8].try_into().expect("two bytes")),
        u16::from_be_bytes(bytes[8..10].try_into().expect("two bytes")),
        u64::from_be_bytes([
            0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ])
    )
}

fn client_now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn client_timestamp(unix_ms: u128) -> String {
    let unix_ms = i64::try_from(unix_ms).unwrap_or(i64::MAX);
    chrono::DateTime::from_timestamp_millis(unix_ms)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
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
    let created_at = client_timestamp(
        state
            .store
            .projection_time_at(store_index)
            .unwrap_or_default(),
    );
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
    }
}

fn client_host_id(node: &str) -> String {
    format!("host/{}", node.replace(char::is_whitespace, "-"))
}

fn client_error_envelope(status: StatusCode, raw: &Value, request_id: &str) -> Value {
    json!({
        "api_version": CLIENT_API_VERSION,
        "error_version": "st3.client.error.v0",
        "request_id": request_id,
        "code": client_error_code(raw.get("code").and_then(Value::as_str)),
        "message": raw.get("message").and_then(Value::as_str).unwrap_or("the request failed"),
        "retryable": client_error_retryable(status, raw.get("code").and_then(Value::as_str)),
        "details": raw.get("details").cloned().unwrap_or_else(|| json!({})),
    })
}

fn client_error_retryable(status: StatusCode, code: Option<&str>) -> bool {
    matches!(
        code,
        Some(
            "remote-unavailable"
                | "terminal-unavailable"
                | "cursor-gap"
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
        "not-found"
        | "forbidden"
        | "attention-migrated"
        | "unsupported-capability"
        | "validation-failed"
        | "idempotency-conflict"
        | "stale-fence"
        | "timeline-history-incomplete"
        | "cursor-gap"
        | "page-cursor-expired"
        | "rate-limited"
        | "runtime-not-local"
        | "runtime-authority-indeterminate"
        | "remote-unavailable"
        | "terminal-unavailable"
        | "terminal-ended"
        | "blob-too-large"
        | "unsupported-media-type"
        | "blob-content-mismatch"
        | "blob-quota-exceeded"
        | "blob-not-found"
        | "blob-expired"
        | "internal" => code.unwrap_or("internal").to_owned(),
        "too-many-attachments" | "invalid-blob-reference" => "validation-failed".into(),
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
    let end = offset.saturating_add(limit).min(items.len());
    let page_items = items.get(offset..end).unwrap_or_default().to_vec();
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
        filters: client_page_filters(query),
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
fn client_page_filters(query: &ClientListQuery) -> BTreeMap<String, String> {
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
            let items = read(&reader, &snapshot)?;
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
        .map(|(peer, sync)| ClientSyncPeer {
            host_id: client_host_id(&peer),
            peer_only_envelopes: sync.peer_only_envelopes,
            local_only_envelopes: sync.local_only_envelopes,
            last_exchange_at: state
                .store
                .replication_peer_last_success(&peer)
                .ok()
                .flatten()
                .map(client_timestamp),
            estimated_catch_up_seconds: sync.estimated_catch_up_seconds,
            diverged_since: sync
                .diverged
                .then_some(sync.graph_differs_since_unix_ms)
                .flatten()
                .map(client_timestamp),
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
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<client_v0::ClientSession>,
) -> Json<Value> {
    let cursor = format!("event-cursor/{}/{}", state.node, snapshot.store_index);
    let oldest = state
        .store
        .event_bounds()
        .map(|(oldest, _)| oldest.saturating_sub(1))
        .unwrap_or_default();
    let capabilities = client_v0::capabilities(&session);
    Json(json!({
        "kind": "capabilities",
        "machine_version": st_drivers::version::machine_version(),
        "session_actor": session.actor,
        "transport": session.transport,
        "capabilities": capabilities,
        "limits": {
            "max_page_items": CLIENT_MAX_PAGE_ITEMS,
            "max_event_items": 500,
            "max_response_bytes": CLIENT_MAX_RESPONSE_BYTES,
            "max_wait_ms": 30_000,
            "max_glass_body_bytes": st3_schema::glasses::MAX_BODY_BYTES,
            "max_glasses": st3_schema::glasses::MAX_GLASSES,
            "max_glass_depth": st3_schema::glasses::MAX_DEPTH,
            "max_glass_nodes": st3_schema::glasses::MAX_NODES
        },
        "event_cursor": cursor,
        "oldest_event_cursor": format!("event-cursor/{}/{oldest}", state.node),
        "schemas": [
            "../client-v0/schemas/client-v0.schema.json",
            "../client-v0/schemas/operations.json"
        ]
    }))
}

fn client_work_resources(
    store: &Store,
    actor: Option<&str>,
    history: bool,
    snapshot_unix_ms: u128,
    snapshot_index: u64,
) -> anyhow::Result<Vec<Value>> {
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

/// Translate internal step states once for every client projection.
fn client_work_state(status: &str) -> &str {
    match status {
        "pending" => "waiting",
        "working" => "claimed",
        other => other,
    }
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
                "state": state,
                "agentless": work.agentless,
                "gate_kind": gate_kind,
                "attempt": work.attempt,
                "readiness_epoch": work.readiness_epoch,
                "claimant": work.claimant,
                "claim_incarnation": work.claim_incarnation,
                "claim_expires_at_unix_ms": work.claim_expires_at_unix_ms,
                "execution_started_at_unix_ms": work.execution_started_at_unix_ms,
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
    let mut items = store.cached_agent_resources(snapshot_index, history, || {
        let mut items = client_agent_resources_uncached(store, history, snapshot_index)?;
        let subjects = items.iter().filter_map(|item| item["id"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        let observations = store.agent_todo_observations_for(&subjects, snapshot_index)?;
        for item in &mut items {
            let claims = observations.get(item["id"].as_str().unwrap_or_default());
            item["todo"] = client_v0::agent_todo_value(
                claims.and_then(|claims| claims.get("harness.todo.observed")),
                claims.and_then(|claims| claims.get("harness.session-file")),
                item["incarnation_id"].as_str(),
            );
        }
        Ok(items)
    })?;
    let local_host = client_host_id(store.origin());
    for item in &mut items {
        if item.get("updated_at").and_then(Value::as_str) == Some("") {
            item["updated_at"] = Value::String(at.to_owned());
        }
        overlay_delivery_presence(item, &local_host);
    }
    overlay_subagents(store, &mut items)?;
    Ok(items)
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
    })
}

/// Each seat's running subagents: open, with a lease that runs past this read. A lease runs out
/// without a claim, so this is read per request rather than cached with the agents.
fn overlay_subagents(store: &Store, items: &mut [Value]) -> anyhow::Result<()> {
    let mut running = BTreeMap::<String, Vec<Value>>::new();
    for subagent in store.running_subagents(client_now_ms() as u64)? {
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
    let assessment = delivery_presence::assess(recipient, &driver);
    if assessment.stale() {
        item["state"] = Value::String("waiting".into());
    }
    item["delivery"] = assessment.to_value();
}

fn client_agent_resources_uncached(
    store: &Store,
    history: bool,
    snapshot_index: u64,
) -> anyhow::Result<Vec<Value>> {
    // Without history the store reduces only agents that can be current, including unhealthy
    // ones; the filters below keep the current layer either way.
    let status = store.status_for_subject_prefix_at("agent/", Some(snapshot_index), history)?;
    let work_queues = store.agent_work_queues()?;
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
    let desired_hosts = store
        .desired_subjects_named(&agent_subjects)?
        .into_iter()
        .filter_map(|desired| {
            let host = desired.member.map(|member| member.host)?;
            Some((desired.subject, client_host_id(&host)))
        })
        .collect::<BTreeMap<_, _>>();
    let usage_summaries = store.usage_summaries_at(&agent_subjects, Some(snapshot_index))?;
    let member_faults = store.member_reconcile_faults_for(&agent_subjects, snapshot_index)?;
    let queued_steps = work_queues
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
    let step_labels = store.step_labels(&queued_steps)?;
    let label = |id: &String| {
        step_labels.get(id).map(|step| {
            json!({
                "id": id,
                "mission_id": step.mission,
                "mission_run_id": step.run,
                "path": step.path,
                "title": step.title,
                "goal": step.goal,
                "state": client_work_state(&step.status),
                "since": client_timestamp(step.updated_at_unix_ms),
            })
        })
    };
    let mut agents = status
        .subjects
        .into_iter()
        .filter(|subject| {
            subject.subject.starts_with("agent/") || subject.kind.as_deref() == Some("agent")
        })
        .filter(|subject| history || subject.projection.layer == "current")
        .map(|subject| -> anyhow::Result<(String, Value)> {
            let fault = member_faults.get(&subject.subject);
            let fields = subject
                .actual
                .as_ref()
                .map(|actual| actual.get("fields").unwrap_or(actual));
            let observed = fields
                .and_then(|fields| fields.get("status"))
                .and_then(Value::as_str);
            let driver = subject
                .harness
                .as_ref()
                .and_then(|harness| harness.driver.clone())
                .or_else(|| subject.desired.as_ref().and_then(desired_harness_driver));
            let harness_state = subject
                .harness
                .as_ref()
                .map(|harness| harness.state.clone());
            let last_activity_at = store.agent_last_activity_at(
                &subject.subject,
                subject
                    .harness
                    .as_ref()
                    .map(|harness| harness.incarnation_id.as_str()),
                snapshot_index,
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
                    Some("indeterminate" | "unknown" | "unauthenticated" | "blocked"),
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
            let state = if fault.is_some() { "failed" } else { state };
            let suspension = crate::suspension::current(store, &subject.subject)?;
            // A suspended seat has no process by design: it is neither stopped nor failed.
            let state = match suspension.as_ref().map(|item| item.phase.as_str()) {
                Some("suspended") if fault.is_none() => "suspended",
                Some("snapshotting" | "restoring") if state == "stopped" => "suspended",
                _ => state,
            };
            let runtime_id = fields
                .and_then(|fields| fields.get("runtime_id"))
                .and_then(Value::as_str);
            let runtime_ids = runtime_id
                .map(|runtime| vec![format!("runtime/{runtime}")])
                .unwrap_or_default();
            let incarnation_id = fields
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
                        store
                            .claim_by_id(claim)
                            .ok()
                            .flatten()
                            .map(|claim| client_timestamp(claim.accepted_at_unix_ms))
                    })
                })
                .unwrap_or_default();
            let name = crate::model::effective_agent_name(
                &subject.subject, subject.desired.as_ref(),
            ).to_owned();
            let revision = subject
                .desired_revision
                .clone()
                .or_else(|| subject.claims.last().cloned())
                .unwrap_or_else(|| format!("agent/{}", subject.subject));
            let queue = work_queues
                .get(&subject.subject)
                .cloned()
                .unwrap_or_default();
            let usage = usage_summaries.get(&subject.subject);
            let value = json!({
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
                "harness_state": harness_state,
                "blocked_on": subject.harness.as_ref().and_then(|harness| harness.blocked_on.as_deref()),
                "ask": subject.harness.as_ref().and_then(|harness| harness.ask.as_deref()),
                "reason": subject.harness.as_ref().and_then(|harness| harness.reason.as_deref()),
                "host_id": desired_hosts.get(&subject.subject),
                "last_activity_at": last_activity_at.map(client_timestamp),
                "silent_since": silent_since.map(client_timestamp),
                "fault": fault,
                "incarnation_id": incarnation_id,
                "current_session_id": current_session_id,
                "current_work_ids": queue.current_work_ids,
                "active_work_count": queue.active_work_count,
                "next_work_id": queue.next_work_id,
                "upcoming_work_ids": queue.upcoming_work_ids,
                "queued_work_count": queue.queued_work_count,
                "current_work": queue.current_work_ids.iter().filter_map(label).collect::<Vec<_>>(),
                "next_work": queue.next_work_id.as_ref().and_then(label),
                "upcoming_work": queue.upcoming_work_ids.iter().filter_map(label).collect::<Vec<_>>(),
                "usage": usage,
                "under": subject.under.into_iter().map(|relationship| json!({
                    "agent_id": relationship.agent,
                    "reason": relationship.reason
                })).collect::<Vec<_>>(),
                "operational": subject.projection,
                "suspension": suspension.as_ref().map(client_suspension),
                "rollout": store.rollout(&subject.subject)?,
            });
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

fn managed_session_owner_at(
    store: &Store,
    snapshot_index: u64,
    session_id: &str,
) -> anyhow::Result<Option<(String, Option<String>, Option<String>)>> {
    let status = store.status_for_subject_prefix_at("agent/", Some(snapshot_index), true)?;
    for subject in status.subjects {
        if !subject.subject.starts_with("agent/") && subject.kind.as_deref() != Some("agent") {
            continue;
        }
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
        if managed_session_id(&subject.subject, identity) == session_id {
            return Ok(Some((
                subject.subject,
                incarnation.map(str::to_owned),
                subject.actual_origin,
            )));
        }
    }
    Ok(None)
}

/// How many of a subject's claims, oldest first, date its session in the session list.
const SESSION_CLAIMS: usize = 10_000;

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
        let running = session.process.is_some();
        let process = session.process.as_ref().map(|process| {
            json!({
                "pid": process.pid,
                "started_at": crate::external_sessions::timestamp(process.started_at_unix_ms),
                "fingerprint": process.fingerprint,
                "exact_session": process.exact_session
            })
        });
        sessions.push(json!({
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
        }));
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
    let status = store.status_for_subject_prefix_at("agent/", Some(snapshot_index), history)?;
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
        // A session is dated by the subject's first SESSION_CLAIMS claims, the page this list
        // once read whole and filtered, so a subject with more keeps the dates it had.
        let through_claim = subject
            .claims
            .get(SESSION_CLAIMS - 1)
            .filter(|_| subject.claims.len() > SESSION_CLAIMS);
        let mut accepted_times = store
            .runtime_claim_span_at(
                &subject.subject,
                incarnation,
                runtime,
                snapshot_index,
                through_claim.map(String::as_str),
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
            .map(client_timestamp)
            .or_else(|| accepted_times.iter().min().copied().map(client_timestamp))
            .unwrap_or_else(|| at.to_owned());
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

fn client_attention_actions(kind: &str, review_mode: Option<&str>) -> Vec<&'static str> {
    match kind {
        "human-gate" if review_mode == Some("feedback") => {
            vec!["review.approve", "review.request-changes"]
        }
        "human-gate" => vec!["review.approve", "review.reject"],
        "launch-approval" => vec!["launch.approve", "launch.cancel"],
        "revision-approval" => {
            vec!["mission.approve-revision", "mission.cancel-revision"]
        }
        "unread-message" => vec!["message.read"],
        "person-step" => vec!["work.done"],
        "fault" | "agent-request" => Vec::new(),
        _ => Vec::new(),
    }
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

/// An attention card's ID: its source, recipient and waiting episode. A source asked again
/// (a human gate's new request, a person step's new episode) gets a new card.
fn client_attention_id(subject: &str, person: &str, episode: &str) -> anyhow::Result<String> {
    let identity = serde_json::to_vec(&(subject, person, episode))?;
    Ok(format!(
        "attention/{}",
        &hex::encode(Sha256::digest(identity))[..32]
    ))
}

fn client_attention_resources(
    store: &Store,
    person: Option<&str>,
    _history: bool,
) -> anyhow::Result<Vec<Value>> {
    let current = store.attention_snapshot(person, client_now_ms())?;
    let mut resources = Vec::new();
    for item in current {
        let id = client_attention_id(&item.subject, &item.person, &item.episode)?;
        let mut resource = json!({
            "id": id, "kind": "attention", "attention_kind": item.kind,
            "source_id": item.subject, "source_kind": item.kind, "episode": item.episode,
            "person_id": item.person, "revision": item.episode,
            "updated_at": client_timestamp(item.requested_at_unix_ms),
            "title": item.title, "detail": item.detail, "priority": item.priority,
            "state": "open", "requested_at": client_timestamp(item.requested_at_unix_ms),
            "targets": item.targets, "actions": client_attention_actions(&item.kind, item.review_mode.as_deref()),
            "operational": {"layer": "current", "actionable": true, "reasons": []}
        });
        for (name, value) in [
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
        if item.kind == "person-step" {
            resource["action_parameters"] =
                json!({"work.done": {"target_id": item.subject, "episode": item.episode}});
            match item.request {
                // An update asks nothing, so it is not a `request`: a client that predates
                // updates shows a free-text card, and any response to it reads it.
                Some(update) if update["type"] == "update" => {
                    resource["action_parameters"]["work.done"]["summary"] = json!("Read");
                    resource["action_parameters"]["work.done"]["answer"] = json!({"id": "read"});
                    resource["update"] = update;
                }
                Some(request) => resource["request"] = request,
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
        resources.push(json!({
            "id": message.subject,
            "kind": "message",
            "revision": last.map(|claim| claim.id.as_str()).unwrap_or("message/unknown"),
            "updated_at": client_timestamp(updated_at),
            "from": message.from,
            "to": message.to,
            "title": message.title,
            "content": message.content,
            "state": message.status,
            "delivery": message_delivery_value(&message.to, &message.status, sent_at, client_now_ms()),
            "sent_at": client_timestamp(sent_at),
            "session_id": session_id,
            "in_reply_to": message.in_reply_to,
            "tags": message.tags,
            "attachments": message.attachments.iter().map(client_attachment).collect::<Vec<_>>(),
            "operational": { "layer": if current { "current" } else { "history" }, "actionable": current, "reasons": reasons }
        }));
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
fn message_delivery_value(to: &str, status: &str, sent_at: u128, now: u128) -> Value {
    let read = matches!(status, "read" | "closed");
    let age_ms = now.saturating_sub(sent_at);
    let path = delivery_presence::known(to);
    let blocked = !read && path.as_ref().is_some_and(|path| path.stale());
    let reason = if read {
        "the recipient has read the message"
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
    json!({
        "state": if read { "read" } else if blocked || age_ms > 10_000 { "waiting" } else { "pending" },
        "phase": status, "reason": reason, "age_ms": age_ms,
        "recipient_delivery": path.map(|path| path.to_value()),
    })
}

async fn message_delivery(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let subject = if subject.starts_with("message/") {
        subject
    } else {
        format!("message/{subject}")
    };
    blocking_api(move || {
        let message = state.store.message(&subject).map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("message `{subject}` does not exist")))?;
        let claims = state.store.claims_for(&subject, Some("message.sent")).map_err(ApiError::internal)?;
        let sent_at = claims.first().map(|claim| claim.accepted_at_unix_ms).unwrap_or_default();
        Ok(Json(json!({ "id": subject, "from": message.from, "to": message.to,
            "delivery": message_delivery_value(&message.to, &message.status, sent_at, client_now_ms()) })))
    }).await
}

#[derive(Deserialize)]
struct MessageKeyQuery {
    key: String,
}

/// The message a send's idempotency key landed as, so a client whose send went unanswered can
/// tell whether it was sent before it sends again. Only a message send or reply answers; any
/// other key is `message-not-sent`.
async fn message_by_key(
    State(state): State<AppState>,
    Query(query): Query<MessageKeyQuery>,
) -> Result<Json<MessageSendReceipt>, ApiError> {
    blocking_api(move || {
        let not_sent = || ApiError {
            status: StatusCode::NOT_FOUND,
            code: "message-not-sent".into(),
            message: format!("no message was sent with idempotency key `{}`", query.key),
            details: Box::default(),
        };
        let claim = state
            .store
            .operation_claim(&query.key)
            .map_err(ApiError::internal)?
            .filter(|claim| claim.kind == "message.sent")
            .ok_or_else(not_sent)?;
        let message = state
            .store
            .message(&claim.subject)
            .map_err(ApiError::internal)?
            .ok_or_else(not_sent)?;
        Ok(Json(MessageSendReceipt {
            message,
            idempotency_key: query.key.clone(),
            already_sent: true,
            sent_at: Some(client_timestamp(claim.accepted_at_unix_ms)),
        }))
    })
    .await
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
        .desired_subjects()?
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

fn client_launch_approval_resources(
    state: &AppState,
    session: &PlanningSessionView,
) -> anyhow::Result<Vec<Value>> {
    let variants = client_launch_variant_resources(state, session)?;
    let tokens = variants
        .into_iter()
        .filter_map(|variant| {
            Some((
                variant["candidate_revision"].as_u64()?,
                (
                    variant["preview_token"].as_str()?.to_owned(),
                    variant["id"].as_str()?.to_owned(),
                ),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let claims = state.store.claims_for(&session.subject, None)?;
    let mut approvals = claims
        .into_iter()
        .filter(|claim| claim.kind == "planning-session.approved")
        .filter_map(|claim| {
            let revision = claim.body.pointer("/fields/candidate_revision")?.as_u64()?;
            let (token, variant_id) = tokens.get(&revision)?;
            let reviewer = claim
                .body
                .pointer("/fields/requester")
                .and_then(Value::as_str)
                .or(claim.actor.as_deref())?;
            Some(json!({
                "id": format!("launch-approval/{}/{}", session.id, revision),
                "kind": "launch-approval",
                "revision": claim.id,
                "updated_at": client_timestamp(claim.accepted_at_unix_ms),
                "launch_id": format!("launch/{}", session.id),
                "variant_id": variant_id,
                "reviewer": reviewer,
                "state": "approved",
                "preview_token": token,
                "decided_at": client_timestamp(claim.accepted_at_unix_ms),
                "operational": { "layer": "history", "actionable": false, "reasons": ["approved"] }
            }))
        })
        .collect::<Vec<_>>();
    approvals.sort_by(|left, right| {
        left["decided_at"]
            .as_str()
            .cmp(&right["decided_at"].as_str())
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(approvals)
}

fn client_launch_resources(state: &AppState, history: bool) -> anyhow::Result<Vec<Value>> {
    let store = &state.store;
    let sessions = store.planning_sessions(history)?;
    let mut resources = sessions
        .into_iter()
        .map(|session| {
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
            let decisions = client_launch_decision_resources(store, &session)?;
            let latest_variant = client_launch_variant_resources(state, &session)?.into_iter().rev().next();
            let visualization = latest_variant.as_ref().and_then(|variant| variant.get("visualization").cloned());
            let preview = latest_variant.as_ref().and_then(|variant| variant.get("preview").cloned());
            let preview_token = latest_variant.as_ref().and_then(|variant| variant.get("preview_token").cloned());
            let approval_ids = store.claims_for(&session.subject, None)?.into_iter().filter(|claim| claim.kind == "planning-session.approved").filter_map(|claim| claim.body.pointer("/fields/candidate_revision").and_then(Value::as_u64).map(|revision| format!("launch-approval/{}/{revision}", session.id))).collect::<Vec<_>>();
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
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    resources.sort_by(|left, right| {
        right["updated_at"]
            .as_str()
            .cmp(&left["updated_at"].as_str())
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(resources)
}

fn client_history_resource(claim: ClaimRecord) -> Value {
    let mut targets = vec![claim.subject.clone()];
    if let Some(actor) = &claim.actor
        && actor.contains('/')
        && !actor.chars().any(char::is_whitespace)
    {
        targets.push(actor.clone());
    }
    json!({
        "id": format!("history/{}", claim.store_index),
        "kind": "history",
        "revision": claim.id,
        "updated_at": client_timestamp(claim.accepted_at_unix_ms),
        "event_type": claim.kind,
        "occurred_at": client_timestamp(claim.accepted_at_unix_ms),
        "store_index": claim.store_index,
        "summary": format!("{} on {}", claim.kind, claim.subject),
        "targets": targets,
        "operational": { "layer": "history", "actionable": false, "reasons": ["audit"] }
    })
}

async fn client_work(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    if query.history {
        return client_work_history(&state, snapshot, &query).await;
    }
    let actor = query.actor.clone();
    client_snapshot_page(&state, snapshot, "work", &query, move |state, snapshot| {
        client_work_resources(
            &state.store,
            actor.as_deref(),
            false,
            client_snapshot_time(snapshot),
            snapshot.store_index,
        )
    })
    .await
}

/// The work history, read a page at a time in the order it shows, each page inside one SQLite
/// snapshot. Continuation seeks after the last update time and subject, so unrelated writes
/// do not invalidate it or shift its offset.
async fn client_work_history(
    state: &AppState,
    snapshot: ClientSnapshot,
    query: &ClientListQuery,
) -> Result<ClientPageResponse, ApiError> {
    let (offset, limit, expires_at_unix_ms, after_key) = if let Some(encoded) = &query.cursor {
        let cursor = decode_client_cursor(encoded)?;
        if cursor.collection != "work"
            || cursor.snapshot.id != snapshot.id
            || cursor.snapshot.store_index != snapshot.store_index
            || cursor.history != query.history
            || cursor.person != query.person
            || cursor.actor != query.actor
            || cursor.owner_run != query.owner_run
            || cursor.status != query.status
            || cursor.native_only != query.native_only
            || cursor.items_digest != "sql-page"
            || query
                .limit
                .is_some_and(|limit| limit.clamp(1, CLIENT_MAX_PAGE_ITEMS) != cursor.limit)
        {
            return Err(client_page_expired(
                "the page cursor does not match this collection, snapshot, or filter",
            ));
        }
        if client_now_ms() > cursor.expires_at_unix_ms {
            return Err(client_page_expired("the page cursor expired"));
        }
        (
            if cursor.after_key.is_some() {
                0
            } else {
                cursor.offset
            },
            cursor.limit,
            cursor.expires_at_unix_ms,
            cursor.after_key,
        )
    } else {
        (
            0,
            query
                .limit
                .unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS)
                .clamp(1, CLIENT_MAX_PAGE_ITEMS),
            client_now_ms().saturating_add(CLIENT_PAGE_TTL_MS),
            None,
        )
    };
    let reader = state.clone();
    let actor = query.actor.clone();
    let read = blocking_store(move || {
        reader.store.clone().read_snapshot(|index| {
            let snapshot = client_snapshot_at(&reader, index);
            let (items, has_more) = client_work_history_page_after(
                &reader.store,
                actor.as_deref(),
                client_snapshot_time(&snapshot),
                index,
                offset,
                limit,
                after_key.as_ref(),
            )?;
            Ok(Some((snapshot, items, has_more)))
        })
    })
    .await?;
    let Some((snapshot, items, has_more)) = read else {
        return Err(client_page_expired(
            "the snapshot changed; restart pagination from the first page",
        ));
    };
    let next_cursor = has_more
        .then(|| {
            encode_client_cursor(&ClientPageCursor {
                snapshot: snapshot.clone(),
                collection: "work".into(),
                offset: offset.saturating_add(items.len()),
                limit,
                history: query.history,
                person: query.person.clone(),
                actor: query.actor.clone(),
                owner_run: query.owner_run.clone(),
                status: query.status.clone(),
                native_only: query.native_only,
                items_digest: "sql-page".into(),
                before_index: None,
                after_key: None,
                expires_at_unix_ms,
            })
        })
        .transpose()?;
    let page = ClientResourcePage {
        kind: "page".into(),
        collection: "work".into(),
        filters: client_page_filters(query),
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
    Ok((Extension(snapshot), Json(page)))
}

async fn client_work_detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone();
    let actor = query.actor.clone();
    let snapshot_unix_ms = client_snapshot_time(&snapshot);
    let requested = id.clone();
    let item = blocking_store(move || {
        client_work_item(
            &store,
            &requested,
            actor.as_deref(),
            snapshot_unix_ms,
            snapshot.store_index,
        )
    })
    .await?;
    item.map(Json).ok_or_else(|| {
        ApiError::not_found(format!(
            "work `{}` does not exist",
            client_detail_id("work", &id)
        ))
    })
}

async fn client_agents(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    let history = query.history;
    let status = query.status.clone();
    client_snapshot_page(
        &state,
        snapshot,
        "agents",
        &query,
        move |state, snapshot| {
            let mut items = client_agent_resources(
                &state.store,
                history,
                &snapshot.created_at,
                snapshot.store_index,
            )?;
            if let Some(status) = status.as_deref() {
                items.retain(|item| item.get("state").and_then(Value::as_str) == Some(status));
            }
            Ok(items)
        },
    )
    .await
}

async fn client_agents_detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone();
    let history = query.history;
    let created_at = snapshot.created_at.clone();
    let snapshot_index = snapshot.store_index;
    let items = blocking_store(move || {
        client_agent_resources(&store, history, &created_at, snapshot_index)
    })
    .await?;
    client_detail(items, "agent", &id)
}

async fn client_sessions(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    let history = query.history;
    let native_only = query.native_only;
    client_snapshot_page(
        &state,
        snapshot,
        "sessions",
        &query,
        move |state, snapshot| {
            let mut items = client_session_resources(
                &state.store,
                history,
                &snapshot.created_at,
                snapshot.store_index,
                state.native_session_home.as_deref(),
                native_only,
            )?;
            if native_only {
                items.retain(|item| item.get("managed") == Some(&Value::Bool(false)));
            }
            Ok(items)
        },
    )
    .await
}

async fn client_sessions_detail(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<client_v0::ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<Value>, ApiError> {
    if let Some(id) = id.strip_suffix("/timeline") {
        // An agent's timeline is its current session's: st resolves it, not the client.
        let session_id = client_v0::conversation_session_id(&state, id)?;
        let id = session_id.as_str();
        let managed = managed_session_owner_at(&state.store, snapshot.store_index, &session_id)
            .map_err(ApiError::internal)?;
        if let Some((_, _, origin)) = managed {
            let remote_host = origin
                .as_deref()
                .filter(|origin| *origin != state.store.origin())
                .map(client_host_id);
            if let Some(remote_host) = remote_host {
                let relay = state
                    .client_relay
                    .as_ref()
                    .ok_or_else(|| remote_unavailable(&remote_host))?;
                if !client_v0::acting_party(&session) {
                    return Err(ApiError::bad(St3Error::new(
                        "forbidden",
                        "remote session detail requires a concrete person or agent",
                    )));
                }
                let value = relay
                    .read(
                        &remote_host,
                        &crate::peer::ClientReadRequest {
                            authority_actor: session.authority_actor.clone(),
                            relay: None,
                            request: crate::peer::ClientReadOperation::Timeline {
                                session_id,
                                limit: query.limit.unwrap_or(50).clamp(1, 200),
                                cursor: query.cursor.clone(),
                            },
                        },
                    )
                    .await
                    .map_err(|error| remote_read_error(&remote_host, error))?;
                return Ok(Json(value));
            }
        }
        return client_v0::timeline_value(&state, &snapshot, &session, id, &query);
    }
    client_detail(
        client_session_resources(
            &state.store,
            true,
            &snapshot.created_at,
            snapshot.store_index,
            state.native_session_home.as_deref(),
            false,
        )
        .map_err(ApiError::internal)?,
        "session",
        &id,
    )
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
        .ok_or_else(|| remote_unavailable(&target))?;
    relay.forward(&request).await.map(Json).map_err(|error| {
        match error.downcast_ref::<crate::peer::ClientReadRejected>() {
            Some(rejected) => ApiError {
                status: StatusCode::from_u16(rejected.status).unwrap_or(StatusCode::CONFLICT),
                code: rejected.code.clone(),
                message: rejected.message.clone(),
                details: Box::new(rejected.details.clone()),
            },
            None => remote_unavailable(&target),
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

/// A read that is not sent for a stated reason, in the shape of an unreachable owner.
fn remote_unavailable_because(host: &str, reason: &str, message: &str) -> ApiError {
    let mut error = remote_unavailable(host);
    error.details.insert("reason".into(), reason.into());
    error.message = message.into();
    error
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
        "page-cursor-expired"
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

async fn client_attention(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<client_v0::ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    let person = client_v0::person_filter(&session, query.person.as_deref())?;
    let mut effective_query = query.clone();
    effective_query.person.clone_from(&person);
    let history = query.history;
    client_snapshot_page(
        &state,
        snapshot,
        "attention",
        &effective_query,
        move |state, _| client_attention_resources_with_previews(state, person.as_deref(), history),
    )
    .await
}

async fn client_attention_detail(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<Value>, ApiError> {
    let person = client_v0::person_filter(&session, query.person.as_deref())?;
    client_detail(
        client_attention_resources_with_previews(&state, person.as_deref(), query.history)
            .map_err(ApiError::internal)?,
        "attention",
        &id,
    )
}

async fn client_messages(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Extension(session): Extension<client_v0::ClientSession>,
    Query(query): Query<ClientListQuery>,
) -> Result<ClientPageResponse, ApiError> {
    let person = client_v0::person_filter(&session, query.person.as_deref())?;
    let mut effective_query = query.clone();
    effective_query.person.clone_from(&person);
    let (history, actor) = (query.history, query.actor.clone());
    // An agent another host owns is listed by that host: its messages can reach this node late,
    // and a list that shows only what has arrived reads as empty while the replica lags.
    let owner = match actor.as_deref() {
        Some(actor) => remote_agent_owner(&state, actor).await?,
        None => None,
    };
    let mut replicated = None;
    if let (Some(owner), Some(actor)) = (owner.as_deref(), actor.as_deref()) {
        match relayed_messages_page(&state, &session, owner, actor, &query).await {
            Ok(mut page) => {
                page.replicated = Some(ClientReplicated {
                    owner_host_id: owner.to_owned(),
                    source: "owner".into(),
                    complete: true,
                    state: "current".into(),
                    reason: None,
                });
                return Ok((Extension(snapshot), Json(page)));
            }
            Err(error) if error.code == "remote-unavailable" => {
                let lagging = client_sync_notice(&state)
                    .is_some_and(|notice| notice.peers.iter().any(|peer| peer.host_id == owner));
                replicated = Some(ClientReplicated {
                    owner_host_id: owner.to_owned(),
                    source: "replica".into(),
                    complete: false,
                    state: if lagging { "lagging" } else { "unverified" }.into(),
                    reason: error
                        .details
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
            }
            Err(error) => return Err(error),
        }
    }
    let (snapshot, Json(mut page)) = client_snapshot_page(
        &state,
        snapshot,
        "messages",
        &effective_query,
        move |state, _| {
            client_message_resources(&state.store, person.as_deref(), history, actor.as_deref())
        },
    )
    .await
    .map(|(Extension(snapshot), page)| (snapshot, page))?;
    page.replicated = replicated;
    Ok((Extension(snapshot), Json(page)))
}

/// The host that owns `actor`'s runtime, when that is another host.
async fn remote_agent_owner(state: &AppState, actor: &str) -> Result<Option<String>, ApiError> {
    if !actor.starts_with("agent/") {
        return Ok(None);
    }
    let (store, subject) = (state.store.clone(), actor.to_owned());
    let origin = blocking_store(move || {
        Ok(store
            .status(Some(&subject))?
            .subjects
            .first()
            .and_then(|subject| subject.actual_origin.clone()))
    })
    .await?;
    Ok(origin
        .filter(|origin| origin != state.store.origin())
        .map(|origin| client_host_id(&origin)))
}

/// One page of an agent's messages as its owner lists them.
async fn relayed_messages_page(
    state: &AppState,
    session: &client_v0::ClientSession,
    owner: &str,
    actor: &str,
    query: &ClientListQuery,
) -> Result<ClientResourcePage, ApiError> {
    if !client_v0::acting_party(session) {
        return Err(remote_unavailable_because(
            owner,
            "not-relayed",
            "a remote agent's messages are relayed only for a concrete person or agent",
        ));
    }
    let relay = state
        .client_relay
        .as_ref()
        .filter(|relay| relay.reaches(owner))
        .ok_or_else(|| remote_unavailable(owner))?;
    let value = relay
        .read(
            owner,
            &crate::peer::ClientReadRequest {
                authority_actor: session.authority_actor.clone(),
                relay: None,
                request: crate::peer::ClientReadOperation::Messages {
                    actor: actor.to_owned(),
                    history: query.history,
                    limit: query.limit.map(|limit| limit.clamp(1, 200)),
                    cursor: query.cursor.clone(),
                },
            },
        )
        .await
        .map_err(|error| remote_read_error(owner, error))?;
    serde_json::from_value(value).map_err(ApiError::internal)
}

async fn client_messages_detail(
    State(state): State<AppState>,
    Extension(session): Extension<client_v0::ClientSession>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<Value>, ApiError> {
    let person = client_v0::person_filter(&session, query.person.as_deref())?;
    client_detail(
        client_message_resources(&state.store, person.as_deref(), query.history, None)
            .map_err(ApiError::internal)?,
        "message",
        &id,
    )
}

async fn client_launches(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    let items = client_launch_resources(&state, query.history).map_err(ApiError::internal)?;
    client_page(&state, &snapshot, "launches", items, &query).map(Json)
}

async fn client_launches_detail(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<Value>, ApiError> {
    let items = client_launch_resources(&state, query.history).map_err(ApiError::internal)?;
    // The route carries a session ID. A native ID may itself start with `launch/`.
    let resource_id = format!("launch/{id}");
    let id = if items.iter().any(|item| item["id"] == resource_id) {
        resource_id
    } else {
        id
    };
    client_detail(items, "launch", &id)
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

async fn client_launch_variants(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    let session = client_launch_session(&state, &id)?;
    let items = client_launch_variant_resources(&state, &session).map_err(ApiError::internal)?;
    client_page(
        &state,
        &snapshot,
        &format!("launches/{id}/variants"),
        items,
        &query,
    )
    .map(Json)
}

async fn client_launch_variant_detail(
    State(state): State<AppState>,
    AxumPath((id, variant)): AxumPath<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let session = client_launch_session(&state, &id)?;
    client_detail(
        client_launch_variant_resources(&state, &session).map_err(ApiError::internal)?,
        "launch-variant",
        &format!("{}/{variant}", session.id),
    )
}

async fn client_launch_decisions(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    let session = client_launch_session(&state, &id)?;
    let items =
        client_launch_decision_resources(&state.store, &session).map_err(ApiError::internal)?;
    client_page(
        &state,
        &snapshot,
        &format!("launches/{id}/decisions"),
        items,
        &query,
    )
    .map(Json)
}

async fn client_launch_decision_detail(
    State(state): State<AppState>,
    AxumPath((id, decision)): AxumPath<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let session = client_launch_session(&state, &id)?;
    client_detail(
        client_launch_decision_resources(&state.store, &session).map_err(ApiError::internal)?,
        "launch-decision",
        &decision,
    )
}

async fn client_launch_approvals(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    let session = client_launch_session(&state, &id)?;
    let items = client_launch_approval_resources(&state, &session).map_err(ApiError::internal)?;
    client_page(
        &state,
        &snapshot,
        &format!("launches/{id}/approvals"),
        items,
        &query,
    )
    .map(Json)
}

async fn client_launch_approval_detail(
    State(state): State<AppState>,
    AxumPath((id, approval)): AxumPath<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let session = client_launch_session(&state, &id)?;
    client_detail(
        client_launch_approval_resources(&state, &session).map_err(ApiError::internal)?,
        "launch-approval",
        &format!("{}/{approval}", session.id),
    )
}

async fn client_history(
    State(state): State<AppState>,
    Extension(snapshot): Extension<ClientSnapshot>,
    Query(query): Query<ClientListQuery>,
) -> Result<Json<ClientResourcePage>, ApiError> {
    let requested_limit = query
        .limit
        .unwrap_or(CLIENT_DEFAULT_PAGE_ITEMS)
        .clamp(1, CLIENT_MAX_PAGE_ITEMS);
    let (before_index, limit, offset, expires_at_unix_ms) = if let Some(encoded) = &query.cursor {
        let cursor = decode_client_cursor(encoded)?;
        if cursor.collection != "history"
            || cursor.snapshot.id != snapshot.id
            || cursor.snapshot.store_index != snapshot.store_index
            || cursor.history != query.history
            || cursor.person != query.person
            || cursor.actor != query.actor
            || cursor.owner_run != query.owner_run
            || cursor.status != query.status
            || cursor.native_only != query.native_only
            || query
                .limit
                .is_some_and(|limit| limit.clamp(1, CLIENT_MAX_PAGE_ITEMS) != cursor.limit)
        {
            return Err(client_page_expired(
                "the page cursor does not match this collection, snapshot, or filter",
            ));
        }
        if client_now_ms() > cursor.expires_at_unix_ms {
            return Err(client_page_expired("the page cursor expired"));
        }
        let before = cursor
            .before_index
            .ok_or_else(|| client_page_expired("the history cursor is malformed"))?;
        (
            before,
            cursor.limit,
            cursor.offset,
            cursor.expires_at_unix_ms,
        )
    } else {
        (
            snapshot.store_index.saturating_add(1),
            requested_limit,
            0,
            client_now_ms().saturating_add(CLIENT_PAGE_TTL_MS),
        )
    };
    let store = state.store.clone();
    let page =
        blocking_store(move || store.claims_page(None, None, 0, Some(before_index), true, limit))
            .await?;
    let has_more = page.next_cursor.is_some();
    let items = page
        .claims
        .into_iter()
        .map(client_history_resource)
        .collect::<Vec<_>>();
    let next_cursor = page
        .next_cursor
        .map(|next| {
            encode_client_cursor(&ClientPageCursor {
                snapshot: snapshot.clone(),
                collection: "history".into(),
                offset: offset.saturating_add(items.len()),
                limit,
                history: query.history,
                person: query.person.clone(),
                actor: query.actor.clone(),
                owner_run: query.owner_run.clone(),
                status: query.status.clone(),
                native_only: query.native_only,
                items_digest: String::new(),
                before_index: Some(next),
                after_key: None,
                expires_at_unix_ms,
            })
        })
        .transpose()?;
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
    if query.native_only {
        filters.insert("native_only".into(), "true".into());
    }
    Ok(Json(ClientResourcePage {
        kind: "page".into(),
        collection: "history".into(),
        filters,
        items,
        page: ClientPageInfo {
            limit,
            has_more,
            next_cursor,
            cursor_expires_at: has_more.then(|| client_timestamp(expires_at_unix_ms)),
        },
        sync: client_sync_notice(&state),
        replicated: None,
    }))
}

async fn client_history_detail(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let id = client_detail_id("history", &id);
    let index = id
        .strip_prefix("history/")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|index| *index > 0)
        .ok_or_else(|| ApiError::not_found(format!("history `{id}` does not exist")))?;
    let upper = index
        .checked_add(1)
        .ok_or_else(|| ApiError::not_found(format!("history `{id}` does not exist")))?;
    let store = state.store.clone();
    let page =
        blocking_store(move || store.claims_page(None, None, index - 1, Some(upper), true, 1))
            .await?;
    page.claims
        .into_iter()
        .next()
        .filter(|claim| claim.store_index == index)
        .map(client_history_resource)
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("history `{id}` does not exist")))
}

async fn blocking_store<T, F>(operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
{
    let profile = crate::profile::current();
    let cpu_kind = crate::performance::current();
    tokio::task::spawn_blocking(move || {
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
    tokio::task::spawn_blocking(move || {
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
    tokio::task::spawn_blocking(move || {
        let _entered = crate::profile::enter(profile.as_ref());
        crate::performance::with_charged(cpu_kind, operation)
    })
    .await
    .map_err(ApiError::internal)?
}

pub async fn serve_unix(socket: &Path, app: Router) -> anyhow::Result<()> {
    serve_unix_inner(socket, app, false).await
}

/// Make the daemon's first diagnostic report, which the operations collection lists, off the
/// request path. Some of its checks read the whole claim log, seconds of work on a busy host's
/// store; until it is made, the collection says so instead of making a read wait for it.
pub fn start_operation_report(state: &AppState) {
    client_v0::start_operation_report(state);
}

/// Read the headers of this host's native session transcripts off the request path as the
/// daemon starts. Saved-history session lists and session reads use this background inventory
/// instead of walking the transcript trees, so a cold tree cannot hold those requests.
pub fn start_native_session_discovery(state: &AppState) {
    crate::external_sessions::start_history_inventory(state.native_session_home.as_deref());
}

/// The local daemon binds a Unix peer to the harness identity inherited by that peer or one of
/// its parents. `state_socket` gives clients without the daemon's runtime environment a stable
/// address for the same listener. Test servers and the paired gateway use the unbound listener.
pub async fn serve_unix_bound(
    socket: &Path,
    state_socket: &Path,
    app: Router,
) -> anyhow::Result<()> {
    serve_unix_with_ancestor(socket, Some(state_socket), app, true, harness_ancestor).await
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
    crate::config::validate_unix_socket_path(socket, "--socket or --client-gateway-socket")?;
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
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
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    if let Some(state_socket) = state_socket {
        publish_state_socket(socket, state_socket)?;
    }
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            // Running out of file descriptors, or a peer that hung up before it was accepted,
            // fails one accept. It must not end the daemon: back off and keep serving.
            Err(error) => {
                eprintln!("st3: accept a local API connection: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // Every local connection names its caller, so request counts by client are always on.
        let peer_pid = local_peer_pid(&stream);
        let app = app.clone();
        tokio::spawn(async move {
            // /proc ancestry may fault in pages on a loaded host. Keep that work
            // out of the accept loop so a slow lookup delays only this peer.
            let (bound_agent, caller, delivery_peer) = match peer_pid {
                Some(pid) => tokio::task::spawn_blocking(move || {
                    let bound_agent = bind_harness.then(|| ancestor(pid)).flatten();
                    let caller = Some(crate::profile::Caller::of_command(
                        local_process_arguments(pid).map(|(arguments, _)| arguments),
                        bound_agent.as_deref(),
                    ));
                    let delivery_peer = bind_harness.then(|| native_delivery_peer(pid)).flatten();
                    (bound_agent, caller, delivery_peer)
                })
                .await
                .unwrap_or_default(),
                None => (None, None, None),
            };
            let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                let app = app.clone();
                let bound_agent = bound_agent.clone();
                let caller = caller.clone();
                let delivery_peer = delivery_peer.clone();
                async move {
                    let mut request = request.map(Body::new);
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
mod gateway_listener_tests {
    #[tokio::test]
    async fn live_listener_cannot_be_unlinked_by_another_daemon() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("gateway.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let error = super::serve_unix(&socket, axum::Router::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("refusing to replace live Unix socket listener"),
            "{error}"
        );
        assert!(tokio::net::UnixStream::connect(&socket).await.is_ok());
        drop(listener);
    }
}

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
    if request.method() == axum::http::Method::GET {
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
        "isolation": isolation_name(st_runtime::isolation_mode()),
        "store_index": state.store.index().map_err(ApiError::internal)?,
        "security": "trusted-network-no-tls-no-acls",
        "features": {"owned_sets":1,"seat_rollout":1},
    })))
}

fn isolation_name(mode: st_runtime::Isolation) -> &'static str {
    match mode {
        st_runtime::Isolation::Scope => "scope",
        st_runtime::Isolation::Detached => "detached",
        st_runtime::Isolation::DegradedDetached => "degraded-detached",
    }
}

async fn doctor(State(state): State<AppState>) -> Result<Json<DoctorReport>, ApiError> {
    let environment = tokio::task::spawn_blocking(crate::environment::snapshot)
        .await
        .map_err(ApiError::internal)?;
    // Linking a crate takes seconds, so it runs while the other checks do.
    let build_tools = environment.as_ref().ok().cloned().map(|environment| {
        tokio::task::spawn_blocking(move || crate::environment::check_build_tools(&environment))
    });
    let pty_root = state.pty_root.clone();
    let priority = tokio::task::spawn_blocking(move || {
        let observations = st_runtime::PtyRuntime::new(pty_root)
            .snapshot()
            .unwrap_or_default();
        st_runtime::priority_report(&observations)
    });
    let token = crate::resource::github_token().await;
    let mut report = tokio::task::spawn_blocking(move || {
        // This node's claims are signed as their batches are sealed; seal and judge them so
        // the signature counts cover everything written so far.
        state.store.replication_snapshot().map_err(ApiError::internal)?;
        doctor_report(&state)
    })
        .await
        .map_err(ApiError::internal)??
        .0;
    if let Some(build_tools) = build_tools {
        report.checks.push(build_tools_check(
            &build_tools.await.map_err(ApiError::internal)?,
        ));
    }
    report.checks.push(match environment {
        Ok(environment) => DoctorCheck {
            name: "daemon-environment".into(),
            status: "pass".into(),
            message: format!(
                "account interactive login shell; refreshed on use every 60 seconds; PATH={}",
                environment.get("PATH").map(String::as_str).unwrap_or("")
            ),
        },
        Err(error) => DoctorCheck {
            name: "daemon-environment".into(),
            status: "fail".into(),
            message: error.to_string(),
        },
    });
    let (status, message) = priority.await.map_err(ApiError::internal)?;
    report.checks.push(DoctorCheck {
        name: "priority".into(),
        status: status.into(),
        message,
    });
    report.checks.push(DoctorCheck {
        name: "github-observer-auth".into(),
        status: if token.is_ok() { "pass" } else { "warn" }.into(),
        message: if token.is_ok() {
            "GitHub observers have a token; credential values are not displayed".into()
        } else {
            crate::resource::GITHUB_AUTH_REMEDY.into()
        },
    });
    // Request samples are live operational telemetry. Keep them on /v1/doctor,
    // outside the store-index-fenced operation projection built by doctor_report.
    let mut routes = request_latency_snapshot();
    routes.sort_by_key(|route| std::cmp::Reverse(route["p99_ms"].as_u64().unwrap_or_default()));
    for route in routes.into_iter().take(10) {
        report.checks.push(DoctorCheck {
            name: format!(
                "request-latency/{}",
                route["route"].as_str().unwrap_or("unknown")
            ),
            status: "pass".into(),
            message: format!(
                "{} requests; recent p50 {} ms, p99 {} ms, max {} ms",
                route["count"], route["p50_ms"], route["p99_ms"], route["max_ms"]
            ),
        });
    }
    report.checks.extend(github_usage_checks(
        &crate::resource::github_usage_report(),
        client_now_ms(),
    ));
    report.status = if report.checks.iter().any(|check| check.status == "fail") {
        "fail"
    } else if report.checks.iter().any(|check| check.status == "warn") {
        "warn"
    } else {
        "pass"
    }
    .into();
    Ok(Json(report))
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
            message: "no attention item has been open for more than a day".into(),
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
            "{} attention items have been open for more than a day; `st attention ls --as PERSON` shows how to close a person's item, and a fault closes at its source: {}",
            stale.len(),
            listed.join("; ")
        ),
    }
}

fn build_tools_check(tools: &crate::environment::BuildTools) -> DoctorCheck {
    use crate::environment::LinkResult;
    let mut problems = Vec::new();
    if !tools.missing.is_empty() {
        problems.push(format!(
            "missing from the login PATH: {}",
            tools.missing.join(", ")
        ));
    }
    match &tools.link {
        LinkResult::Linked => {}
        LinkResult::NotAttempted => {
            problems.push("no small crate was linked because cargo or rustc is missing".into());
        }
        LinkResult::Failed(error) => problems.push(format!("a small crate did not link: {error}")),
    }
    if problems.is_empty() {
        return DoctorCheck {
            name: "build-tools".into(),
            status: "pass".into(),
            message: format!(
                "{} are on the login PATH, and a small crate links",
                tools.found.join(", ")
            ),
        };
    }
    DoctorCheck {
        name: "build-tools".into(),
        status: "warn".into(),
        message: format!(
            "{}; install what is missing, or export its directory from the account's shell startup files",
            problems.join("; ")
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
    let mut checks = Vec::new();
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
    match state.store.operation_projection_drift() {
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
    checks.push(DoctorCheck {
        name: "runtime-ownership".into(),
        status: if duplicates.is_empty() {
            "pass"
        } else {
            "fail"
        }
        .into(),
        message: if duplicates.is_empty() {
            "each desired member has a unique runtime ID".into()
        } else {
            format!("duplicate runtime owners: {}", duplicates.join("; "))
        },
    });
    checks.push(claude_hooks_check(state, &desired)?);
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
        .filter_map(|subject| {
            subject
                .member
                .as_ref()
                .map(|member| member.runtime_id.clone())
        })
        .collect::<std::collections::BTreeSet<_>>();
    for subject in &desired {
        if terminal_owned.contains(&subject.subject) {
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
                            "the first sync with {} ended with a different graph, and a heal \
                             did not fix it: {}; ",
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
    checks.push(stale_attention_check(
        &doctor_attention_items(&state.store, client_now_ms()).map_err(ApiError::internal)?,
        client_now_ms(),
    ));
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

async fn operational_repair_plan(
    State(state): State<AppState>,
) -> Result<Json<OperationalRepairPlan>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.operational_repair_plan())
        .await
        .map(Json)
}

async fn apply_operational_repair(
    State(state): State<AppState>,
    Json(request): Json<OperationalRepairApplyRequest>,
) -> Result<Json<OperationalRepairResult>, ApiError> {
    let store = state.store.clone();
    let result = blocking_action(move || store.apply_operational_repair(&request.token)).await?;
    if result.applied != 0 {
        signal_changed(&state);
    }
    Ok(Json(result))
}

async fn replication_status(
    State(state): State<AppState>,
) -> Result<Json<ReplicationStatus>, ApiError> {
    let store = state.store.clone();
    let configured = state.fleet_id.is_some();
    let fleet = state.fleet_id.clone();
    let peers = replication_peer_names(&state);
    let state_dir = state.state_dir.clone();
    blocking_store(move || {
        let mut status = store.replication_status_sealed(configured, fleet.as_deref(), &peers)?;
        status.removed = crate::fleet::file::RemovalNotice::load(&state_dir);
        Ok(status)
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
struct ReplicationRecordsQuery {
    #[serde(default = "default_true")]
    unresolved: bool,
}

fn default_true() -> bool {
    true
}

async fn replication_records(
    State(state): State<AppState>,
    Query(query): Query<ReplicationRecordsQuery>,
) -> Result<Json<Vec<ReplicaRecordView>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.replica_records(query.unresolved))
        .await
        .map(Json)
}

async fn replication_record(
    State(state): State<AppState>,
    AxumPath(record): AxumPath<String>,
) -> Result<Json<ReplicaRecordView>, ApiError> {
    let record = if record.starts_with("record/") {
        record
    } else {
        format!("record/{record}")
    };
    let store = state.store.clone();
    let record_for_read = record.clone();
    blocking_store(move || store.replica_record(&record_for_read))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("replica record `{record}` does not exist")))
}

async fn checkpoint_plan(
    State(state): State<AppState>,
    Json(request): Json<crate::store::CheckpointPlanRequest>,
) -> Result<Json<crate::store::CheckpointPlanView>, ApiError> {
    let cut = match request.day.as_deref() {
        Some(day) => crate::store::checkpoint_cut(day).map_err(ApiError::bad)?,
        None => crate::store::newest_due_cut(client_now_ms()),
    };
    let store = state.store.clone();
    let scratch = state.state_dir.join("checkpoint");
    blocking_store(move || store.checkpoint_plan_view(cut, &scratch))
        .await
        .map(Json)
}

async fn checkpoint_status(
    State(state): State<AppState>,
) -> Result<Json<crate::store::CheckpointStatusView>, ApiError> {
    let store = state.store.clone();
    let peers = state.configured_peers.clone();
    blocking_store(move || store.checkpoint_status(client_now_ms(), &peers))
        .await
        .map(Json)
}

async fn checkpoint_excuse(
    State(state): State<AppState>,
    Json(request): Json<crate::store::CheckpointExcuseRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let claim = state
        .store
        .excuse_checkpoint_writer(&request)
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(claim))
}

async fn checkpoint_resume(
    State(state): State<AppState>,
    Json(request): Json<crate::store::CheckpointResumeRequest>,
) -> Result<Json<crate::store::CheckpointStatusView>, ApiError> {
    let store = state.store.clone();
    blocking_action(move || store.resume_checkpoints(&request.actor, &request.reason)).await?;
    signal_changed(&state);
    let store = state.store.clone();
    let peers = state.configured_peers.clone();
    blocking_store(move || store.checkpoint_status(client_now_ms(), &peers))
        .await
        .map(Json)
}

async fn repair_replication_record(
    State(state): State<AppState>,
    Json(request): Json<ReplicationRepairRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let claim = state
        .store
        .repair_replica_record(
            &request.record_ref,
            &request.replacement_claim_id,
            &request.reason,
            &request.actor,
            &request.idempotency_key,
        )
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(claim))
}

/// Spool one snapshot on a read worker; a slow downloader never holds the SQLite snapshot.
async fn backup_export(State(state): State<AppState>) -> Result<Response, ApiError> {
    let store = state.store.clone();
    let file = blocking_store(move || {
        let mut file = tempfile::tempfile()?;
        store.write_backup(&mut file)?;
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(0))?;
        Ok(file)
    })
    .await?;
    let file = tokio::fs::File::from_std(file);
    let stream = futures_util::stream::try_unfold(file, |mut file| async move {
        use tokio::io::AsyncReadExt;
        let mut bytes = vec![0_u8; 64 * 1024];
        let read = file.read(&mut bytes).await?;
        if read == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        bytes.truncate(read);
        Ok(Some((bytes, file)))
    });
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(stream),
    )
        .into_response())
}

async fn replication_export(
    State(state): State<AppState>,
    Json(request): Json<ReplicationExportRequest>,
) -> Result<Json<ReplicationExportResponse>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || {
        let exchange = if request.summary_only {
            store.export_replication_summary(&request.fleet_id)?
        } else {
            store.export_replication_exchange_answering(
                &request.fleet_id,
                &request.inventory,
                &request.signature_requests,
            )?
        };
        Ok(Json(ReplicationExportResponse {
            exchange,
            store_index: store.index()?,
        }))
    })
    .await
}

/// One page of a checkpoint's manifest, for the replication worker to answer a peer with.
async fn replication_checkpoint_manifest(
    State(state): State<AppState>,
    Json(request): Json<crate::store::CheckpointManifestRequest>,
) -> Result<Json<crate::store::CheckpointManifestPage>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.checkpoint_manifest_page(&request))
        .await
        .map(Json)
}

/// The newest stable checkpoint this node needs a peer's manifest for, if any.
async fn replication_checkpoint_need(
    State(state): State<AppState>,
) -> Result<Json<Option<crate::store::CheckpointManifestNeed>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.checkpoint_manifest_need())
        .await
        .map(Json)
}

/// Adopt a checkpoint from the manifest the replication worker fetched from a peer.
async fn replication_checkpoint_adopt(
    State(state): State<AppState>,
    Json(manifest): Json<crate::store::CheckpointManifest>,
) -> Result<Json<Vec<crate::store::CheckpointAction>>, ApiError> {
    let store = state.store.clone();
    let actions = blocking_action(move || store.adopt_checkpoint(&manifest)).await?;
    if !actions.is_empty() {
        signal_changed(&state);
    }
    Ok(Json(actions))
}

async fn replication_receive(
    State(state): State<AppState>,
    Json(request): Json<ReplicationReceiveRequest>,
) -> Result<Json<ReplicationReceiveResponse>, ApiError> {
    let store = state.store.clone();
    let (response, reconcile_changed) = blocking_action(move || {
        let before_index = store
            .index()
            .map_err(|error| St3Error::new("internal", error.to_string()))?;
        let projection_was_healthy = !store
            .replication_projection_needs_recovery()
            .map_err(|error| St3Error::new("internal", error.to_string()))?;
        if let Some(round_trip_ms) = request.round_trip_ms {
            store.record_replication_round_trip(Duration::from_millis(round_trip_ms));
        }
        // Only the worker's own requests carry a round trip, and only they heal.
        let receipt = store.receive_replication_exchange_asking(
            &request.peer,
            &request.fleet_id,
            &request.exchange,
            request.round_trip_ms.is_some(),
        )?;
        if store
            .observes_transport_to(&request.peer)
            .map_err(|error| St3Error::new("internal", error.to_string()))?
        {
            store
                .record_transport_observation(&request.peer, "up", None, None)
                .map_err(|error| St3Error::new("internal", error.to_string()))?;
        }
        let new_data = replication_receive_has_new_data(receipt.received + receipt.signatures);
        let (admission, repairs) = if new_data {
            let admission = store
                .validate_replication_backlog()
                .map_err(|error| St3Error::new("internal", error.to_string()))?;
            let repairs = store
                .apply_replication_repairs()
                .map_err(|error| St3Error::new("internal", error.to_string()))?;
            (admission, repairs)
        } else {
            (Default::default(), 0)
        };
        // A catching-up node defers projection; the first receive after the deferral window,
        // with or without new data, projects what it admitted meanwhile.
        let was_deferred = store.replication_projection_deferred();
        let projection = if new_data || was_deferred {
            store
                .project_replication_backlog_unless_catching_up()
                .map_err(|error| St3Error::new("internal", error.to_string()))?
        } else {
            Some(true)
        };
        let projected = projection.unwrap_or(false);
        let store_index = store
            .index()
            .map_err(|error| St3Error::new("internal", error.to_string()))?;
        let changed = store_index != before_index
            || (projected && (admission.changed || repairs != 0 || was_deferred));
        let quiet_only = projection_was_healthy
            && projected
            && repairs == 0
            && admission.unknown == 0
            && admission.invalid == 0
            && store
                .claims_since_only_quiet_notifications(before_index)
                .map_err(|error| St3Error::new("internal", error.to_string()))?;
        // A deferred projection left the graph as it was, so the reconciler has nothing new.
        Ok((
            ReplicationReceiveResponse {
                receipt,
                changed,
                store_index,
            },
            changed && !quiet_only && projection.is_some(),
        ))
    })
    .await?;
    if response.changed {
        if reconcile_changed {
            crate::performance::record_wake("replication receive", None);
            state.notify.notify_one();
        }
        state
            .event_notify
            .send_modify(|generation| *generation = generation.saturating_add(1));
    }
    Ok(Json(response))
}

/// Answer a peer's heal question. A swap or a replay can change the graph.
async fn replication_heal_answer(
    State(state): State<AppState>,
    Json(request): Json<ReplicationHealAnswerRequest>,
) -> Result<Json<ReplicationHealAnswer>, ApiError> {
    if state.fleet_id.as_deref() != Some(request.fleet_id.as_str()) {
        return Err(ApiError::bad(St3Error::new(
            "fleet-id-mismatch",
            "the peer belongs to another fleet",
        )));
    }
    let store = state.store.clone();
    let (answer, changed) = blocking_store(move || {
        let before = store.replication_status(false, None, &[])?.graph_digest;
        let answer = store.heal_answer(&request.peer, &request.query)?;
        let changed = store.replication_status(false, None, &[])?.graph_digest != before;
        Ok((answer, changed))
    })
    .await?;
    if changed {
        signal_changed(&state);
    }
    Ok(Json(answer))
}

/// Compare a peer's heal answer with this node's claims and say what to ask next.
async fn replication_heal_next(
    State(state): State<AppState>,
    Json(request): Json<ReplicationHealNextRequest>,
) -> Result<Json<ReplicationHealStep>, ApiError> {
    let store = state.store.clone();
    let (step, changed) = blocking_store(move || {
        let before = store.replication_status(false, None, &[])?.graph_digest;
        let step = store.heal_next(&request.peer, request.answer)?;
        let changed = store.replication_status(false, None, &[])?.graph_digest != before;
        Ok((step, changed))
    })
    .await?;
    if changed {
        signal_changed(&state);
    }
    Ok(Json(step))
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

async fn replication_peer_failure(
    State(state): State<AppState>,
    Json(request): Json<ReplicationPeerFailureRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone();
    let changed = blocking_store(move || {
        let before_index = store.index()?;
        let stale = store.record_peer_failure(&request.peer, &request.status, &request.error)?;
        if stale && store.observes_transport_to(&request.peer)? {
            store.record_transport_observation(
                &request.peer,
                &request.status,
                Some(&request.error),
                None,
            )?;
        }
        // A peer that fails mid-sync sends no more exchanges, so project what it delivered.
        let projected = store.replication_projection_deferred()
            && store.project_replication_backlog_unless_catching_up()? == Some(true);
        Ok(store.index()? != before_index || projected)
    })
    .await?;
    if changed {
        signal_changed(&state);
    }
    Ok(Json(json!({ "recorded": true, "changed": changed })))
}

#[derive(Deserialize, Serialize)]
pub struct FleetEndpointsRequest {
    pub mode: String,
    pub endpoints: Vec<Value>,
}

async fn fleet_publish_endpoints(
    State(state): State<AppState>,
    Json(request): Json<FleetEndpointsRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone();
    let written = blocking_store(move || {
        store.publish_fleet_endpoints(&request.mode, &request.endpoints, env!("CARGO_PKG_VERSION"))
    })
    .await?;
    if written {
        signal_changed(&state);
    }
    Ok(Json(json!({ "published": written })))
}

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

async fn fleet_status(State(state): State<AppState>) -> Result<Json<FleetStatus>, ApiError> {
    let peers = replication_peer_names(&state);
    let store = state.store.clone();
    let node = state.node.clone();
    let fleet_id = state.fleet_id.clone();
    let state_dir = state.state_dir.clone();
    blocking_store(move || {
        let replication =
            store.replication_status_sealed(fleet_id.is_some(), fleet_id.as_deref(), &peers)?;
        Ok(FleetStatus {
            node,
            fleet_id,
            member_key: store.member_public_key(),
            view: store.fleet_view_sealed()?,
            peers: replication.peers,
            invites: store.fleet_invites(false)?,
            removed: crate::fleet::file::RemovalNotice::load(&state_dir),
        })
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
struct FleetInviteListQuery {
    #[serde(default)]
    all: bool,
}

async fn fleet_invite_list(
    State(state): State<AppState>,
    Query(query): Query<FleetInviteListQuery>,
) -> Result<Json<Vec<crate::store::FleetInviteView>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.fleet_invites(query.all))
        .await
        .map(Json)
}

async fn fleet_invite_create(
    State(state): State<AppState>,
    Json(request): Json<FleetInviteRequest>,
) -> Result<Json<FleetInviteCreated>, ApiError> {
    concrete_person(&request.person)?;
    let fleet_id = state.fleet_id.clone().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "not-in-a-fleet",
            "this node is not in a fleet; run st fleet create first",
        ))
    })?;
    let store = state.store.clone();
    let node = state.node.clone();
    let created = blocking_action(move || {
        let via = request.via.clone().unwrap_or_else(|| "auto".into());
        let invitation = crate::fleet::join::invite(
            &store,
            &fleet_id,
            &node,
            &crate::fleet::join::InviteOptions {
                name: request.name.clone(),
                lifetime: std::time::Duration::from_secs(request.expires_seconds),
                via,
                person: request.person.clone(),
                migrate: request.migrate,
            },
        )?;
        Ok(FleetInviteCreated {
            invite: invitation.invite,
            code: invitation.code,
            expires_at_unix_ms: invitation.expires_at_unix_ms,
        })
    })
    .await?;
    signal_changed(&state);
    Ok(Json(created))
}

async fn fleet_invite_revoke(
    State(state): State<AppState>,
    Json(request): Json<FleetInviteRevokeRequest>,
) -> Result<Json<Value>, ApiError> {
    concrete_person(&request.person)?;
    let store = state.store.clone();
    let invite = request
        .invite
        .strip_prefix("fleet-invite/")
        .unwrap_or(&request.invite)
        .to_owned();
    blocking_store(move || {
        store.revoke_fleet_invite(&invite, &request.reason, Some(&request.person))
    })
    .await?;
    signal_changed(&state);
    Ok(Json(json!({ "revoked": true })))
}

async fn fleet_redeem(
    State(state): State<AppState>,
    Json(request): Json<crate::fleet::handshake::JoinRequest>,
) -> Result<Json<Value>, ApiError> {
    use crate::store::FleetRedemption;
    let store = state.store.clone();
    let state_dir = state.state_dir.clone();
    let fleet_id = state.fleet_id.clone();
    let (answer, changed) = blocking_store(move || {
        Ok(match store.redeem_fleet_invite(&request)? {
            FleetRedemption::Closed => (json!({ "status": "closed" }), false),
            FleetRedemption::Refused(reason) => {
                (json!({ "status": "refused", "reason": reason }), false)
            }
            FleetRedemption::Admitted {
                token,
                writer_floor,
                admitted_claim,
                first,
            } => {
                let fleet_id =
                    fleet_id.ok_or_else(|| anyhow::anyhow!("this node is not in a fleet"))?;
                let fabric_protocol = crate::config::FleetFile::load(&state_dir)?
                    .and_then(|file| file.fabric_protocol)
                    .unwrap_or_else(|| crate::fleet::transport::default_fabric_protocol(&fleet_id));
                (
                    json!({
                        "status": "admitted",
                        "token": hex::encode(token),
                        "writer_floor": writer_floor,
                        "admitted_claim": admitted_claim,
                        "anchor_key": store.fleet_anchor()?,
                        "fleet_id": fleet_id,
                        "fabric_protocol": fabric_protocol,
                    }),
                    first,
                )
            }
        })
    })
    .await?;
    if changed {
        signal_changed(&state);
    }
    Ok(Json(answer))
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

async fn fleet_remove(
    State(state): State<AppState>,
    Json(request): Json<FleetRemoveRequest>,
) -> Result<Json<crate::store::FleetRemoval>, ApiError> {
    concrete_person(&request.person)?;
    let store = state.store.clone();
    let removal = blocking_action(move || {
        store.remove_fleet_member(&request.name, &request.reason, &request.person)
    })
    .await?;
    signal_changed(&state);
    Ok(Json(removal))
}

async fn fleet_leave_begin(
    State(state): State<AppState>,
    Json(request): Json<FleetPersonRequest>,
) -> Result<Json<Value>, ApiError> {
    concrete_person(&request.person)?;
    let store = state.store.clone();
    blocking_store(move || store.set_fleet_leaving(true)).await?;
    Ok(Json(json!({ "leaving": true })))
}

/// The body is read and ignored, so the answer never races a client still sending it.
async fn fleet_leave_cancel(
    State(state): State<AppState>,
    _body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.set_fleet_leaving(false)).await?;
    Ok(Json(json!({ "leaving": false })))
}

async fn fleet_leave_claim(
    State(state): State<AppState>,
    Json(request): Json<FleetPersonRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    concrete_person(&request.person)?;
    let store = state.store.clone();
    let claim = blocking_action(move || store.leave_fleet(&request.person)).await?;
    signal_changed(&state);
    Ok(Json(claim))
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
                | "/v1/internal/replication/heal/answer"
                | "/v1/internal/replication/heal/next"
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

async fn fleet_membership_view(
    State(state): State<AppState>,
) -> Result<Json<crate::fleet::FleetView>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.fleet_view_sealed())
        .await
        .map(Json)
}

async fn replication_wake(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone();
    let (admission, repairs, projection_attempted, projected, was_deferred) =
        blocking_store(move || {
            let admission = store.validate_replication_backlog()?;
            let repairs = store.apply_replication_repairs()?;
            let was_deferred = store.replication_projection_deferred();
            let projection_attempted = admission.changed
                || repairs != 0
                || was_deferred
                || store.replication_projection_needs_recovery()?;
            let projected = if projection_attempted {
                store
                    .project_replication_backlog_unless_catching_up()?
                    .unwrap_or(false)
            } else {
                true
            };
            Ok((
                admission,
                repairs,
                projection_attempted,
                projected,
                was_deferred,
            ))
        })
        .await?;
    if projected && (admission.changed || repairs != 0 || was_deferred) {
        signal_changed(&state);
    }
    Ok(Json(json!({
        "admitted": admission.valid,
        "unknown": admission.unknown,
        "invalid": admission.invalid,
        "repairs": repairs,
        "projected": projected,
        "projection_attempted": projection_attempted,
    })))
}

async fn start_planning_session(
    State(state): State<AppState>,
    Json(request): Json<PlanningSessionStartRequest>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    let request_text = std::str::from_utf8(&request.request).map_err(|_| {
        ApiError::bad(St3Error::new(
            "launch-request-not-text",
            "a launch request must contain valid UTF-8",
        ))
    })?;
    if request_text.trim().is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "empty-launch-request",
            "a launch request cannot be empty",
        )));
    }
    let target_run = request
        .run
        .as_deref()
        .map(|run| state.store.mission_run(run))
        .transpose()
        .map_err(ApiError::internal)?
        .flatten();
    if request.run.is_some() && target_run.is_none() {
        return Err(ApiError::not_found("the target mission run does not exist"));
    }
    if let Some(run) = &target_run
        && (!matches!(run.status.as_str(), "running" | "blocked") || run.phase != "normal")
    {
        return Err(ApiError::bad(St3Error::new(
            "mission-run-not-revisable",
            format!(
                "mission run `{}` is {} in its {} phase",
                run.subject, run.status, run.phase
            ),
        )));
    }
    let mission_id = target_run
        .as_ref()
        .map(|run| {
            run.mission
                .strip_prefix("mission/")
                .unwrap_or(&run.mission)
                .to_owned()
        })
        .unwrap_or_else(|| request.mission.clone());
    crate::mission::validate_mission_id(&mission_id).map_err(ApiError::bad)?;
    let id = hex::encode(Sha256::digest(request.idempotency_key.as_bytes()))[..24].to_owned();
    let request_name = format!("doc/planning/{id}/request");
    let request_document = state
        .store
        .put_document(
            &request_name,
            &request.request,
            &None,
            &format!("{}:request", request.idempotency_key),
        )
        .map_err(ApiError::bad)?;
    let requester =
        normalize_planning_reviewer(request.requester.as_deref().unwrap_or("person/requester"));
    let provider = request
        .provider
        .as_deref()
        .unwrap_or(&state.planner_default.provider)
        .to_owned();
    if !matches!(
        provider.as_str(),
        "codex" | "claude" | "pi" | "omp" | "opencode"
    ) {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-planner",
            "the planner must use an eligible typed harness driver",
        )));
    }
    let inherit_default = provider == state.planner_default.provider;
    let planner_config = PlannerSpec {
        provider,
        model: request.model.clone().or_else(|| {
            inherit_default
                .then(|| state.planner_default.model.clone())
                .flatten()
        }),
        effort: request.effort.clone().or_else(|| {
            inherit_default
                .then(|| state.planner_default.effort.clone())
                .flatten()
        }),
    };
    if planner_config.provider == "opencode" && planner_config.effort.is_some() {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-planner-effort",
            "the OpenCode harness does not accept an effort override",
        )));
    }
    let planner_alias = format!("agent/planner.{}", &id[..10]);
    let request_reference = format!("{}@{}", request_document.name, request_document.hash);
    let context_reference = if let Some(run) = &target_run {
        let mission = state
            .store
            .mission_spec(&mission_id, Some(&run.revision))
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::internal("the target mission revision is unavailable"))?;
        let context = serde_json::to_vec_pretty(&json!({"mission_run": run, "mission": mission}))
            .map_err(ApiError::internal)?;
        let document = state
            .store
            .put_document(
                &format!("doc/planning/{id}/run-context"),
                &context,
                &None,
                &format!("{}:run-context", request.idempotency_key),
            )
            .map_err(ApiError::bad)?;
        Some(format!("{}@{}", document.name, document.hash))
    } else {
        None
    };
    let expected_subject = state
        .store
        .selected_desired_token(&planner_alias)
        .map_err(ApiError::internal)?
        .into_iter()
        .collect();
    // The planner seat starts idle, so what it is asked to do arrives as its launch request.
    let references = context_reference
        .as_ref()
        .map(|context| format!("{request_reference}\n{context}"))
        .unwrap_or_else(|| request_reference.clone());
    let request_text = format!(
        "You are the durable {} planner for launch {id}. Use `st documents get` for each immutable document reference below. Write one Markdown mission and one complete version 2 KDL mission. The KDL mission ID must be `{mission_id}` and its state must be ready. You can submit named variants with `st launch submit {id} --variant NAME --markdown FILE --kdl KDL_FILE`. Use temporary files outside the workspace, and remove them after submission. Do not change the workspace. Do not publish or run the mission. Stay available for revision messages until approval or cancellation.\n\n{references}",
        planner_config.provider
    );
    let arguments = match planner_config.provider.as_str() {
        "codex" => vec![
            "--dangerously-bypass-approvals-and-sandbox".into(),
            "--dangerously-bypass-hook-trust".into(),
        ],
        "claude" => vec!["--permission-mode".into(), "bypassPermissions".into()],
        _ => Vec::new(),
    };
    let planner = quick_agent(
        &state,
        QuickAgentRequest {
            subject: planner_alias,
            worktree: request.workspace.clone(),
            model: planner_config.model.clone(),
            effort: planner_config.effort.clone(),
            arguments,
            expected_subject,
            idempotency_key: format!("{}:planner", request.idempotency_key),
        },
        &planner_config.provider,
    )
    .await?;
    let session = state
        .store
        .create_planning_session(
            &id,
            &mission_id,
            &request_reference,
            &request.workspace,
            &requester,
            &planner.subject,
            &planner_config,
            target_run.as_ref().map(|run| run.subject.as_str()),
            target_run.as_ref().map(|run| run.generation.as_str()),
        )
        .map_err(ApiError::bad)?;
    send_planning_message(
        &state,
        &format!("planning-request:{id}"),
        &requester,
        &planner.subject,
        &request_text,
        "Launch request",
    )?;
    let mut started_fields = BTreeMap::from([
        (
            "mission".into(),
            Value::String(format!("mission/{}", session.mission)),
        ),
        ("request".into(), Value::String(request_reference)),
        ("planner".into(), Value::String(session.planner.clone())),
        (
            "planner_config".into(),
            serde_json::to_value(&planner_config).map_err(ApiError::internal)?,
        ),
        ("workspace".into(), Value::String(session.workspace.clone())),
        ("requester".into(), Value::String(session.requester.clone())),
    ]);
    if let Some(run) = &session.target_mission_run {
        started_fields.insert("target_run".into(), Value::String(run.clone()));
    }
    if let Some(generation) = &session.source_generation {
        started_fields.insert(
            "target_generation".into(),
            Value::String(generation.clone()),
        );
    }
    record_planning_event(
        &state,
        &session,
        "planning-session.started",
        Some(&requester),
        started_fields,
        &format!("{}:started", request.idempotency_key),
    )?;
    signal_changed(&state);
    Ok(Json(session))
}

async fn get_planning_session(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    let store = state.store.clone();
    let id_for_read = id.clone();
    blocking_store(move || store.planning_session(&id_for_read))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("launch `{id}` does not exist")))
}

async fn submit_planning_candidate(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<PlanningCandidateSubmitRequest>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    submit_planning_variant(state, id, "default".into(), request).await
}

async fn submit_named_planning_candidate(
    State(state): State<AppState>,
    AxumPath((id, variant)): AxumPath<(String, String)>,
    Json(request): Json<PlanningCandidateSubmitRequest>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    submit_planning_variant(state, id, variant, request).await
}

async fn submit_planning_variant(
    state: AppState,
    id: String,
    variant: String,
    request: PlanningCandidateSubmitRequest,
) -> Result<Json<PlanningSessionView>, ApiError> {
    validate_planning_variant(&variant)?;
    let session = required_planning_session(&state, &id)?;
    let markdown = std::str::from_utf8(&request.markdown).map_err(|_| {
        ApiError::bad(St3Error::new(
            "launch-markdown-not-text",
            "planning Markdown must contain valid UTF-8",
        ))
    })?;
    if markdown.trim().is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "empty-launch-markdown",
            "planning Markdown cannot be empty",
        )));
    }
    let kdl = std::str::from_utf8(&request.kdl).map_err(|_| {
        ApiError::bad(St3Error::new(
            "launch-kdl-not-text",
            "planning KDL must contain valid UTF-8",
        ))
    })?;
    let (intent, _) = mission_source(&state, kdl, None)?;
    let mission = launch_candidate_mission(&intent, &session.mission).map_err(ApiError::bad)?;
    if mission.state != crate::model::MissionState::Ready {
        return Err(ApiError::bad(St3Error::new(
            "launch-mission-not-ready",
            "a planning candidate must contain a ready mission",
        )));
    }
    let mut content_hasher = Sha256::new();
    content_hasher.update(b"st3.planning-candidate.v1\0");
    content_hasher.update((request.markdown.len() as u64).to_be_bytes());
    content_hasher.update(&request.markdown);
    content_hasher.update((request.kdl.len() as u64).to_be_bytes());
    content_hasher.update(&request.kdl);
    let content_hash = hex::encode(content_hasher.finalize());
    let markdown_name = format!(
        "doc/planning/{}/candidate/{content_hash}/markdown",
        session.id
    );
    let kdl_name = format!("doc/planning/{}/candidate/{content_hash}/kdl", session.id);
    let markdown_document = state
        .store
        .put_document(
            &markdown_name,
            &request.markdown,
            &None,
            &format!("planning-candidate:{}:{content_hash}:markdown", session.id),
        )
        .map_err(ApiError::bad)?;
    let kdl_document = state
        .store
        .put_document(
            &kdl_name,
            &request.kdl,
            &None,
            &format!("planning-candidate:{}:{content_hash}:kdl", session.id),
        )
        .map_err(ApiError::bad)?;
    let response = state
        .store
        .add_planning_candidate(
            &session.id,
            &request.actor,
            &variant,
            &format!("{}@{}", markdown_document.name, markdown_document.hash),
            &format!("{}@{}", kdl_document.name, kdl_document.hash),
            &mission.revision,
        )
        .map_err(ApiError::bad)?;
    let candidate = response
        .variants
        .iter()
        .find(|candidate| candidate.name == variant)
        .map(|variant| &variant.candidate)
        .expect("the submitted planning candidate is visible");
    record_planning_event(
        &state,
        &response,
        "planning-session.candidate-submitted",
        Some(&request.actor),
        BTreeMap::from([
            ("variant".into(), Value::String(variant.clone())),
            ("candidate_revision".into(), Value::from(candidate.revision)),
            ("markdown".into(), Value::String(candidate.markdown.clone())),
            ("kdl".into(), Value::String(candidate.kdl.clone())),
            (
                "mission_revision".into(),
                Value::String(candidate.mission_revision.clone()),
            ),
        ]),
        &format!(
            "planning-candidate:{}:{variant}:{}",
            response.id, candidate.revision
        ),
    )?;
    signal_changed(&state);
    preview_planning_variant(state, id, variant).await
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

async fn preview_planning_candidate(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    preview_planning_variant(state, id, "default".into()).await
}

async fn preview_named_planning_candidate(
    State(state): State<AppState>,
    AxumPath((id, variant)): AxumPath<(String, String)>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    preview_planning_variant(state, id, variant).await
}

async fn preview_planning_variant(
    state: AppState,
    id: String,
    variant: String,
) -> Result<Json<PlanningSessionView>, ApiError> {
    validate_planning_variant(&variant)?;
    let session = required_planning_session(&state, &id)?;
    if session.status != "review" {
        return Err(ApiError::bad(St3Error::new(
            "launch-not-reviewable",
            format!("launch `{}` is {}", session.id, session.status),
        )));
    }
    let candidate = session
        .variants
        .iter()
        .find(|candidate| candidate.name == variant)
        .map(|variant| &variant.candidate)
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-launch-candidate",
                "the launch has no candidate",
            ))
        })?;
    let kdl = planning_document_text(&state, &candidate.kdl)?;
    let (intent, mission_response) = mission_source(&state, &kdl, None)?;
    let mission = &intent.missions[&session.mission];
    let graph = render_planning_graph(mission);
    let diff = render_planning_diff(&mission_response);
    let mut normalized = serde_json::to_value(mission).map_err(ApiError::internal)?;
    client_safe_json(&mut normalized);
    let diagnostics = launch_diagnostics(&mission_response.blockers, &mission_response.warnings);
    let hash = launch_preview_token_values(
        &session.id,
        &variant,
        candidate.revision,
        session.source_generation.as_deref(),
        &normalized,
        &diagnostics,
    )
    .map_err(ApiError::internal)?;
    if session
        .variants
        .iter()
        .find(|candidate| candidate.name == variant)
        .and_then(|variant| variant.preview.as_ref())
        .is_some_and(|preview| {
            preview.candidate_revision == candidate.revision && preview.hash == hash
        })
    {
        return Ok(Json(session));
    }
    let response = state
        .store
        .save_planning_preview(
            &session.id,
            &variant,
            candidate.revision,
            &hash,
            &graph,
            &diff,
            &mission_response,
        )
        .map_err(ApiError::bad)?;
    let preview = response
        .variants
        .iter()
        .find(|candidate| candidate.name == variant)
        .and_then(|variant| variant.preview.as_ref())
        .expect("the saved launch preview is visible");
    record_planning_event(
        &state,
        &response,
        "planning-session.previewed",
        Some(&response.requester),
        BTreeMap::from([
            ("variant".into(), Value::String(variant.clone())),
            (
                "candidate_revision".into(),
                Value::from(preview.candidate_revision),
            ),
            ("preview_hash".into(), Value::String(preview.hash.clone())),
            ("store_index".into(), Value::from(preview.store_index)),
            ("graph".into(), Value::String(preview.graph.clone())),
            ("diff".into(), Value::String(preview.diff.clone())),
            (
                "mission".into(),
                serde_json::to_value(&preview.mission).map_err(ApiError::internal)?,
            ),
        ]),
        &format!("planning-preview:{}:{}", response.id, preview.hash),
    )?;
    signal_changed(&state);
    Ok(Json(response))
}

async fn compare_planning_variants(
    State(state): State<AppState>,
    AxumPath((id, left, right)): AxumPath<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    let session = required_planning_session(&state, &id)?;
    let variant = |name: &str| {
        session
            .variants
            .iter()
            .find(|variant| variant.name == name)
            .ok_or_else(|| ApiError::not_found(format!("launch variant `{name}` does not exist")))
    };
    let left = variant(&left)?;
    let right = variant(&right)?;
    Ok(Json(json!({
        "session": session.subject,
        "source_generation": session.source_generation,
        "left": left,
        "right": right,
    })))
}

async fn propose_planning_variant(
    State(state): State<AppState>,
    AxumPath((id, variant)): AxumPath<(String, String)>,
    Json(request): Json<PlanningProposalRequest>,
) -> Result<Json<RevisionSubmissionView>, ApiError> {
    if let Some(cached) = cached_revision_submission(
        &state,
        &format!("{}:cutover", request.idempotency_key),
        &format!("{}:proposal", request.idempotency_key),
    )? {
        return Ok(Json(cached));
    }
    let session = required_planning_session(&state, &id)?;
    let run_subject = session.target_mission_run.as_deref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "launch-has-no-run",
            "this launch creates a new mission and cannot propose a run revision",
        ))
    })?;
    let current = state
        .store
        .mission_run(run_subject)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("the target mission run does not exist"))?;
    if session.source_generation.as_deref() != Some(current.generation.as_str()) {
        return Err(ApiError::bad(St3Error::new(
            "stale-launch-generation",
            "the launch variants target a superseded generation",
        )));
    }
    let variant = session
        .variants
        .iter()
        .find(|candidate| candidate.name == variant)
        .ok_or_else(|| ApiError::not_found(format!("launch variant `{variant}` does not exist")))?;
    let preview = variant.preview.as_ref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "missing-launch-preview",
            "preview the named variant before proposal",
        ))
    })?;
    if !preview.mission.blockers.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "launch-preview-blocked",
            preview.mission.blockers.join("; "),
        )));
    }
    let intent =
        parse_intent(&preview.mission.resolved_intent.kdl, &state.node).map_err(ApiError::bad)?;
    let mission = &intent.missions[&session.mission];
    let old = state
        .store
        .mission_spec(&session.mission, Some(&current.revision))
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal("the current mission revision is unavailable"))?;
    let (_, reviewers) = crate::store::analyze_mission_revision(&old, mission, &current.requester)
        .map_err(ApiError::bad)?;
    state
        .store
        .apply(
            &intent,
            &preview.mission.subject_tokens,
            &format!("{}:publish", request.idempotency_key),
        )
        .map_err(ApiError::bad)?;
    let result =
        if reviewers.is_empty() && matches!(old.revision_cutover, RevisionCutover::RestartActive) {
            RevisionSubmissionView {
                status: "applied".into(),
                mission_run: state
                    .store
                    .adopt_mission_revision(
                        run_subject,
                        mission,
                        &request.actor,
                        &request.reason,
                        &format!("{}:cutover", request.idempotency_key),
                    )
                    .map_err(ApiError::bad)?,
                proposal: None,
            }
        } else {
            let proposal = state
                .store
                .create_revision_proposal(
                    run_subject,
                    mission,
                    &request.actor,
                    &request.reason,
                    &format!("{}:proposal", request.idempotency_key),
                )
                .map_err(ApiError::bad)?;
            RevisionSubmissionView {
                status: proposal.status.clone(),
                mission_run: state
                    .store
                    .mission_run(run_subject)
                    .map_err(ApiError::internal)?
                    .expect("the target mission run exists"),
                proposal: Some(proposal),
            }
        };
    signal_changed(&state);
    Ok(Json(result))
}

fn validate_planning_variant(variant: &str) -> Result<(), ApiError> {
    if variant.is_empty()
        || variant.len() > 80
        || !variant
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ApiError::bad(St3Error::new(
            "invalid-launch-variant",
            "a launch variant must use 1 through 80 letters, digits, dots, dashes, or underscores",
        )));
    }
    Ok(())
}

async fn revise_planning_session(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<PlanningRevisionRequest>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    let session = required_planning_session(&state, &id)?;
    authorize_planning_reviewer(&session, &request.actor)?;
    if request.feedback.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "empty-launch-feedback",
            "launch feedback cannot be empty",
        )));
    }
    std::str::from_utf8(&request.feedback).map_err(|_| {
        ApiError::bad(St3Error::new(
            "launch-feedback-not-text",
            "launch feedback must contain valid UTF-8",
        ))
    })?;
    let feedback_hash = hex::encode(Sha256::digest(&request.feedback));
    let name = format!("doc/planning/{}/feedback/{feedback_hash}", session.id);
    let document = state
        .store
        .put_document(
            &name,
            &request.feedback,
            &None,
            &format!("{}:feedback", request.idempotency_key),
        )
        .map_err(ApiError::bad)?;
    let response = state
        .store
        .request_planning_revision(&session.id, &request.actor)
        .map_err(ApiError::bad)?;
    record_planning_event(
        &state,
        &response,
        "planning-session.revision-requested",
        Some(&request.actor),
        BTreeMap::from([(
            "feedback".into(),
            Value::String(format!("{}@{}", document.name, document.hash)),
        )]),
        &format!("{}:revision-requested", request.idempotency_key),
    )?;
    send_planning_message(
        &state,
        &format!("planning-revision:{}:{feedback_hash}", session.id),
        &request.actor,
        &session.planner,
        &format!("{}@{}", document.name, document.hash),
        "Launch revision requested",
    )?;
    signal_changed(&state);
    Ok(Json(response))
}

async fn approve_planning_session(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<PlanningApprovalRequest>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    let session = required_planning_session(&state, &id)?;
    authorize_planning_reviewer(&session, &request.actor)?;
    if session.status != "review" && session.status != "approved" {
        return Err(ApiError::bad(St3Error::new(
            "launch-not-reviewable",
            format!("launch `{}` is {}", session.id, session.status),
        )));
    }
    let candidate = session.candidate.as_ref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "missing-launch-candidate",
            "the launch has no candidate",
        ))
    })?;
    let preview = session.preview.as_ref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "missing-launch-preview",
            "preview the candidate before approval",
        ))
    })?;
    let variant = session
        .variants
        .iter()
        .find(|variant| variant.name == candidate.variant)
        .ok_or_else(|| ApiError::internal("the selected launch variant is unavailable"))?;
    let projected =
        client_launch_variant_resources(&state, &session).map_err(ApiError::internal)?;
    let approval_token = projected
        .iter()
        .find(|resource| {
            resource["id"] == format!("launch-variant/{}/{}", session.id, variant.name)
        })
        .and_then(|resource| resource["preview_token"].as_str())
        .ok_or_else(|| ApiError::internal("the selected launch preview has no approval token"))?;
    if (preview.hash != request.preview_hash && approval_token != request.preview_hash)
        || preview.candidate_revision != candidate.revision
    {
        return Err(ApiError::bad(St3Error::new(
            "stale-launch-preview",
            "the approval does not name the current preview",
        )));
    }
    if session.status == "approved" {
        if session.published_revision.as_deref() == Some(candidate.mission_revision.as_str()) {
            let approval_key = format!("planning-approval:{}:{}", session.id, preview.hash);
            stop_planning_agent(&state, &session.planner, &approval_key)?;
            signal_changed(&state);
            return Ok(Json(session));
        }
        return Err(ApiError::internal(format!(
            "launch `{}` has inconsistent approved revision state",
            session.id
        )));
    }
    if !preview.mission.blockers.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "launch-preview-blocked",
            preview.mission.blockers.join("; "),
        )));
    }
    let target = if let Some(run_subject) = session.target_mission_run.as_deref() {
        let current = state
            .store
            .mission_run(run_subject)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found("the target mission run does not exist"))?;
        if session.source_generation.as_deref() != Some(current.generation.as_str()) {
            return Err(ApiError::bad(St3Error::new(
                "stale-launch-generation",
                "the launch approval targets a superseded generation",
            )));
        }
        Some((run_subject.to_owned(), current))
    } else {
        None
    };
    let intent =
        parse_intent(&preview.mission.resolved_intent.kdl, &state.node).map_err(ApiError::bad)?;
    let approval_key = format!("planning-approval:{}:{}", session.id, preview.hash);
    state
        .store
        .apply(
            &intent,
            &preview.mission.subject_tokens,
            &format!("{approval_key}:publish"),
        )
        .map_err(ApiError::bad)?;
    if let Some((run_subject, current)) = target {
        let mission = &intent.missions[&session.mission];
        let old = state
            .store
            .mission_spec(&session.mission, Some(&current.revision))
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::internal("the current mission revision is unavailable"))?;
        let (_, reviewers) =
            crate::store::analyze_mission_revision(&old, mission, &current.requester)
                .map_err(ApiError::bad)?;
        if reviewers.is_empty() && matches!(old.revision_cutover, RevisionCutover::RestartActive) {
            state
                .store
                .adopt_mission_revision(
                    &run_subject,
                    mission,
                    &request.actor,
                    "the requester approved the launch revision",
                    &format!("{approval_key}:cutover"),
                )
                .map_err(ApiError::bad)?;
        } else {
            let proposal = state
                .store
                .create_revision_proposal(
                    &run_subject,
                    mission,
                    &request.actor,
                    "the requester approved the launch revision",
                    &format!("{approval_key}:proposal"),
                )
                .map_err(ApiError::bad)?;
            if proposal.reviewers.contains(&request.actor) {
                let revision_preview = proposal
                    .preview_hash
                    .as_deref()
                    .ok_or_else(|| ApiError::internal("the revision proposal has no preview"))?;
                state
                    .store
                    .approve_revision_proposal(
                        &proposal.subject,
                        &request.actor,
                        revision_preview,
                        &format!("{approval_key}:revision-approval"),
                    )
                    .map_err(ApiError::bad)?;
            }
        }
    }
    let response = state
        .store
        .finish_planning_session(
            &session.id,
            &request.actor,
            "approved",
            Some(&candidate.mission_revision),
        )
        .map_err(ApiError::bad)?;
    record_planning_event(
        &state,
        &response,
        "planning-session.approved",
        Some(&request.actor),
        BTreeMap::from([
            ("variant".into(), Value::String(candidate.variant.clone())),
            ("candidate_revision".into(), Value::from(candidate.revision)),
            ("preview_hash".into(), Value::String(preview.hash.clone())),
            (
                "preview_token".into(),
                Value::String(approval_token.to_owned()),
            ),
            (
                "mission_revision".into(),
                Value::String(candidate.mission_revision.clone()),
            ),
            ("markdown".into(), Value::String(candidate.markdown.clone())),
            ("kdl".into(), Value::String(candidate.kdl.clone())),
            (
                "requester".into(),
                Value::String(response.requester.clone()),
            ),
        ]),
        &format!("{approval_key}:event"),
    )?;
    stop_planning_agent(&state, &session.planner, &approval_key)?;
    signal_changed(&state);
    Ok(Json(response))
}

async fn start_approved_launch(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<LaunchStartRequest>,
) -> Result<Json<MissionRunView>, ApiError> {
    let session = required_planning_session(&state, &id)?;
    authorize_planning_reviewer(&session, &request.actor)?;
    if session.status != "approved" {
        return Err(ApiError::bad(St3Error::new(
            "launch-not-approved",
            format!("launch `{}` is {}", session.id, session.status),
        )));
    }
    if session.target_mission_run.is_some() {
        return Err(ApiError::bad(St3Error::new(
            "launch-target-already-running",
            "a launch targeting an existing run is applied in place and cannot start another run",
        )));
    }
    let revision = session.published_revision.clone().ok_or_else(|| {
        ApiError::internal(format!(
            "approved launch `{}` has no published revision",
            session.id
        ))
    })?;
    let response = state
        .store
        .create_mission_run(&MissionRunRequest {
            mission: session.mission,
            revision: Some(revision),
            workspace: request.workspace,
            requester: Some(normalize_message_party(&request.actor)),
            mode: None,
            inputs: request.inputs,
            idempotency_key: format!("launch-start:{}:{}", session.id, request.idempotency_key),
        })
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(response))
}

async fn approve_and_start_launch(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<LaunchApproveAndStartRequest>,
) -> Result<Json<LaunchApproveAndStartView>, ApiError> {
    let Json(launch) = approve_planning_session(
        State(state.clone()),
        AxumPath(id.clone()),
        Json(PlanningApprovalRequest {
            actor: request.actor.clone(),
            preview_hash: request.preview_hash,
            idempotency_key: format!("{}:approve", request.idempotency_key),
        }),
    )
    .await?;
    // Approval is intentionally committed before start. If runtime creation fails, the durable
    // approved launch remains recoverable through this same idempotent request or `/start`.
    let Json(mission_run) = start_approved_launch(
        State(state),
        AxumPath(id),
        Json(LaunchStartRequest {
            actor: request.actor,
            workspace: request.workspace,
            inputs: request.inputs,
            idempotency_key: format!("{}:start", request.idempotency_key),
        }),
    )
    .await?;
    Ok(Json(LaunchApproveAndStartView {
        launch,
        mission_run,
    }))
}

async fn request_launch_decision(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<LaunchDecisionRequest>,
) -> Result<Json<Value>, ApiError> {
    let session = required_planning_session(&state, &id)?;
    if normalize_message_party(&request.actor) != session.planner {
        return Err(ApiError::bad(St3Error::new(
            "launch-decision-not-authorized",
            "only the launch planner can request a decision",
        )));
    }
    let question = request.question.trim();
    if question.is_empty() || question.len() > 2_000 {
        return Err(ApiError::bad(St3Error::new(
            "invalid-launch-question",
            "a launch question must contain 1 through 2000 bytes",
        )));
    }
    let options = request
        .options
        .iter()
        .map(|option| LaunchDecisionOption {
            id: option.id.trim().to_owned(),
            label: option.label.trim().to_owned(),
            description: option
                .description
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        })
        .collect::<Vec<_>>();
    let unique = options
        .iter()
        .map(|option| option.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let boolean = matches!(request.decision_type, LaunchDecisionType::Boolean);
    if (boolean && !options.is_empty())
        || (!boolean && options.is_empty())
        || options.len() > 20
        || options
            .iter()
            .any(|option| option.id.is_empty() || option.label.is_empty())
        || unique.len() != options.len()
    {
        return Err(ApiError::bad(St3Error::new(
            "invalid-launch-options",
            "boolean questions have no options; choice and rank questions need 1 through 20 options with distinct non-empty IDs and labels",
        )));
    }
    let digest = hex::encode(Sha256::digest(request.idempotency_key.as_bytes()));
    let decision_id = format!("launch-decision/{}", &digest[..32]);
    record_planning_event(
        &state,
        &session,
        "planning-session.question-requested",
        Some(&request.actor),
        BTreeMap::from([
            ("decision_id".into(), Value::String(decision_id.clone())),
            ("revision".into(), Value::from(1)),
            ("question".into(), Value::String(question.to_owned())),
            (
                "decision_type".into(),
                serde_json::to_value(&request.decision_type).map_err(ApiError::internal)?,
            ),
            (
                "options".into(),
                serde_json::to_value(&options).map_err(ApiError::internal)?,
            ),
            ("requester".into(), Value::String(session.requester.clone())),
            ("planner".into(), Value::String(session.planner.clone())),
        ]),
        &format!("launch-question:{}:{}", session.id, request.idempotency_key),
    )?;
    send_planning_message(
        &state,
        &format!("launch-question-message:{}:{decision_id}", session.id),
        &session.planner,
        &session.requester,
        &format!(
            "{decision_id}\n{question}\nDecision: {}",
            serde_json::to_string(&json!({"type": request.decision_type, "options": options}))
                .map_err(ApiError::internal)?
        ),
        "Launch decision requested",
    )?;
    signal_changed(&state);
    let value = client_launch_decision_resources(&state.store, &session)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|value| value["id"] == decision_id)
        .ok_or_else(|| ApiError::internal("the recorded launch decision is unavailable"))?;
    Ok(Json(value))
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

async fn answer_launch_decision(
    State(state): State<AppState>,
    AxumPath((id, decision)): AxumPath<(String, String)>,
    Json(request): Json<LaunchDecisionAnswerRequest>,
) -> Result<Json<Value>, ApiError> {
    let session = required_planning_session(&state, &id)?;
    authorize_planning_reviewer(&session, &request.actor)?;
    let decision_id = client_detail_id("launch-decision", &decision);
    let decisions =
        client_launch_decision_resources(&state.store, &session).map_err(ApiError::internal)?;
    let current = decisions
        .iter()
        .find(|value| value["id"] == decision_id)
        .ok_or_else(|| {
            ApiError::not_found(format!("launch decision `{decision_id}` does not exist"))
        })?;
    if request.expected_revision != 1 {
        return Err(ApiError::bad(St3Error::new(
            "stale-launch-decision",
            "the launch decision revision changed",
        )));
    }
    let response = serde_json::to_value(&request.response).map_err(ApiError::internal)?;
    let explanation = request
        .explanation
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    if current["state"] == "answered" {
        if current["response"] == response
            && current["explanation"]
                == serde_json::to_value(&explanation).map_err(ApiError::internal)?
        {
            return Ok(Json(current.clone()));
        }
        return Err(ApiError::bad(St3Error::new(
            "launch-decision-immutable",
            "a launch decision answer is immutable",
        )));
    }
    let decision_type: LaunchDecisionType =
        serde_json::from_value(current["decision_type"].clone())
            .map_err(|_| ApiError::internal("launch decision has an invalid type"))?;
    let options: Vec<LaunchDecisionOption> = serde_json::from_value(current["options"].clone())
        .map_err(|_| ApiError::internal("launch decision has invalid options"))?;
    let valid = valid_launch_decision_response(&decision_type, &options, &request.response);
    if !valid {
        return Err(ApiError::bad(St3Error::new(
            "invalid-launch-answer",
            "the structured response must match the decision type and contain unique option IDs from the offered options; rank responses must include every option exactly once",
        )));
    }
    let mut answer_fields = BTreeMap::from([
        ("decision_id".into(), Value::String(decision_id.clone())),
        (
            "expected_revision".into(),
            Value::from(request.expected_revision),
        ),
        ("response".into(), response.clone()),
        ("requester".into(), Value::String(session.requester.clone())),
    ]);
    if let Some(explanation) = &explanation {
        answer_fields.insert("explanation".into(), Value::String(explanation.clone()));
    }
    record_planning_event(
        &state,
        &session,
        "planning-session.question-answered",
        Some(&request.actor),
        answer_fields,
        &format!("launch-answer:{}:{}", session.id, request.idempotency_key),
    )?;
    send_planning_message(
        &state,
        &format!("launch-answer-message:{}:{decision_id}", session.id),
        &session.requester,
        &session.planner,
        &format!(
            "{decision_id}\nResponse: {}",
            serde_json::to_string(&response).map_err(ApiError::internal)?
        ),
        "Launch decision answered",
    )?;
    close_planning_message(
        &state,
        &format!("launch-question-message:{}:{decision_id}", session.id),
        &session.requester,
        &request.idempotency_key,
    )?;
    signal_changed(&state);
    let value = client_launch_decision_resources(&state.store, &session)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|value| value["id"] == decision_id)
        .ok_or_else(|| ApiError::internal("the answered launch decision is unavailable"))?;
    Ok(Json(value))
}

async fn cancel_planning_session(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<PlanningCancelRequest>,
) -> Result<Json<PlanningSessionView>, ApiError> {
    let session = required_planning_session(&state, &id)?;
    authorize_planning_reviewer(&session, &request.actor)?;
    if session.status == "cancelled" {
        stop_planning_agent(&state, &session.planner, &request.idempotency_key)?;
        signal_changed(&state);
        return Ok(Json(session));
    }
    let response = state
        .store
        .finish_planning_session(&session.id, &request.actor, "cancelled", None)
        .map_err(ApiError::bad)?;
    record_planning_event(
        &state,
        &response,
        "planning-session.cancelled",
        Some(&request.actor),
        BTreeMap::from([
            (
                "reason".into(),
                request
                    .reason
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            ),
            (
                "requester".into(),
                Value::String(response.requester.clone()),
            ),
        ]),
        &format!("{}:cancelled", request.idempotency_key),
    )?;
    stop_planning_agent(&state, &session.planner, &request.idempotency_key)?;
    signal_changed(&state);
    Ok(Json(response))
}

fn required_planning_session(state: &AppState, id: &str) -> Result<PlanningSessionView, ApiError> {
    // Legacy CLI launch IDs can themselves begin with `launch/`. Resolve the exact stored
    // ID before treating that prefix as the client resource namespace.
    client_launch_session(state, id)
}

fn normalize_planning_reviewer(value: &str) -> String {
    if value.contains('/') {
        value.to_owned()
    } else {
        format!("person/{value}")
    }
}

fn authorize_planning_reviewer(session: &PlanningSessionView, actor: &str) -> Result<(), ApiError> {
    if normalize_planning_reviewer(actor) == session.requester {
        return Ok(());
    }
    Err(ApiError::bad(St3Error::new(
        "launch-review-not-authorized",
        format!("`{actor}` cannot review launch `{}`", session.id),
    )))
}

fn record_planning_event(
    state: &AppState,
    session: &PlanningSessionView,
    kind: &str,
    actor: Option<&str>,
    fields: BTreeMap<String, Value>,
    idempotency_key: &str,
) -> Result<(), ApiError> {
    state
        .store
        .append_claim(&ClaimInput {
            subject: session.subject.clone(),
            kind: kind.into(),
            actor: actor.map(normalize_message_party),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(idempotency_key.into()),
        })
        .map_err(ApiError::bad)?;
    state
        .store
        .rebuild_claim_projections()
        .map_err(ApiError::internal)?;
    Ok(())
}

fn planning_document_text(state: &AppState, reference: &str) -> Result<String, ApiError> {
    let (name, hash) = reference.rsplit_once('@').ok_or_else(|| {
        ApiError::internal(format!(
            "planning document reference `{reference}` is invalid"
        ))
    })?;
    let bytes = state
        .store
        .get_document(name, hash)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal(format!("planning document `{reference}` is missing")))?;
    String::from_utf8(bytes).map_err(ApiError::internal)
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

fn render_planning_graph(mission: &crate::model::MissionSpec) -> String {
    fn append(mission: &crate::model::MissionSpec, indent: &str, lines: &mut Vec<String>) {
        for id in &mission.display_order {
            let step = &mission.steps[id];
            let dependencies = step
                .dependencies
                .iter()
                .filter_map(|dependency| match dependency {
                    crate::model::DependencySpec::Step { step, .. } => Some(step.as_str()),
                    crate::model::DependencySpec::Predicate { .. } => None,
                })
                .collect::<Vec<_>>();
            let suffix = if dependencies.is_empty() {
                "root".into()
            } else {
                format!("after {}", dependencies.join(", "))
            };
            let queue = step
                .queue
                .as_deref()
                .zip(step.queue_position)
                .map(|(queue, position)| format!(" · queue {queue} #{position}"))
                .unwrap_or_default();
            lines.push(format!("{indent}{} [{suffix}]{queue}", step.path));
            for product in &step.products {
                lines.push(format!("{indent}  produces {}", product.subject));
            }
            for gate in &step.gates {
                lines.push(format!("{indent}  gate {}", crate::graph::gate_name(gate)));
            }
            if let Some(nested) = &step.nested_mission {
                append(nested, &format!("{indent}  "), lines);
            }
        }
    }
    let mut lines = vec![format!("mission/{}", mission.id)];
    for baseline in &mission.baselines {
        lines.push(format!("  baseline {}", baseline.name));
    }
    for product in &mission.products {
        lines.push(format!("  produces {}", product.subject));
    }
    for gate in &mission.gates {
        lines.push(format!("  gate {}", crate::graph::gate_name(gate)));
    }
    append(mission, "  ", &mut lines);
    lines.join("\n")
}

fn render_planning_diff(mission: &MissionResponse) -> String {
    if mission.changes.is_empty() {
        return "No graph changes.".into();
    }
    mission
        .changes
        .iter()
        .map(|change| format!("{} {}", change.change, change.subject))
        .collect::<Vec<_>>()
        .join("\n")
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

fn close_planning_message(
    state: &AppState,
    message_key: &str,
    actor: &str,
    operation_key: &str,
) -> Result<(), ApiError> {
    let subject = format!(
        "message/{}",
        &hex::encode(Sha256::digest(message_key.as_bytes()))[..16]
    );
    for (kind, status) in [
        ("message.delivered", "delivered"),
        ("message.read", "read"),
        ("message.closed", "closed"),
    ] {
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.clone(),
                kind: kind.into(),
                actor: Some(normalize_message_party(actor)),
                fields: BTreeMap::from([("status".into(), Value::String(status.into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("{operation_key}:{status}")),
            })
            .map_err(ApiError::bad)?;
    }
    Ok(())
}

fn stop_planning_agent(state: &AppState, planner: &str, key: &str) -> Result<(), ApiError> {
    let local_agent = planner
        .strip_prefix("agent/")
        .and_then(|value| value.split_once('/').map(|(_, local)| local))
        .unwrap_or(planner);
    let standing_mission = format!("mission/standing/{local_agent}");
    let cancellation = state
        .store
        .active_mission_runs()
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|run| run.mission == standing_mission)
        .map(|run| run.subject)
        .map(|run| {
            format!(
                "mission-run {:?} {{ cancellation \"planning-session-ended\" {{ reason \"the launch ended\" }} }}\n",
                run
            )
        })
        .unwrap_or_default();
    let kdl = if cancellation.is_empty() {
        format!("version 2\nstop {planner:?}\n")
    } else {
        format!("version 2\n{cancellation}")
    };
    let intent = crate::graph::parse_internal_intent(&kdl, &state.node).map_err(ApiError::bad)?;
    state
        .store
        .apply_internal(&intent, &format!("{key}:stop-planner"))
        .map_err(ApiError::bad)?;
    Ok(())
}

async fn mission(
    State(state): State<AppState>,
    Json(request): Json<MissionRequest>,
) -> Result<Json<MissionResponse>, ApiError> {
    let initial = parse_intent(&request.intent.kdl, &state.node).map_err(ApiError::bad)?;
    let bindings = state
        .store
        .document_bindings_at(&initial.document_refs, request.at_index)
        .map_err(ApiError::internal)?;
    let resolved_kdl =
        resolve_document_references(&request.intent.kdl, &bindings).map_err(ApiError::bad)?;
    let intent = parse_intent(&resolved_kdl, &state.node).map_err(ApiError::bad)?;
    let resolved = crate::model::IntentInput {
        kdl: resolved_kdl,
        source_name: request.intent.source_name,
    };
    let mut response = state
        .store
        .mission_at(&intent, resolved, request.at_index)
        .map_err(ApiError::bad)?;
    publication_refusals(&state, &intent)
        .await?
        .block(&mut response);
    response
        .warnings
        .extend(ignored_authority_warnings(&intent, &state.node).map_err(ApiError::bad)?);
    Ok(Json(response))
}

/// Start running each exec gate of a mission file once, the way a run would: `st missions check`.
/// The answer lists every gate; poll `GET /v1/gate-checks/{id}` until it is finished.
async fn start_gate_check(
    State(state): State<AppState>,
    Json(request): Json<crate::model::GateCheckRequest>,
) -> Result<Json<crate::model::GateCheckView>, ApiError> {
    let initial = parse_intent(&request.intent.kdl, &state.node).map_err(ApiError::bad)?;
    // Check the commands a publication would store: document names pinned to their versions.
    let intent = state
        .store
        .document_bindings_at(&initial.document_refs, None)
        .ok()
        .and_then(|bindings| resolve_document_references(&request.intent.kdl, &bindings).ok())
        .and_then(|resolved| parse_intent(&resolved, &state.node).ok())
        .unwrap_or(initial);
    let workspace = std::path::Path::new(&request.workspace);
    if !workspace.is_absolute() {
        return Err(ApiError::bad(St3Error::new(
            "invalid-check-workspace",
            "a gate check's workspace must be an absolute path",
        )));
    }
    let (node, state_dir, pty_root) = (state.node, state.state_dir, state.pty_root);
    let view = tokio::task::spawn_blocking(move || {
        crate::gate_check::start(
            crate::gate_check::CheckHost {
                node: &node,
                state_dir: &state_dir,
                pty_root: &pty_root,
            },
            &intent,
            &request.workspace,
            &request.inputs,
        )
    })
    .await
    .map_err(|error| ApiError::internal(anyhow::anyhow!(error)))?
    .map_err(ApiError::internal)?;
    Ok(Json(view))
}

async fn read_gate_check(
    AxumPath(id): AxumPath<String>,
) -> Result<Json<crate::model::GateCheckView>, ApiError> {
    tokio::task::spawn_blocking(move || crate::gate_check::poll(&id))
        .await
        .map_err(|error| ApiError::internal(anyhow::anyhow!(error)))?
        .map(Json)
        .ok_or_else(|| {
            ApiError::not_found(
                "no such gate check: it started over an hour ago or the daemon restarted",
            )
        })
}

/// One preview warning for each agent the publication declares, directly or inside a mission,
/// that carries authority blocks, which free mode ignores.
fn ignored_authority_warnings(
    intent: &crate::model::NormalizedIntent,
    node: &str,
) -> Result<Vec<String>, St3Error> {
    let mut ignored = intent
        .subjects
        .values()
        .filter(|desired| desired.kind == "agent")
        .map(|desired| {
            (
                desired.subject.clone(),
                crate::graph::declared_authority_blocks(&desired.desired),
            )
        })
        .filter(|(_, blocks)| !blocks.is_empty())
        .collect::<BTreeMap<_, _>>();
    for id in crate::mission::top_level_mission_ids(&intent.missions) {
        if let Some(mission) = intent.missions.get(&id) {
            ignored.extend(crate::graph::mission_declared_authority_blocks(
                mission, node,
            )?);
        }
    }
    Ok(ignored
        .into_iter()
        .map(|(subject, blocks)| {
            format!(
                "free-mode: `{subject}` declares {}, which st ignores; within a fleet every agent may do what its person may do",
                blocks
                    .iter()
                    .map(|block| format!("`{block}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect())
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
    blocking_action(move || {
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
    .await
}

#[derive(Deserialize)]
struct AgentRolloutRequest {
    subject: String,
    actor: String,
    expected_desired: String,
    expected_incarnation: String,
    policy: crate::rollout::Policy,
    idempotency_key: String,
}
async fn rollout_agent(
    State(state): State<AppState>,
    Json(request): Json<AgentRolloutRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let actor = person_or_agent_actor(&request.actor, "invalid-rollout-actor")?;
    let subject = agent_subject(request.subject);
    let old =
        crate::rollout::launched_member(&state.store, &subject, &request.expected_incarnation)
            .map_err(ApiError::internal)?
            .map(|(_, member)| member)
            .or_else(|| {
                state
                    .store
                    .rollout(&subject)
                    .ok()
                    .flatten()
                    .filter(|o| o.old_incarnation == request.expected_incarnation)
                    .map(|o| o.old_member)
            })
            .ok_or_else(|| {
                ApiError::bad(St3Error::new(
                    "rollout-launch-unknown",
                    "the named incarnation has no original launch receipt",
                ))
            })?;
    let response = state
        .store
        .request_rollout(
            &subject,
            &request.expected_desired,
            &old,
            &request.expected_incarnation,
            &actor,
            &request.policy,
            &format!("seat-rollout-retry:{subject}:{}", request.idempotency_key),
        )
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(response))
}

#[derive(Deserialize)]
struct AgentRestartRequest {
    subject: String,
    actor: String,
    idempotency_key: String,
}

async fn restart_agent(
    State(state): State<AppState>,
    Json(request): Json<AgentRestartRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let actor = person_or_agent_actor(&request.actor, "invalid-restart-actor")?;
    let subject = if request.subject.starts_with("agent/") {
        request.subject
    } else {
        format!("agent/{}", request.subject)
    };
    state.store.owned_member_guard(&subject).map_err(ApiError::bad)?;
    let key = format!("agent-restart:{subject}:{}", request.idempotency_key);
    if let Some(prior) = state
        .store
        .operation_claim(&key)
        .map_err(ApiError::internal)?
    {
        return Ok(Json(prior));
    }
    if state
        .store
        .rollout(&subject)
        .map_err(ApiError::bad)?
        .is_some_and(|o| o.holds_seat())
    {
        return Err(ApiError::bad(St3Error::new(
            "rollout-in-progress",
            "the seat has a rollout; inspect it or retry with st agents rollout",
        )));
    }
    if crate::suspension::current(&state.store, &subject)
        .map_err(ApiError::internal)?
        .is_some_and(|suspension| suspension.holds_seat())
    {
        return Err(ApiError::bad(St3Error::new(
            "restart-suspended",
            "the seat is suspended; resume it with `st agents resume`",
        )));
    }
    let status = state
        .store
        .status(Some(&subject))
        .map_err(ApiError::internal)?;
    let current = status
        .subjects
        .iter()
        .find(|item| item.subject == subject)
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-agent",
                format!("no seat `{subject}`"),
            ))
        })?;
    if !current.conflicts.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "restart-conflict",
            "resolve the seat's conflicting declarations before restarting",
        )));
    }
    let desired = state
        .store
        .desired_subject_with_writer(&subject)
        .map_err(ApiError::internal)?
        .map(|(desired, _)| desired)
        .filter(|desired| desired.kind == "agent")
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "restart-not-declared",
                "restart needs an active seat declaration; start a stopped seat first",
            ))
        })?;
    let member = desired
        .member
        .as_ref()
        .filter(|member| member.lifecycle == crate::model::MemberLifecycle::Service)
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "restart-no-launch",
                "the seat has no readable service launch declaration",
            ))
        })?;
    let token = current.desired_token.clone().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "restart-not-declared",
            "the seat has no selected declaration",
        ))
    })?;
    let incarnation = current
        .actual
        .as_ref()
        .map(|actual| actual.get("fields").unwrap_or(actual))
        .and_then(|fields| fields.get("incarnation_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let claim = state
        .store
        .append_claim(&ClaimInput {
            subject,
            kind: "runtime.action.requested".into(),
            actor: Some(actor),
            fields: BTreeMap::from([
                ("action".into(), Value::String("restart".into())),
                (
                    "runtime_id".into(),
                    Value::String(member.runtime_id.clone()),
                ),
                ("incarnation_id".into(), Value::String(incarnation.into())),
            ]),
            evidence: vec![token],
            expected_subject: None,
            idempotency_key: Some(key),
        })
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(claim))
}

#[derive(Deserialize)]
struct MissionSeatStartRequest {
    subject: String,
    actor: String,
    /// The selected stop the caller read, when it read one.
    #[serde(default)]
    expected: Option<String>,
    idempotency_key: String,
}

/// Start a stopped mission seat on the declaration its run gave it, as `st agents start` does.
async fn start_mission_seat(
    State(state): State<AppState>,
    Json(request): Json<MissionSeatStartRequest>,
) -> Result<Json<ApplyResponse>, ApiError> {
    let actor = person_or_agent_actor(&request.actor, "invalid-start-actor")?;
    let subject = agent_subject(request.subject);
    let response = state
        .store
        .start_mission_seat(
            &subject,
            request.expected.as_deref(),
            &actor,
            &format!("agent-start:{subject}:{}", request.idempotency_key),
        )
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(response))
}

#[derive(Deserialize)]
pub(crate) struct AgentSuspensionRequest {
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
    let member = state
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

/// Ask the seat's owner to suspend it. The seat must be running and quiet now: its driver reports
/// the harness quiescent, it holds no claimed step and runs no subagent, and its driver has bound
/// a native session to resume. The owner checks again before it stops anything.
async fn suspend_agent(
    State(state): State<AppState>,
    Json(request): Json<AgentSuspensionRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    request_suspend(&state, request, None).map(Json)
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

/// Ask the seat's owner to resume a suspended seat on the native session it suspended.
async fn resume_agent(
    State(state): State<AppState>,
    Json(request): Json<AgentSuspensionRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    request_resume(&state, request, None).map(Json)
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
    let claim = state
        .store
        .append_claim(&ClaimInput {
            subject,
            kind: "runtime.action.requested".into(),
            actor: Some(actor),
            fields: BTreeMap::from([
                ("action".into(), Value::String("resume".into())),
                (
                    "runtime_id".into(),
                    Value::String(member.runtime_id.clone()),
                ),
            ]),
            evidence: vec![token, suspend],
            expected_subject: None,
            idempotency_key: Some(key),
        })
        .map_err(ApiError::bad)?;
    signal_changed(state);
    Ok(claim)
}

#[derive(Deserialize)]
struct NativeSessionReport {
    #[serde(default)]
    account_ref: Option<String>,
    subject: String,
    actor: String,
    incarnation_id: String,
    harness: String,
    session_id: String,
    #[serde(default)]
    path: Option<String>,
}

/// A seat's driver reports the native session its harness bound for one incarnation. Only the
/// seat itself reports its own session.
async fn report_native_session(
    State(state): State<AppState>,
    Json(request): Json<NativeSessionReport>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let subject = agent_subject(request.subject);
    if request.actor != subject {
        return Err(ApiError::bad(St3Error::new(
            "foreign-agent-actor",
            "a seat reports only its own native session",
        )));
    }
    if request.session_id.is_empty() || request.incarnation_id.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "invalid-native-session",
            "a native session report needs a session ID and an incarnation",
        )));
    }
    let mut fields = BTreeMap::from([
        ("harness".into(), Value::String(request.harness)),
        (
            "session_id".into(),
            Value::String(request.session_id.clone()),
        ),
        ("agent".into(), Value::String(subject.clone())),
        (
            "incarnation_id".into(),
            Value::String(request.incarnation_id.clone()),
        ),
        ("status".into(), Value::String("active".into())),
    ]);
    if let Some(path) = request.path {
        fields.insert("path".into(), Value::String(path));
    }
    if let Some(account) = request.account_ref.filter(|name| !name.is_empty()) {
        fields.insert("account_ref".into(), Value::String(account));
    }
    let claim = state
        .store
        .append_claim(&ClaimInput {
            subject: subject.clone(),
            kind: "harness.session-file".into(),
            actor: Some(subject.clone()),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!(
                "native-session:{subject}:{}:{}",
                request.incarnation_id, request.session_id
            )),
        })
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(claim))
}

#[derive(Deserialize)]
struct AgentRenameRequest {
    subject: String,
    name: Option<String>,
    actor: String,
    idempotency_key: String,
}

async fn rename_agent(
    State(state): State<AppState>,
    Json(request): Json<AgentRenameRequest>,
) -> Result<Json<ApplyResponse>, ApiError> {
    let subject = if request.subject.starts_with("agent/") {
        request.subject
    } else {
        format!("agent/{}", request.subject)
    };
    if !request.actor.starts_with("person/") {
        normalized_agent_actor(&request.actor).ok_or_else(|| {
            ApiError::bad(St3Error::new("invalid-rename-actor", "rename needs a person or agent actor"))
        })?;
    }
    let response = state.store.rename_agent(
        &subject,
        request.name.as_deref(),
        &request.idempotency_key,
    ).map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(response))
}

async fn apply(
    State(state): State<AppState>,
    Json(request): Json<ApplyRequest>,
) -> Result<Json<ApplyResponse>, ApiError> {
    let actor = request.actor.as_deref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "missing-publication-actor",
            "publication needs an explicit actor",
        ))
    })?;
    let intent = parse_intent(&request.intent.kdl, &state.node).map_err(ApiError::bad)?;
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

async fn get_mission(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<crate::model::MissionSpec>, ApiError> {
    let id = id.strip_prefix("mission/").unwrap_or(&id).to_owned();
    let store = state.store.clone();
    let id_for_read = id.clone();
    blocking_store(move || store.mission_spec(&id_for_read, None))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("mission `mission/{id}` does not exist")))
}

async fn put_document(
    State(state): State<AppState>,
    bound: Option<axum::Extension<BoundAgent>>,
    Json(request): Json<DocumentPutRequest>,
) -> Result<Json<DocumentVersion>, ApiError> {
    let response = state
        .store
        .put_document_as(
            &request.name,
            &request.bytes,
            &request.expected_document,
            &request.idempotency_key,
            bound.as_ref().map(|bound| bound.0.0.as_str()),
        )
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(response))
}

async fn list_rules(
    State(state): State<AppState>,
) -> Result<Json<Vec<smallclaims::rules::NamedRule>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.current_rules().map(|rules| rules.as_ref().clone()))
        .await
        .map(Json)
}

#[derive(Deserialize)]
struct RuleAuditQuery {
    rule: Option<String>,
    limit: Option<usize>,
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

async fn list_rule_audits(
    State(state): State<AppState>,
    Query(query): Query<RuleAuditQuery>,
) -> Result<Json<Vec<RuleAudit>>, ApiError> {
    let store = state.store.clone();
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    blocking_store(move || {
        Ok(store
            .rule_audits(query.rule.as_deref(), limit)?
            .into_iter()
            .map(|claim| {
                let field = |name: &str| {
                    claim.body["fields"][name]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                };
                RuleAudit {
                    rule: claim
                        .subject
                        .strip_prefix("rule/")
                        .unwrap_or(&claim.subject)
                        .to_owned(),
                    actor: field("actor"),
                    action: field("action"),
                    target: field("target"),
                    at_unix_ms: claim.accepted_at_unix_ms,
                    claim: claim.id,
                }
            })
            .collect())
    })
    .await
    .map(Json)
}

/// Set one rule as a person.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RuleSetRequest {
    pub actor: String,
    pub name: String,
    pub rule: smallclaims::rules::Rule,
}

async fn set_rule(
    State(state): State<AppState>,
    Json(request): Json<RuleSetRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    concrete_person(&request.actor)?;
    let claim = state
        .store
        .set_rule(&request.name, &request.rule, &request.actor)
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(claim))
}

#[derive(Deserialize)]
struct DocumentQuery {
    name: Option<String>,
    prefix: Option<String>,
    cursor: Option<String>,
    #[serde(default)]
    history: bool,
    limit: Option<usize>,
}

#[derive(Serialize, Deserialize)]
struct DocumentCursor {
    name: String,
    created_index: u64,
    exact_name: Option<String>,
    prefix: Option<String>,
    history: bool,
}

async fn list_documents(
    State(state): State<AppState>,
    Query(query): Query<DocumentQuery>,
) -> Result<Json<DocumentListResponse>, ApiError> {
    let limit = query.limit.unwrap_or(100).clamp(1, 200);
    let history = query.history;
    let cursor = query
        .cursor
        .as_deref()
        .map(|encoded| {
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded.strip_prefix("document/").unwrap_or(""))
                .map_err(|_| {
                    ApiError::bad(St3Error::new(
                        "validation-failed",
                        "invalid document cursor",
                    ))
                })?;
            let cursor: DocumentCursor = serde_json::from_slice(&bytes).map_err(|_| {
                ApiError::bad(St3Error::new(
                    "validation-failed",
                    "invalid document cursor",
                ))
            })?;
            if cursor.exact_name != query.name
                || cursor.prefix != query.prefix
                || cursor.history != history
            {
                return Err(ApiError::bad(St3Error::new(
                    "validation-failed",
                    "document cursor filters changed",
                )));
            }
            Ok(cursor)
        })
        .transpose()?;
    let store = state.store.clone();
    let name = query.name.clone();
    let prefix = query.prefix.clone();
    let mut items = blocking_store(move || {
        store.list_documents_page(
            name.as_deref(),
            prefix.as_deref(),
            history,
            cursor
                .as_ref()
                .map(|cursor| (cursor.name.as_str(), cursor.created_index)),
            limit.saturating_add(1),
        )
    })
    .await?;
    let has_more = items.len() > limit;
    items.truncate(limit);
    let next_cursor = if has_more {
        let last = items.last().expect("nonempty page with more items");
        let cursor = DocumentCursor {
            name: last.name.clone(),
            created_index: last.created_index,
            exact_name: query.name,
            prefix: query.prefix,
            history,
        };
        Some(format!(
            "document/{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&cursor).map_err(ApiError::internal)?)
        ))
    } else {
        None
    };
    Ok(Json(DocumentListResponse {
        items,
        has_more,
        limit,
        history,
        next_cursor,
    }))
}

#[derive(Deserialize)]
struct DocumentContentQuery {
    reference: String,
}

#[derive(Serialize)]
struct DocumentContent {
    reference: String,
    bytes: Vec<u8>,
}

async fn get_document(
    State(state): State<AppState>,
    Query(query): Query<DocumentContentQuery>,
) -> Result<Json<DocumentContent>, ApiError> {
    let (name, hash) = query.reference.rsplit_once('@').ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "invalid-document-reference",
            "a document reference needs `@HASH`",
        ))
    })?;
    let store = state.store.clone();
    let name = name.to_owned();
    let hash = hash.to_owned();
    let bytes = blocking_store(move || store.get_document(&name, &hash))
        .await?
        .ok_or_else(|| {
            ApiError::not_found(format!("document `{}` is not stored", query.reference))
        })?;
    Ok(Json(DocumentContent {
        reference: query.reference,
        bytes,
    }))
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
async fn post_claim(
    State(state): State<AppState>,
    Json(request): Json<ClaimInput>,
) -> Result<Json<ClaimRecord>, ApiError> {
    // A write can wait for the store's writer. It waits on the blocking pool, so the API's
    // workers keep answering other requests meanwhile.
    let store = state.store.clone();
    let kind = request.kind.clone();
    let (response, appended) =
        blocking_action(move || store.append_client_claim_outcome(&request)).await?;
    finish_claim_publication(&state, &kind, response, appended).await
}

// Both claim transports must publish response-usage rollups, even on replay after the original
// observation committed but a later step failed. Keep visibility wakes identical as well.
async fn finish_claim_publication(
    state: &AppState,
    kind: &str,
    response: ClaimRecord,
    appended: bool,
) -> Result<Json<ClaimRecord>, ApiError> {
    // Publish only the cumulative buckets. The response detail and turn ID remain local.
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
    if appended {
        if crate::store::local_observation_position(&response).is_some() {
            signal_local_change(state);
        } else if kind == "harness.usage" || kind == "subagent.renewed" {
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

#[derive(Deserialize)]
struct UsageQuery {
    since_ms: Option<u64>,
    until_ms: Option<u64>,
}

async fn get_usage(
    State(state): State<AppState>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<Value>, ApiError> {
    let until_ms = query.until_ms.unwrap_or(client_now_ms() as u64);
    let since_ms = query
        .since_ms
        .unwrap_or(until_ms.saturating_sub(86_400_000));
    if since_ms > until_ms {
        return Err(ApiError::bad(St3Error::new(
            "invalid-usage-period",
            "usage start must be before its end",
        )));
    }
    let store = state.store.clone();
    let (rows, limits) = blocking_store(move || {
        Ok((
            store.usage_period_rows(since_ms, until_ms)?,
            store.account_limits()?,
        ))
    })
    .await?;
    Ok(Json(
        json!({"since_ms": since_ms, "until_ms": until_ms, "rows": rows, "limits": limits}),
    ))
}

#[derive(Deserialize)]
struct HarnessDiagnosticRequest {
    actor: String,
    code: String,
    reason: String,
    severity: String,
    status: String,
    incarnation_id: Option<String>,
    idempotency_key: String,
}

async fn post_harness_diagnostic(
    State(state): State<AppState>,
    Json(request): Json<HarnessDiagnosticRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let actor = if request.actor.starts_with("agent/") {
        request.actor
    } else if !request.actor.contains('/') && !request.actor.trim().is_empty() {
        format!("agent/{}", request.actor)
    } else {
        return Err(ApiError::bad(St3Error::new(
            "invalid-diagnostic-actor",
            "a harness diagnostic requires one concrete agent subject",
        )));
    };
    if !matches!(request.severity.as_str(), "warning" | "error") {
        return Err(ApiError::bad(St3Error::new(
            "invalid-diagnostic-severity",
            "a harness diagnostic severity must be warning or error",
        )));
    }
    if request.code.trim().is_empty() || request.reason.trim().is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "invalid-harness-diagnostic",
            "a harness diagnostic requires a nonempty code and reason",
        )));
    }
    let mut fields = BTreeMap::from([
        ("code".into(), Value::String(request.code)),
        ("reason".into(), Value::String(request.reason)),
        ("severity".into(), Value::String(request.severity)),
        ("status".into(), Value::String(request.status)),
    ]);
    if let Some(incarnation) = request.incarnation_id {
        fields.insert("incarnation_id".into(), Value::String(incarnation));
    }
    let (record, appended) = state
        .store
        .append_claim_outcome(&ClaimInput {
            subject: actor.clone(),
            kind: "harness.diagnostic".into(),
            actor: Some(actor),
            fields,
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(request.idempotency_key),
        })
        .map_err(ApiError::bad)?;
    if appended {
        signal_changed(&state);
    }
    Ok(Json(record))
}

async fn get_claim(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let store = state.store.clone();
    let id_for_read = id.clone();
    blocking_store(move || store.claim_by_id(&id_for_read))
        .await?
        .filter(|claim| !claim.subject.starts_with("glass/"))
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("claim `{id}` does not exist")))
}

#[derive(Deserialize)]
struct ClaimsQuery {
    subject: Option<String>,
    owner_run: Option<String>,
    #[serde(default, alias = "after")]
    after_index: u64,
    #[serde(alias = "before")]
    before_index: Option<u64>,
    #[serde(default)]
    order: ClaimsOrder,
    #[serde(default = "default_claim_limit")]
    limit: usize,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ClaimsOrder {
    #[default]
    Asc,
    Desc,
}

fn default_claim_limit() -> usize {
    100
}

async fn list_claims(
    State(state): State<AppState>,
    Query(query): Query<ClaimsQuery>,
) -> Result<Json<ClaimsPage>, ApiError> {
    if query.limit == 0 || query.limit > 500 {
        return Err(ApiError::bad(St3Error::new(
            "invalid-page-limit",
            "a claim page limit must be between 1 and 500",
        )));
    }
    let store = state.store.clone();
    blocking_store(move || {
        store.claims_page(
            query.subject.as_deref(),
            query.owner_run.as_deref(),
            query.after_index,
            query.before_index,
            matches!(query.order, ClaimsOrder::Desc),
            query.limit,
        )
    })
    .await
    .map(Json)
}

#[derive(Default, Deserialize)]
struct ReviewsQuery {
    reviewer: Option<String>,
}

async fn list_reviews(
    State(state): State<AppState>,
    Query(query): Query<ReviewsQuery>,
) -> Result<Json<Vec<HumanReviewView>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.pending_human_reviews(query.reviewer.as_deref()))
        .await
        .map(Json)
}

#[derive(Default, Deserialize)]
struct AttentionQuery {
    person: Option<String>,
}

async fn ask_person(
    State(state): State<AppState>,
    Json(request): Json<PersonAskRequest>,
) -> Result<Json<StepRunView>, ApiError> {
    let result = state.store.ask_person(&request).map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(result))
}

async fn done_person_step(
    State(state): State<AppState>,
    Json(request): Json<PersonStepResponse>,
) -> Result<Json<StepRunView>, ApiError> {
    let result = state
        .store
        .finish_person_step(&request, false)
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(result))
}

async fn cancel_person_ask(
    State(state): State<AppState>,
    Json(request): Json<PersonStepResponse>,
) -> Result<Json<StepRunView>, ApiError> {
    let result = state
        .store
        .finish_person_step(&request, true)
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(result))
}

async fn list_attention(
    State(state): State<AppState>,
    Query(query): Query<AttentionQuery>,
) -> Result<Json<Vec<AttentionItemView>>, ApiError> {
    let person = query.person.map(|person| {
        if person.contains('/') {
            person
        } else {
            format!("person/{person}")
        }
    });
    if person
        .as_deref()
        .is_some_and(|person| !person.starts_with("person/"))
    {
        return Err(ApiError::bad(St3Error::new(
            "invalid-attention-person",
            "an attention filter must name a person subject",
        )));
    }
    let store = state.store.clone();
    blocking_store(move || store.attention_items(person.as_deref()))
        .await
        .map(Json)
}

async fn request_attention(
    State(_state): State<AppState>,
    Json(_post): Json<crate::model::AttentionRequestPost>,
) -> Result<Json<AttentionRequestView>, ApiError> {
    Err(ApiError::bad(St3Error::new(
        "attention-migrated",
        "attention is a derived view; use work ask/done/cancel-ask or act on its source",
    )))
}

async fn resolve_attention(
    State(_state): State<AppState>,
    AxumPath(_subject): AxumPath<String>,
    Json(_request): Json<AttentionResolveRequest>,
) -> Result<Json<AttentionRequestView>, ApiError> {
    Err(ApiError::bad(St3Error::new(
        "attention-migrated",
        "attention is a derived view; use work ask/done/cancel-ask or act on its source",
    )))
}

#[derive(Deserialize)]
struct SubscriptionRequestQuery {
    subscription: String,
}

async fn list_subscription_requests(
    State(state): State<AppState>,
    Query(query): Query<SubscriptionRequestQuery>,
) -> Result<Json<Vec<crate::model::SubscriptionRequestView>>, ApiError> {
    let subscription = if query.subscription.starts_with("subscription/") {
        query.subscription
    } else {
        format!("subscription/{}", query.subscription)
    };
    let store = state.store.clone();
    blocking_store(move || store.subscription_requests(&subscription))
        .await
        .map(Json)
}

/// Release a held subscription request or cancel an open one as a person.
async fn decide_subscription_request(
    State(state): State<AppState>,
    AxumPath((decision, request)): AxumPath<(String, String)>,
    Json(input): Json<crate::model::SubscriptionRequestDecision>,
) -> Result<Json<crate::model::SubscriptionRequestView>, ApiError> {
    let store = state.store.clone();
    let response =
        blocking_action(move || store.decide_subscription_request(&request, &decision, &input))
            .await?;
    signal_changed(&state);
    Ok(Json(response))
}

async fn withdraw_attention(
    State(_state): State<AppState>,
    AxumPath(_subject): AxumPath<String>,
    Json(_request): Json<AttentionWithdrawRequest>,
) -> Result<Json<AttentionRequestView>, ApiError> {
    Err(ApiError::bad(St3Error::new(
        "attention-migrated",
        "attention is a derived view; use work ask/done/cancel-ask or act on its source",
    )))
}

/// The owner a review decision answers. A step, mission or loop run, or a resource, answers for
/// itself. An `attention/...` card or a `gate-operation/...` request names its gate's owner, and
/// a bare `GENERATION/PATH` a step run. Anything else is refused as naming no review, rather
/// than read as a step run that has none.
fn review_owner(state: &AppState, target: &str) -> Result<String, ApiError> {
    if ["resource/", "step-run/", "mission-run/", "loop-run/"]
        .iter()
        .any(|prefix| target.starts_with(prefix))
    {
        return Ok(target.to_owned());
    }
    let unknown = |why: String| ApiError::bad(St3Error::new("review-target-unknown", why));
    if target.starts_with("attention/") {
        if let Some(card) = client_attention_resources(&state.store, None, false)
            .map_err(ApiError::internal)?
            .into_iter()
            .find(|card| card["id"] == target)
        {
            return match (card["attention_kind"].as_str(), card["source_id"].as_str()) {
                (Some("human-gate"), Some(owner)) => Ok(owner.to_owned()),
                (kind, _) => Err(unknown(format!(
                    "`{target}` is a {} card, not a review; answer it where it asks",
                    kind.unwrap_or("different")
                ))),
            };
        }
        // A gate's card closes once it is answered or asked again. Say which, for its owner.
        for (request, owner, reviewer) in state
            .store
            .human_gate_requests()
            .map_err(ApiError::internal)?
        {
            if client_attention_id(&owner, &reviewer, &request).map_err(ApiError::internal)?
                != target
            {
                continue;
            }
            let current = state
                .store
                .pending_human_reviews(Some(&reviewer))
                .map_err(ApiError::internal)?
                .into_iter()
                .find(|review| review.owner == owner);
            let why = match current {
                Some(review) => format!(
                    "st asked `{owner}` again as `{}`; review that card",
                    client_attention_id(&owner, &review.reviewer, &review.request)
                        .map_err(ApiError::internal)?
                ),
                None => format!(
                    "`{owner}` has no pending human review: {}",
                    state
                        .store
                        .human_review_refusal(&owner)
                        .map_err(ApiError::internal)?
                ),
            };
            return Err(ApiError::bad(St3Error::new(
                "review-not-requested",
                format!("`{target}` is not open: {why}"),
            )));
        }
        return Err(unknown(format!("`{target}` names no review card")));
    }
    if target.starts_with("gate-operation/") {
        return state
            .store
            .claims_for(target, Some("gate.requested"))
            .map_err(ApiError::internal)?
            .last()
            .and_then(|request| request.body.pointer("/fields/owner"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| unknown(format!("`{target}` names no gate request")));
    }
    let step = format!("step-run/{target}");
    if target.contains('/')
        && state
            .store
            .step_run(&step)
            .map_err(ApiError::internal)?
            .is_some()
    {
        return Ok(step);
    }
    Err(unknown(format!(
        "`{target}` names no step run, mission run, loop or attention card; \
         `st attention ls --as PERSON` lists the reviews waiting"
    )))
}

async fn post_review(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<ReviewRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    if !matches!(
        request.decision.as_str(),
        "approved" | "rejected" | "changes-requested"
    ) {
        return Err(ApiError::bad(St3Error::new(
            "invalid-review-decision",
            "a review decision must be approved, rejected, or changes-requested",
        )));
    }
    if matches!(request.decision.as_str(), "rejected" | "changes-requested")
        && request
            .reason
            .as_deref()
            .is_none_or(|reason| reason.trim().is_empty())
    {
        return Err(ApiError::bad(St3Error::new(
            "missing-review-reason",
            "a rejection or request for changes needs a reason",
        )));
    }
    let subject = review_owner(&state, &subject)?;
    let actor = request.actor.map(|actor| {
        if actor.contains('/') {
            actor
        } else {
            format!("person/{actor}")
        }
    });
    let review_request = if subject.starts_with("step-run/")
        || subject.starts_with("mission-run/")
        || subject.starts_with("loop-run/")
    {
        let pending = state
            .store
            .pending_human_reviews(None)
            .map_err(ApiError::internal)?
            .into_iter()
            .find(|review| review.owner == subject);
        let Some(pending) = pending else {
            let why = state
                .store
                .human_review_refusal(&subject)
                .map_err(ApiError::internal)?;
            return Err(ApiError::bad(St3Error::new(
                "review-not-requested",
                format!("`{subject}` has no pending human review: {why}"),
            )));
        };
        let review_request = state
            .store
            .claim_by_id(&pending.request)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::internal("the pending review request does not exist"))?;
        let reviewer = review_request
            .body
            .pointer("/fields/reviewer")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::internal("a human review request has no reviewer"))?;
        if actor.as_deref() != Some(reviewer) {
            return Err(ApiError::bad(St3Error::new(
                "wrong-reviewer",
                format!("the pending review requires `{reviewer}`"),
            )));
        }
        Some(review_request)
    } else {
        None
    };
    let mode = review_request
        .as_ref()
        .and_then(|claim| claim.body.pointer("/fields/mode"))
        .and_then(Value::as_str)
        .unwrap_or("approve");
    if mode == "feedback" && !subject.starts_with("step-run/") {
        return Err(ApiError::bad(St3Error::new(
            "feedback-gate-needs-step",
            "a feedback review must belong to a step",
        )));
    }
    if !match mode {
        "feedback" => matches!(request.decision.as_str(), "approved" | "changes-requested"),
        _ => matches!(request.decision.as_str(), "approved" | "rejected"),
    } {
        return Err(ApiError::bad(St3Error::new(
            "review-decision-not-offered",
            format!(
                "`{}` is not offered by this {mode} review",
                request.decision
            ),
        )));
    }
    let verdict = match request.decision.as_str() {
        "approved" => "pass",
        "changes-requested" => "feedback",
        _ => "fail",
    };
    let mut fields = BTreeMap::from([
        ("verdict".into(), Value::String(verdict.into())),
        ("decision".into(), Value::String(request.decision.clone())),
        (
            "reason".into(),
            request.reason.map(Value::String).unwrap_or(Value::Null),
        ),
    ]);
    if let Some(review_request) = &review_request {
        fields.insert("request".into(), Value::String(review_request.id.clone()));
    }
    let response = state
        .store
        .append_claim(&ClaimInput {
            subject: review_request
                .as_ref()
                .map(|request| request.subject.clone())
                .unwrap_or_else(|| subject.clone()),
            kind: "gate.result".into(),
            actor,
            fields,
            evidence: review_request
                .iter()
                .map(|request| request.id.clone())
                .collect(),
            expected_subject: request.expected_subject,
            idempotency_key: None,
        })
        .map_err(ApiError::bad)?;
    if request.decision == "rejected" {
        if let Some(owner) = review_request
            .as_ref()
            .and_then(|claim| claim.body.pointer("/fields/owner"))
            .and_then(Value::as_str)
        {
            if owner.starts_with("step-run/") {
                let step = state.store.step_run(owner).map_err(ApiError::internal)?;
                let claimant = if let Some(step) = step {
                    if step.claimant.is_some() || step.carried_claimant.is_some() {
                        step.claimant.or(step.carried_claimant)
                    } else {
                        state
                            .store
                            .claims_for(owner, Some("work.claimed"))
                            .map_err(ApiError::internal)?
                            .into_iter()
                            .rev()
                            .find(|claim| {
                                claim
                                    .body
                                    .pointer("/fields/attempt")
                                    .and_then(Value::as_u64)
                                    == Some(u64::from(step.attempt))
                            })
                            .and_then(|claim| {
                                claim
                                    .body
                                    .pointer("/fields/claimant")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                            })
                    }
                } else {
                    None
                };
                if let Some(claimant) = claimant {
                    let reason = response
                        .body
                        .pointer("/fields/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("No reason provided.");
                    send_planning_message(
                        &state,
                        &format!("review-rejected:{}:{claimant}", response.id),
                        response.actor.as_deref().unwrap_or("daemon/runtime"),
                        &claimant,
                        &format!("Your work on `{owner}` was rejected. Reason: {reason}"),
                        "Human review rejected",
                    )?;
                }
            }
        }
    }
    signal_changed(&state);
    Ok(Json(response))
}

async fn send_message(
    State(state): State<AppState>,
    Json(request): Json<MessageSendRequest>,
) -> Result<Json<MessageSendReceipt>, ApiError> {
    blocking_api(move || accept_message_receipt(&state, request, None, None).map(Json)).await
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
) -> Result<(), ApiError> {
    use smallclaims::principal::{FIELDS_FORMAT, Judged, KeyGrant};
    let mut signed = signature.signed_fields.clone();
    signed.sort();
    if signature.format.as_deref() != Some(FIELDS_FORMAT) || signed != SIGNED_MESSAGE_FIELDS {
        return Err(device_signature_error(
            "device-signature-format",
            format!(
                "a device signs a message in {FIELDS_FORMAT} over {}",
                SIGNED_MESSAGE_FIELDS.join(", ")
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
    if request.content.trim().is_empty() && request.attachments.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "empty-message",
            "a message needs nonempty content",
        )));
    }
    if request.content.len() > 4096 && !request.content.starts_with("doc/") {
        return Err(ApiError::bad(St3Error::new(
            "message-too-large",
            "an inline message cannot exceed 4 KiB; post a document first",
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
    let attachments = client_blobs::resolve_attachments(state, &from, &request.attachments)?;
    let id = hex::encode(Sha256::digest(request.idempotency_key.as_bytes()))[..16].to_owned();
    let subject = format!("message/{id}");
    let mut fields = BTreeMap::from([
        ("from".into(), Value::String(from.clone())),
        ("to".into(), Value::String(to.clone())),
        ("content".into(), Value::String(request.content.clone())),
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
            Value::Array(request.tags.iter().cloned().map(Value::String).collect()),
        ),
    ]);
    if let Some(session_id) = session_id {
        fields.insert("session_id".into(), Value::String(session_id));
    }
    if !attachments.is_empty() {
        fields.insert(
            "attachments".into(),
            serde_json::to_value(&attachments).map_err(ApiError::internal)?,
        );
    }
    if let Some(signature) = &device_signature {
        check_device_signature(state, signature, &request, &subject, &from, &fields)?;
    }
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
        Some(signature) => state.store.append_signed_claim(&input, signature),
        None => state.store.append_claim_outcome(&input),
    }
    .map_err(ApiError::bad)?;
    let mut work_wake = is_work_wake(&request.tags);
    if let Some(parent) = request.in_reply_to.as_deref() {
        // Settling the parent writes its lifecycle claims too.
        work_wake |= state
            .store
            .message(&message_subject(parent))
            .map_err(ApiError::internal)?
            .is_none_or(|message| is_work_wake(&message.tags));
        settle_answered_message(&state.store, parent, &from, &to, &subject, &record.id)?;
    }
    signal_message_changed(state, "message.sent", work_wake);
    Ok(MessageSendReceipt {
        message: MessageView {
            subject,
            from,
            to,
            content: request.content,
            status: "sent".into(),
            title: request.title,
            in_reply_to: request.in_reply_to,
            tags: request.tags,
            created_index: record.store_index,
            attachments,
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

#[derive(Deserialize)]
struct MessagesPageQuery {
    to: Option<String>,
    #[serde(default)]
    include_closed: bool,
    cursor: Option<String>,
    limit: Option<usize>,
    /// A seat delivery process's report; see [`delivery_presence`].
    delivery: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct MessagesPageCursor {
    after: u64,
    through: u64,
    to: Option<String>,
    include_closed: bool,
    limit: usize,
}

async fn list_messages_page(
    State(state): State<AppState>,
    Query(query): Query<MessagesPageQuery>,
    peer: Option<Extension<NativeDeliveryPeer>>,
) -> Result<Json<MessagePage>, ApiError> {
    let limit = query.limit.unwrap_or(100).clamp(1, 200);
    let to = query.to.as_deref().map(normalize_message_party);
    if let (Some(to), Some(report)) = (to.as_deref(), query.delivery.as_deref()) {
        delivery_presence::record(to, report);
    } else {
        record_legacy_poll(
            peer.as_ref().map(|Extension(peer)| peer),
            to.as_deref(),
            query.include_closed,
        );
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(|encoded| {
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded.strip_prefix("messages/").unwrap_or(""))
                .map_err(|_| {
                    ApiError::bad(St3Error::new("validation-failed", "invalid message cursor"))
                })?;
            let cursor: MessagesPageCursor = serde_json::from_slice(&bytes).map_err(|_| {
                ApiError::bad(St3Error::new("validation-failed", "invalid message cursor"))
            })?;
            if cursor.to != to
                || cursor.include_closed != query.include_closed
                || cursor.limit != limit
            {
                return Err(ApiError::bad(St3Error::new(
                    "validation-failed",
                    "message cursor filters changed",
                )));
            }
            Ok(cursor)
        })
        .transpose()?;
    let through = match &cursor {
        Some(cursor) => cursor.through,
        None => state.store.index().map_err(ApiError::internal)?,
    };
    let after = cursor.as_ref().map(|cursor| cursor.after);
    let store = state.store.clone();
    let (items, next_after) = blocking_store(move || {
        store.messages_page(to.as_deref(), query.include_closed, after, through, limit)
    })
    .await?;
    let next_cursor = next_after
        .map(|after| {
            let cursor = MessagesPageCursor {
                after,
                through,
                to: query.to.as_deref().map(normalize_message_party),
                include_closed: query.include_closed,
                limit,
            };
            serde_json::to_vec(&cursor).map(|bytes| {
                format!(
                    "messages/{}",
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
                )
            })
        })
        .transpose()
        .map_err(ApiError::internal)?;
    Ok(Json(MessagePage {
        items,
        has_more: next_cursor.is_some(),
        next_cursor,
        limit,
    }))
}

async fn list_messages(
    State(state): State<AppState>,
    Query(query): Query<MessagesQuery>,
    peer: Option<Extension<NativeDeliveryPeer>>,
) -> Result<Json<Vec<MessageView>>, ApiError> {
    let recipient = query.to.as_deref().map(normalize_message_party);
    record_legacy_poll(
        peer.as_ref().map(|Extension(peer)| peer),
        recipient.as_deref(),
        query.include_closed,
    );
    let store = state.store.clone();
    blocking_store(move || store.messages(recipient.as_deref(), query.include_closed))
        .await
        .map(Json)
}

async fn read_message(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
) -> Result<Json<MessageView>, ApiError> {
    let subject = if subject.starts_with("message/") {
        subject
    } else {
        format!("message/{subject}")
    };
    let store = state.store.clone();
    let lookup = subject.clone();
    blocking_store(move || store.message(&lookup))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("message `{subject}` does not exist")))
}

async fn post_message_claim(
    State(state): State<AppState>,
    AxumPath(message_id): AxumPath<String>,
    Json(request): Json<MessageLifecycleRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    let subject = message_subject(&message_id);
    let kind = match request.lifecycle.as_str() {
        "staged" => "message.staged",
        "delivered" => "message.delivered",
        "read" => "message.read",
        "closed" => "message.closed",
        other => {
            return Err(ApiError::bad(St3Error::new(
                "invalid-message-lifecycle",
                format!("message lifecycle `{other}` is not registered"),
            )));
        }
    };
    let store = state.store.clone();
    let record = blocking_api(move || {
        // One message, not every message the store has ever held: a lifecycle post read and folded
        // the whole mailbox, a quarter second on a busy host's store, while holding up the next write.
        let message = store
            .message(&subject)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("message `{subject}` does not exist")))?;
        let actor = request.actor.ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-message-actor",
                "message lifecycle transitions require the recipient actor",
            ))
        })?;
        let actor = normalize_message_party(&actor);
        if actor != message.to {
            return Err(ApiError::bad(St3Error::new(
                "wrong-message-recipient",
                format!(
                    "message `{subject}` belongs to `{}`, not `{actor}`",
                    message.to
                ),
            )));
        }
        let mut fields =
            BTreeMap::from([("status".into(), Value::String(request.lifecycle.clone()))]);
        if kind == "message.staged" {
            fields.insert("recipient".into(), Value::String(actor.clone()));
            if let Some(transport) = request.transport {
                fields.insert("transport".into(), Value::String(transport));
            }
            if let Some(runtime_id) = request.runtime_id {
                fields.insert("runtime_id".into(), Value::String(runtime_id));
            }
        }
        let (record, appended) = store
            .append_claim_outcome(&ClaimInput {
                subject,
                kind: kind.into(),
                actor: Some(actor),
                fields,
                evidence: request.evidence,
                expected_subject: request.expected_subject,
                idempotency_key: Some(request.idempotency_key),
            })
            .map_err(ApiError::bad)?;
        Ok((record, appended, is_work_wake(&message.tags)))
    })
    .await?;
    let (record, appended, work_wake) = record;
    // An idempotent repeat or an already-settled transition changes nothing a reader can see.
    // Signalling it anyway re-reads every subscribed seat's mailbox (#1085).
    if appended {
        signal_message_changed(&state, kind, work_wake);
    }
    Ok(Json(record))
}

fn message_subject(value: &str) -> String {
    if value.starts_with("message/") {
        value.to_owned()
    } else {
        format!("message/{value}")
    }
}

#[derive(Deserialize)]
struct StatusQuery {
    subject: Option<String>,
    owner_run: Option<String>,
    at_index: Option<u64>,
    #[serde(default)]
    history: bool,
}

/// One subject's desired record. Each Claude seat's status line reads it on every render (every
/// five seconds), which a full status reduction of the seat made a tenth of a core on a member.
async fn get_desired(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
) -> Result<Json<crate::model::DesiredSubject>, ApiError> {
    let store = state.store.clone();
    let named = subject.clone();
    blocking_store(move || store.desired_subjects_named(&[named]))
        .await?
        .into_iter()
        .next()
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("`{subject}` has no desired record")))
}

async fn status(
    State(state): State<AppState>,
    Query(query): Query<StatusQuery>,
) -> Result<Json<StatusResponse>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || {
        if query.history {
            store.status_history(
                query.subject.as_deref(),
                query.owner_run.as_deref(),
                query.at_index,
            )
        } else {
            store.status_at(
                query.subject.as_deref(),
                query.owner_run.as_deref(),
                query.at_index,
            )
        }
    })
    .await
    .map(Json)
}

#[derive(Default, Deserialize)]
struct SessionListQuery {
    #[serde(default)]
    history: bool,
}

async fn list_sessions(
    State(state): State<AppState>,
    Query(query): Query<SessionListQuery>,
) -> Result<Json<Vec<crate::model::SubjectStatus>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.terminal_statuses(query.history))
        .await
        .map(Json)
}

#[derive(Deserialize)]
struct EventQuery {
    #[serde(default, alias = "after_index")]
    after: u64,
    subject: Option<String>,
    owner_run: Option<String>,
    wait: Option<bool>,
    timeout_ms: Option<u64>,
}

async fn events(
    State(state): State<AppState>,
    Query(query): Query<EventQuery>,
) -> Result<Json<Vec<EventRecord>>, ApiError> {
    let read = |state: &AppState| {
        let store = state.store.clone();
        let subject = query.subject.clone();
        let owner_run = query.owner_run.clone();
        async move {
            blocking_store(move || {
                store.events_after_filtered(query.after, subject.as_deref(), owner_run.as_deref())
            })
            .await
        }
    };
    let current = read(&state).await?;
    if !current.is_empty() || query.wait == Some(false) {
        return Ok(Json(current));
    }
    let wait = async {
        let mut event_changed = state.event_notify.subscribe();
        loop {
            let current = read(&state).await?;
            if !current.is_empty() {
                return Ok(current);
            }
            event_changed.changed().await.map_err(ApiError::internal)?;
        }
    };
    let timeout_ms = query.timeout_ms.unwrap_or(30_000).clamp(10, 30_000);
    match tokio::time::timeout(Duration::from_millis(timeout_ms), wait).await {
        Ok(result) => result.map(Json),
        Err(_) => Ok(Json(Vec::new())),
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
    if selected_token != request.expected_subject {
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
    state
        .store
        .apply(&intent, &planned.subject_tokens, &request.idempotency_key)
        .map_err(ApiError::bad)?;
    signal_changed(state);
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
        event_cursor: state.store.index().map_err(ApiError::internal)?,
        incarnation_id: harness.map(|harness| harness.incarnation_id),
        ready,
    };
    state
        .store
        .cache_idempotency_response(&response_key, &response)
        .map_err(ApiError::internal)?;
    Ok(response)
}

async fn start_eval(
    State(state): State<AppState>,
    Json(request): Json<EvalStartRequest>,
) -> Result<Json<EvalStartResponse>, ApiError> {
    let actual_hash = hex::encode(Sha256::digest(&request.bundle));
    if actual_hash != request.bundle_hash {
        return Err(ApiError::bad(St3Error::new(
            "eval-bundle-hash-mismatch",
            "the eval bundle bytes do not match the supplied hash",
        )));
    }
    state
        .store
        .put_blob(&request.bundle)
        .map_err(ApiError::internal)?;
    let nonce = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let workspace = state
        .state_dir
        .join("evals")
        .join(&request.bundle_hash[..16])
        .join(nonce.to_string());
    hydrate_eval(&request.bundle, &workspace).map_err(ApiError::internal)?;
    let eval_file = workspace.join("eval.kdl");
    let kdl = fs::read_to_string(&eval_file)
        .map_err(|error| ApiError::internal(format!("read {}: {error}", eval_file.display())))?;
    let kdl = kdl.replace("${EVAL_ROOT}", &workspace.to_string_lossy());
    let mut intent = parse_intent(&kdl, &state.node).map_err(ApiError::bad)?;
    let ready = intent
        .missions
        .values()
        .filter(|mission| mission.state == crate::model::MissionState::Ready)
        .collect::<Vec<_>>();
    let named_entry = format!("eval/{}", request.name);
    let entry = ready
        .iter()
        .copied()
        .find(|mission| mission.id == named_entry)
        .or_else(|| (ready.len() == 1).then_some(ready[0]))
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "invalid-eval-mission-count",
                format!(
                    "an eval with helper missions needs one ready entry mission named `{named_entry}`"
                ),
            ))
        })?;
    let timeout_ms = entry.timeout_ms.ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "missing-eval-timeout",
            format!("eval entry mission `{}` needs a timeout", entry.id),
        ))
    })?;
    if timeout_ms > MAX_EVAL_TIMEOUT_MS {
        return Err(ApiError::bad(St3Error::new(
            "eval-timeout-too-large",
            format!(
                "eval entry mission `{}` timeout exceeds the 20 minute limit",
                entry.id
            ),
        )));
    }
    let entry_id = entry.id.clone();
    let entry_revision = entry.revision.clone();
    let idempotency_key = format!("eval:{}:{}:{nonce}", request.name, request.bundle_hash);
    let owner_run = state
        .store
        .mission_run_subject_for_idempotency_key(&idempotency_key);
    scope_eval_desired_subjects(
        &mut intent,
        &state.store.desired_subjects().map_err(ApiError::internal)?,
        &owner_run,
    )
    .map_err(ApiError::bad)?;
    stage_eval_documents(&state, &workspace, &intent).map_err(ApiError::bad)?;
    let mission = state
        .store
        .mission(
            &intent,
            crate::model::IntentInput {
                kdl: kdl.clone(),
                source_name: Some(eval_file.display().to_string()),
            },
        )
        .map_err(ApiError::bad)?;
    if !mission.blockers.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "eval-blocked",
            mission.blockers.join("; "),
        )));
    }
    let applied = state
        .store
        .apply(
            &intent,
            &mission.subject_tokens,
            &format!(
                "eval:{}:{}:{nonce}:apply",
                request.name, request.bundle_hash
            ),
        )
        .map_err(ApiError::bad)?;
    let run = match state.store.create_mission_run(&MissionRunRequest {
        mission: entry_id,
        revision: Some(entry_revision),
        workspace: workspace.to_string_lossy().into_owned(),
        requester: Some("person/eval-requester".into()),
        mode: Some("eval".into()),
        inputs: request.inputs,
        idempotency_key,
    }) {
        Ok(run) => run,
        Err(error) => {
            state
                .store
                .discard_desired_owned_by(&owner_run)
                .map_err(ApiError::internal)?;
            return Err(ApiError::bad(error));
        }
    };
    signal_changed(&state);
    Ok(Json(EvalStartResponse {
        event_cursor: applied
            .store_index
            .max(state.store.index().map_err(ApiError::internal)?),
        mission_run: run.subject,
    }))
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

async fn get_eval(
    State(state): State<AppState>,
    AxumPath(run): AxumPath<String>,
) -> Result<Json<EvalStatus>, ApiError> {
    let store = state.store.clone();
    let run_for_read = run.clone();
    let run = blocking_store(move || store.mission_run(&run_for_read))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("eval mission run `{run}` does not exist")))?;
    let store = state.store.clone();
    let run_subject = run.subject.clone();
    let verdict_claim =
        blocking_store(move || store.latest_claim(&run_subject, Some("eval.verdict"))).await?;
    let verdict = verdict_claim
        .as_ref()
        .and_then(|claim| claim.body.pointer("/fields/verdict"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let cleanup = if run.phase == "terminal" && verdict.is_some() {
        "complete"
    } else if run.phase == "final"
        || run.phase == "final-cancelled"
        || run.phase.starts_with("cleanup-")
    {
        "cleaning"
    } else {
        "pending"
    };
    let active_steps = run
        .steps
        .iter()
        .filter(|step| {
            matches!(
                step.status.as_str(),
                "ready" | "claimed" | "working" | "verifying" | "blocked"
            )
        })
        .map(|step| step.step.clone())
        .collect();
    Ok(Json(EvalStatus {
        mission_run: run.subject,
        lifecycle: run.status,
        phase: run.phase,
        active_steps,
        verdict,
        cleanup: cleanup.into(),
        store_index: state.store.index().map_err(ApiError::internal)?,
    }))
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

#[derive(Deserialize)]
struct MissionRunQuery {
    root: Option<String>,
    mission: Option<String>,
}

#[derive(Deserialize)]
struct MissionOverviewQuery {
    mission: String,
}
async fn mission_overview(
    State(state): State<AppState>,
    Query(query): Query<MissionOverviewQuery>,
) -> Result<Json<Value>, ApiError> {
    blocking_store(move || {
        state
            .store
            .read_snapshot(|_| state.store.mission_overview(&query.mission, 10))
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
struct OutcomeHistoryQuery {
    collection: String,
    #[serde(default)]
    since: u64,
    until: Option<u64>,
    status: Option<String>,
    actor: Option<String>,
    before: Option<u64>,
    limit: Option<usize>,
}
async fn outcome_history(
    State(state): State<AppState>,
    Query(query): Query<OutcomeHistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    if !matches!(query.collection.as_str(), "missions" | "work")
        || query
            .status
            .as_deref()
            .is_some_and(|s| !matches!(s, "failed" | "cancelled" | "timed-out" | "completed"))
        || query.limit.is_some_and(|limit| !(1..=200).contains(&limit))
        || query.until.is_some_and(|until| until < query.since)
        || query.since > i64::MAX as u64
        || query.until.is_some_and(|until| until > i64::MAX as u64)
    {
        return Err(ApiError::bad(St3Error::new(
            "invalid-history-filter",
            "select missions or work, a terminal status, a valid time window, and a limit of 1 through 200",
        )));
    }
    blocking_store(move || {
        state.store.read_snapshot(|index| {
            state.store.outcome_history(
                &query.collection,
                u128::from(query.since),
                query.until.map(u128::from).unwrap_or_else(client_now_ms),
                query.status.as_deref(),
                query.actor.as_deref(),
                query.before.unwrap_or(index.saturating_add(1)),
                query.limit.unwrap_or(50),
            )
        })
    })
    .await
    .map(Json)
}
async fn performance_report() -> Json<Value> {
    Json(crate::performance::snapshot())
}

async fn list_mission_runs(
    State(state): State<AppState>,
    Query(query): Query<MissionRunQuery>,
) -> Result<Json<Vec<MissionRunView>>, ApiError> {
    let store = state.store.clone();
    match (query.root, query.mission) {
        (Some(root), None) => blocking_store(move || {
            let mut runs = store.mission_runs_for_root(&root)?;
            annotate_stuck_gates(&store, &mut runs)?;
            Ok(runs)
        })
        .await
        .map(Json),
        (None, Some(mission)) => {
            blocking_store(move || store.active_mission_runs_for_mission(&mission))
                .await
                .map(Json)
        }
        _ => Err(ApiError::bad(St3Error::new(
            "invalid-mission-run-query",
            "select exactly one mission or root mission run",
        ))),
    }
}

fn annotate_stuck_gates(store: &Store, runs: &mut [MissionRunView]) -> anyhow::Result<()> {
    let active = runs
        .iter()
        .filter(|run| matches!(run.status.as_str(), "running" | "standing" | "blocked"))
        .map(|run| run.subject.clone())
        .collect::<Vec<_>>();
    let missions = store.mission_specs_for_runs(&active)?;
    for run in runs {
        if let Some(mission) = missions.get(&run.subject) {
            run.stuck_gates = crate::reconcile::stuck_field_gates(store, run, mission)?;
        }
    }
    Ok(())
}

async fn get_mission_run(
    State(state): State<AppState>,
    AxumPath(run): AxumPath<String>,
) -> Result<Json<MissionRunView>, ApiError> {
    let store = state.store.clone();
    let run_for_read = run.clone();
    blocking_store(move || {
        let Some(mut run) = store.mission_run(&run_for_read)? else {
            return Ok(None);
        };
        annotate_stuck_gates(&store, std::slice::from_mut(&mut run))?;
        Ok(Some(run))
    })
    .await?
    .map(Json)
    .ok_or_else(|| ApiError::not_found(format!("mission run `{run}` does not exist")))
}

async fn revise_mission_run(
    State(state): State<AppState>,
    AxumPath(run): AxumPath<String>,
    Json(request): Json<MissionRevisionRequest>,
) -> Result<Json<RevisionSubmissionView>, ApiError> {
    if let Some(cached) = cached_revision_submission(
        &state,
        &format!("{}:adopt", request.idempotency_key),
        &format!("{}:propose", request.idempotency_key),
    )? {
        return Ok(Json(cached));
    }
    let current = state
        .store
        .mission_run(&run)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("mission run `{run}` does not exist")))?;
    let initial = parse_intent(&request.intent.kdl, &state.node).map_err(ApiError::bad)?;
    if crate::mission::top_level_mission_ids(&initial.missions).len() != 1 {
        return Err(ApiError::bad(St3Error::new(
            "invalid-mission-revision-intent",
            "a run revision must contain exactly one top-level mission",
        )));
    }
    if !initial.subjects.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "invalid-mission-revision-intent",
            "a run revision can contain only its mission",
        )));
    }
    let bindings = state
        .store
        .document_bindings_at(&initial.document_refs, None)
        .map_err(ApiError::internal)?;
    let resolved_kdl =
        resolve_document_references(&request.intent.kdl, &bindings).map_err(ApiError::bad)?;
    let intent = parse_intent(&resolved_kdl, &state.node).map_err(ApiError::bad)?;
    let mission_id = current
        .mission
        .strip_prefix("mission/")
        .unwrap_or(&current.mission);
    let Some(replacement) = intent.missions.get(mission_id) else {
        return Err(ApiError::bad(St3Error::new(
            "wrong-mission-revision",
            format!("revision does not replace mission `{mission_id}`"),
        )));
    };
    let old = state
        .store
        .mission_spec(mission_id, Some(&current.revision))
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-mission-revision",
                "the current mission revision is unavailable",
            ))
        })?;
    if old.inputs != replacement.inputs {
        return Err(ApiError::bad(St3Error::new(
            "run-input-mutation",
            "a run revision cannot change its input declarations",
        )));
    }
    let actor = if request.actor.contains('/') {
        request.actor.clone()
    } else {
        format!("agent/{}", request.actor)
    };
    let (_, reviewers) =
        crate::store::analyze_mission_revision(&old, replacement, &current.requester)
            .map_err(ApiError::bad)?;
    let mut publication = intent.clone();
    publication.subjects.clear();
    let mut planned = state
        .store
        .mission(
            &publication,
            crate::model::IntentInput {
                kdl: resolved_kdl,
                source_name: request.intent.source_name,
            },
        )
        .map_err(ApiError::bad)?;
    publication_refusals(&state, &publication)
        .await?
        .block(&mut planned);
    if !planned.blockers.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "mission-revision-blocked",
            planned.blockers.join("; "),
        )));
    }
    // The revision records its publisher: a broken gate in it raises attention for them.
    state
        .store
        .apply_as(
            &publication,
            &planned.subject_tokens,
            &format!("{}:publish", request.idempotency_key),
            Some(&actor),
        )
        .map_err(ApiError::bad)?;
    // A failed run has no active work to drain, so it adopts an unreviewed revision now.
    let reopening = current.status == "failed" && current.phase == "terminal";
    let revised = if reviewers.is_empty()
        && (reopening || matches!(old.revision_cutover, RevisionCutover::RestartActive))
    {
        let mission_run = state
            .store
            .adopt_mission_revision(
                &run,
                replacement,
                &actor,
                &request.reason,
                &format!("{}:adopt", request.idempotency_key),
            )
            .map_err(ApiError::bad)?;
        RevisionSubmissionView {
            status: "applied".into(),
            mission_run,
            proposal: None,
        }
    } else {
        let proposal = state
            .store
            .create_revision_proposal(
                &run,
                replacement,
                &actor,
                &request.reason,
                &format!("{}:propose", request.idempotency_key),
            )
            .map_err(ApiError::bad)?;
        RevisionSubmissionView {
            status: proposal.status.clone(),
            mission_run: state
                .store
                .mission_run(&run)
                .map_err(ApiError::internal)?
                .expect("the revised mission run exists"),
            proposal: Some(proposal),
        }
    };
    signal_changed(&state);
    Ok(Json(revised))
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
        status: proposal.status.clone(),
        mission_run,
        proposal: Some(proposal),
    }))
}

async fn list_run_generations(
    State(state): State<AppState>,
    AxumPath(run): AxumPath<String>,
) -> Result<Json<Vec<RunGenerationView>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || store.run_generations(&run))
        .await
        .map(Json)
}

async fn get_run_generation(
    State(state): State<AppState>,
    AxumPath(generation): AxumPath<String>,
) -> Result<Json<RunGenerationView>, ApiError> {
    let store = state.store.clone();
    let generation_for_read = generation.clone();
    blocking_store(move || store.run_generation(&generation_for_read))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("run generation `{generation}` does not exist")))
}

async fn get_run_revision_proposal(
    State(state): State<AppState>,
    AxumPath(run): AxumPath<String>,
) -> Result<Json<RevisionProposalView>, ApiError> {
    let store = state.store.clone();
    let run_for_read = run.clone();
    blocking_store(move || store.revision_proposal_for_run(&run_for_read))
        .await?
        .map(Json)
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "mission run `{run}` has no pending revision proposal"
            ))
        })
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
}

async fn list_work(
    State(state): State<AppState>,
    Query(query): Query<WorkQuery>,
) -> Result<Json<Vec<StepRunView>>, ApiError> {
    let store = state.store.clone();
    let (mut work, desired) = blocking_store(move || {
        let work = store.work(query.actor.as_deref(), query.include_terminal)?;
        // Only the agents these steps name, not every subject the fleet has ever declared.
        let actors = work
            .iter()
            .flat_map(|step| {
                step.claimant
                    .iter()
                    .chain(step.assigned_to.iter())
                    .chain(step.available_to.iter())
                    .cloned()
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let desired = store.desired_subjects_named(&actors)?;
        Ok((work, desired))
    })
    .await?;
    let agents = desired
        .into_iter()
        .filter(|subject| subject.kind == "agent")
        .map(|subject| (subject.subject, crate::graph::agent_under(&subject.desired)))
        .collect::<BTreeMap<_, _>>();
    for step in &mut work {
        if let Some(actor) = step
            .claimant
            .as_ref()
            .or(step.assigned_to.as_ref())
            .or_else(|| (step.available_to.len() == 1).then(|| &step.available_to[0]))
        {
            step.under = agents.get(actor).cloned().unwrap_or_default();
        }
    }
    Ok(Json(work))
}

async fn get_work(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
) -> Result<Json<StepRunView>, ApiError> {
    let subject = if subject.starts_with("step-run/") {
        subject
    } else {
        format!("step-run/{subject}")
    };
    state
        .store
        .step_run(&subject)
        .map_err(ApiError::internal)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("step run `{subject}` does not exist")))
}

async fn wake_work(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<WorkWakeRequest>,
) -> Result<Json<MessageView>, ApiError> {
    if request.reason.trim().is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "missing-wake-reason",
            "a manual work wake needs a reason",
        )));
    }
    let step = state
        .store
        .step_run(&subject)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("step run `{subject}` does not exist")))?;
    let agent = step.assigned_to.as_deref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "work-has-no-assignee",
            format!("step run `{}` has no exact assignee to wake", step.subject),
        ))
    })?;
    let allowed = request
        .actor
        .strip_prefix("person/")
        .is_some_and(|name| !name.is_empty() && !name.contains('/'))
        || normalized_agent_actor(&request.actor).is_some();
    if !allowed {
        return Err(ApiError::bad(St3Error::new(
            "invalid-wake-actor",
            format!("`{}` is neither a person nor an agent", request.actor),
        )));
    }
    if let Some(existing) = state
        .store
        .operation_claim(&request.idempotency_key)
        .map_err(ApiError::internal)?
    {
        let message = state
            .store
            .message(&existing.subject)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::internal("the wake operation message is unavailable"))?;
        return Ok(Json(message));
    }
    if step.status != "ready" {
        return Err(ApiError::bad(St3Error::new(
            "work-not-ready",
            format!(
                "step run `{}` is `{}`, not ready",
                step.subject, step.status
            ),
        )));
    }
    let harness = state
        .store
        .current_harness(agent)
        .map_err(ApiError::internal)?
        .filter(crate::model::CurrentHarnessView::is_ready)
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "assignee-harness-not-ready",
                format!("assignee `{agent}` has no ready current harness"),
            ))
        })?;
    if !state
        .store
        .fresh_context_ready(&step, agent, &harness.incarnation_id)
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::bad(St3Error::new(
            "fresh-context-pending",
            format!(
                "`{agent}` must start a fresh harness session before waking `{}`",
                step.subject
            ),
        )));
    }
    let attempt = step
        .wake
        .as_ref()
        .map(|wake| wake.attempts)
        .unwrap_or_default()
        .saturating_add(1);
    let message = crate::reconcile::append_work_wake_message(
        &state.store,
        &step,
        agent,
        &harness.incarnation_id,
        attempt,
        "manual",
        &request.actor,
        &request.reason,
        request.idempotency_key,
    )
    .map_err(ApiError::internal)?;
    signal_changed(&state);
    Ok(Json(message))
}

/// Retry one failed step as a person or an agent.
async fn retry_work(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<WorkRetryRequest>,
) -> Result<Json<MissionRunView>, ApiError> {
    if request.reason.trim().is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "missing-retry-reason",
            "a work retry needs a reason",
        )));
    }
    if let Some(cached) = state
        .store
        .cached_idempotency_response::<MissionRunView>(&request.idempotency_key)
        .map_err(ApiError::internal)?
    {
        return Ok(Json(cached));
    }
    let actor = match request.actor.as_str() {
        actor if actor.starts_with("person/") => actor.to_owned(),
        actor => normalized_agent_actor(actor).ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "retry-authority-denied",
                "a work retry needs a person or agent actor",
            ))
        })?,
    };
    state
        .store
        .step_run(&subject)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("step run `{subject}` does not exist")))?;
    let store = state.store.clone();
    let retried = blocking_action(move || {
        store.retry_failed_step(&subject, &actor, &request.reason, &request.idempotency_key)
    })
    .await?;
    signal_changed(&state);
    Ok(Json(retried))
}

/// A person or an agent sets a finished run's outcome.
async fn set_mission_run_outcome(
    State(state): State<AppState>,
    AxumPath(run): AxumPath<String>,
    Json(request): Json<MissionRunOutcomeRequest>,
) -> Result<Json<MissionRunView>, ApiError> {
    if let Some(cached) = state
        .store
        .cached_idempotency_response::<MissionRunView>(&request.idempotency_key)
        .map_err(ApiError::internal)?
    {
        return Ok(Json(cached));
    }
    let actor = person_or_agent_actor(&request.actor, "run-outcome-authority-denied")?;
    let current = state
        .store
        .mission_run(&run)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("mission run `{run}` does not exist")))?;
    let store = state.store.clone();
    let outcome = blocking_action(move || {
        store.set_mission_run_outcome(
            &current.subject,
            &request.status,
            &actor,
            &request.reason,
            &request.idempotency_key,
        )
    })
    .await?;
    signal_changed(&state);
    Ok(Json(outcome))
}

/// A person or an agent retires a mission.
async fn retire_mission(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<MissionRetireRequest>,
) -> Result<Json<crate::model::MissionSpec>, ApiError> {
    let actor = person_or_agent_actor(&request.actor, "mission-retire-authority-denied")?;
    let store = state.store.clone();
    let retired =
        blocking_action(move || store.retire_mission(&id, &actor, &request.idempotency_key))
            .await?;
    signal_changed(&state);
    Ok(Json(retired))
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

async fn publish_work_mission(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<MissionProductionRequest>,
) -> Result<Json<MissionOutputView>, ApiError> {
    let step = state
        .store
        .step_run(&subject)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("step run `{subject}` does not exist")))?;
    let actor = if request.actor.contains('/') {
        request.actor.clone()
    } else {
        format!("agent/{}", request.actor)
    };
    if !state
        .store
        .mission_output_authorized(&step.subject, &actor, request.incarnation.as_deref())
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::bad(St3Error::new(
            "work-not-claimed",
            format!(
                "`{actor}` does not hold the mission-producing step `{}` or its nested work",
                step.subject
            ),
        )));
    }
    let run = state
        .store
        .mission_run(&step.run)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found(format!("mission run `{}` does not exist", step.run)))?;
    let root = state
        .store
        .mission_spec(
            run.mission.strip_prefix("mission/").unwrap_or(&run.mission),
            Some(&run.revision),
        )
        .map_err(ApiError::internal)?
        .ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "missing-mission-revision",
                "the running mission revision is unavailable",
            ))
        })?;
    let definition = crate::mission::find_step(&root, &step.step).ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "missing-step-definition",
            format!("step `{}` is absent from its mission revision", step.step),
        ))
    })?;
    let expected_mission = definition.produces_mission.as_deref().ok_or_else(|| {
        ApiError::bad(St3Error::new(
            "step-does-not-produce-mission",
            format!("step `{}` does not declare produces-mission", step.step),
        ))
    })?;

    let initial = parse_intent(&request.intent.kdl, &state.node).map_err(ApiError::bad)?;
    if initial.missions.len() != 1 {
        return Err(ApiError::bad(St3Error::new(
            "invalid-mission-output-intent",
            "a mission output must contain exactly one mission",
        )));
    }
    if !initial.subjects.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "invalid-mission-output-intent",
            "a mission output can contain only its mission",
        )));
    }
    let bindings = state
        .store
        .document_bindings_at(&initial.document_refs, None)
        .map_err(ApiError::internal)?;
    let resolved_kdl =
        resolve_document_references(&request.intent.kdl, &bindings).map_err(ApiError::bad)?;
    let intent = parse_intent(&resolved_kdl, &state.node).map_err(ApiError::bad)?;
    let mission = intent
        .missions
        .values()
        .next()
        .expect("one mission was checked");
    if mission.id != expected_mission || mission.state != crate::model::MissionState::Ready {
        return Err(ApiError::bad(St3Error::new(
            "wrong-mission-output",
            format!(
                "step `{}` must publish ready mission `{expected_mission}`",
                step.step
            ),
        )));
    }
    let mut publication = intent.clone();
    publication.subjects.clear();
    let mut planned = state
        .store
        .mission(
            &publication,
            crate::model::IntentInput {
                kdl: resolved_kdl,
                source_name: request.intent.source_name,
            },
        )
        .map_err(ApiError::bad)?;
    publication_refusals(&state, &publication)
        .await?
        .block(&mut planned);
    if !planned.blockers.is_empty() {
        return Err(ApiError::bad(St3Error::new(
            "mission-output-blocked",
            planned.blockers.join("; "),
        )));
    }
    state
        .store
        .apply(
            &publication,
            &planned.subject_tokens,
            &format!("{}:publish", request.idempotency_key),
        )
        .map_err(ApiError::bad)?;
    let output = state
        .store
        .record_mission_output(
            &step.subject,
            &actor,
            request.incarnation.as_deref(),
            expected_mission,
            mission,
            &format!("{}:bind", request.idempotency_key),
        )
        .map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(output))
}

/// Move one run in a seat's queue as a person or an agent.
async fn move_agent_queue(
    State(state): State<AppState>,
    Json(mut request): Json<crate::model::SeatQueueMoveRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    request.agent = if request.agent.starts_with("agent/") {
        request.agent
    } else {
        format!("agent/{}", request.agent)
    };
    request.actor = match request.actor.as_str() {
        actor if actor.starts_with("person/") => actor.to_owned(),
        actor => normalized_agent_actor(actor).ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "invalid-queue-move-actor",
                "a queue move needs a person or agent actor",
            ))
        })?,
    };
    let store = state.store.clone();
    let claim = blocking_action(move || store.move_seat_queue_run(&request)).await?;
    signal_changed(&state);
    Ok(Json(claim))
}

#[derive(Debug, Default, Deserialize)]
struct LaneListQuery {
    #[serde(default)]
    all: bool,
    #[serde(default)]
    run: Option<String>,
}

/// Every open lane, every declared lane with `?all=true`, or the lanes of one `?run=`.
async fn list_lanes(
    State(state): State<AppState>,
    Query(query): Query<LaneListQuery>,
) -> Result<Json<Vec<crate::model::LaneView>>, ApiError> {
    let store = state.store.clone();
    blocking_store(move || match query.run.as_deref() {
        Some(run) => store.lanes_for_run(run),
        None => store.lanes(query.all),
    })
    .await
    .map(Json)
}

/// One lane by subject, `RUN/NAME`, run, mission, or unique name.
async fn get_lane(
    State(state): State<AppState>,
    AxumPath(lane): AxumPath<String>,
) -> Result<Json<crate::model::LaneView>, ApiError> {
    let store = state.store.clone();
    blocking_action(move || {
        let subject = store.resolve_lane(&lane)?;
        store
            .lane(&subject)
            .map_err(|error| St3Error::new("internal", error.to_string()))?
            .ok_or_else(|| St3Error::new("lane-not-found", format!("no lane `{subject}`")))
    })
    .await
    .map(Json)
}

/// Join, leave, move, mark, or approve one lane entry as a person or an agent. A harness can
/// only act as its own seat; `guard_bound_request` checks that before this runs.
async fn change_lane(
    State(state): State<AppState>,
    Json(mut request): Json<crate::model::LaneChangeRequest>,
) -> Result<Json<crate::model::LaneChangeResponse>, ApiError> {
    request.actor = match request.actor.trim() {
        actor if actor.starts_with("person/") => actor.to_owned(),
        actor => normalized_agent_actor(actor).ok_or_else(|| {
            ApiError::bad(St3Error::new(
                "invalid-lane-actor",
                "a lane change needs a person or agent actor",
            ))
        })?,
    };
    let store = state.store.clone();
    let response = blocking_action(move || store.change_lane(&request)).await?;
    if response.claim.is_some() {
        signal_changed(&state);
    }
    Ok(Json(response))
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

async fn post_work_action(
    State(state): State<AppState>,
    AxumPath((action, subject)): AxumPath<(String, String)>,
    Json(request): Json<WorkRequest>,
) -> Result<Json<StepRunView>, ApiError> {
    work_action_response(state, action, subject, request, None).await
}

/// Add time to the execution budget of the step attempt this seat holds.
async fn extend_work(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<crate::model::WorkExtendRequest>,
) -> Result<Json<StepRunView>, ApiError> {
    let extend_ms = request.by_ms;
    let request = WorkRequest {
        actor: request.actor,
        incarnation: request.incarnation,
        summary: None,
        reason: request.reason,
        evidence: Vec::new(),
        idempotency_key: request.idempotency_key,
    };
    work_action_response(state, "extend".into(), subject, request, Some(extend_ms)).await
}

async fn work_action_response(
    state: AppState,
    action: String,
    subject: String,
    request: WorkRequest,
    extend_ms: Option<u64>,
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
    if let Some(response) = state
        .store
        .cached_idempotency_response::<StepRunView>(&request.idempotency_key)
        .map_err(ApiError::internal)?
    {
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
    let (mut response, desired) = blocking_action(move || {
        let response = store.work_action_extending(&subject, &action, &request, extend_ms)?;
        let desired = store.desired_subjects().map_err(|error| {
            St3Error::new("store-read-failed", format!("read desired agents: {error}"))
        })?;
        Ok((response, desired))
    })
    .await?;
    if let Some(assignee) = response
        .claimant
        .as_ref()
        .or(response.assigned_to.as_ref())
        .or_else(|| (response.available_to.len() == 1).then(|| &response.available_to[0]))
    {
        response.under = desired
            .into_iter()
            .filter(|subject| subject.kind == "agent")
            .map(|subject| (subject.subject, crate::graph::agent_under(&subject.desired)))
            .collect::<BTreeMap<_, _>>()
            .get(assignee)
            .cloned()
            .unwrap_or_default();
    }
    if quiet_renewal {
        signal_visible_change(&state);
    } else {
        signal_changed(&state);
    }
    Ok(Json(response))
}

fn stage_eval_documents(
    state: &AppState,
    workspace: &Path,
    intent: &crate::model::NormalizedIntent,
) -> Result<(), St3Error> {
    for reference in &intent.document_refs {
        let (name, hash) = reference.rsplit_once('@').ok_or_else(|| {
            St3Error::new(
                "invalid-document-reference",
                format!("eval document reference `{reference}` has no hash"),
            )
        })?;
        if state
            .store
            .get_document(name, hash)
            .map_err(|error| St3Error::new("internal", error.to_string()))?
            .is_some()
        {
            continue;
        }
        let path = workspace.join(".st3-documents").join(hash);
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            St3Error::new(
                "missing-eval-document",
                format!(
                    "eval document `{reference}` is absent from the store and {} is not staged: {error}",
                    path.display()
                ),
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(St3Error::new(
                "invalid-eval-document",
                format!(
                    "staged eval document {} is not a regular file",
                    path.display()
                ),
            ));
        }
        let bytes = fs::read(&path).map_err(|error| {
            St3Error::new(
                "invalid-eval-document",
                format!("read staged eval document {}: {error}", path.display()),
            )
        })?;
        if hex::encode(Sha256::digest(&bytes)) != hash {
            return Err(St3Error::new(
                "eval-document-hash-mismatch",
                format!(
                    "staged eval document {} does not match `{reference}`",
                    path.display()
                ),
            ));
        }
        let expected = state
            .store
            .latest_document_token(name)
            .map_err(|error| St3Error::new("internal", error.to_string()))?;
        state.store.put_document(
            name,
            &bytes,
            &expected,
            &format!("eval-document:{name}:{hash}"),
        )?;
    }
    Ok(())
}

#[derive(Debug)]
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
    let status = state
        .store
        .status(Some(subject))
        .map_err(ApiError::internal)?;
    let selected = status
        .subjects
        .first()
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
        .desired_subjects()
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|desired| desired.subject == subject)
        .and_then(|desired| desired.member);
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

#[derive(Deserialize)]
struct SessionLogQuery {
    #[serde(default)]
    after: u64,
    #[serde(default = "default_log_limit")]
    limit: usize,
    #[serde(default)]
    previous: bool,
    #[serde(default)]
    wait: bool,
}

fn default_log_limit() -> usize {
    64 * 1024
}

async fn logs_session(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Query(query): Query<SessionLogQuery>,
) -> Result<Json<SessionLogChunk>, ApiError> {
    if query.limit == 0 || query.limit > 64 * 1024 {
        return Err(ApiError::bad(St3Error::new(
            "invalid-log-limit",
            "a log chunk limit must be between 1 and 65536 bytes",
        )));
    }
    let session = live_session(&state, &subject, None)?;
    if session.terminal {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-capability",
            "terminal sessions expose a screen instead of an exec log",
        )));
    }
    let runtime =
        st_runtime::ExecRuntime::new(state.state_dir.join("exec"), state.state_dir.join("logs"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let mut generation = if query.previous {
            runtime
                .previous_generation(&session.runtime_id)
                .map_err(ApiError::internal)?
        } else {
            match runtime
                .observe(&session.runtime_id)
                .map_err(ApiError::internal)?
            {
                Some(st_runtime::ExecObservation::Running(generation))
                | Some(st_runtime::ExecObservation::Exited(generation)) => Some(generation),
                Some(st_runtime::ExecObservation::Indeterminate(reason)) => {
                    return Err(ApiError::internal(reason));
                }
                None => None,
            }
        }
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "subject `{subject}` has no {}exec log generation",
                if query.previous { "previous " } else { "" }
            ))
        })?;
        if !query.previous && generation.generation_id != session.incarnation_id {
            return Err(ApiError::bad(St3Error::new(
                "stale-incarnation",
                format!("subject `{subject}` changed exec generation"),
            )));
        }
        let log = runtime
            .read_log_bytes(&session.runtime_id, query.previous)
            .map_err(ApiError::internal)?
            .unwrap_or_default();
        let start = usize::try_from(query.after)
            .unwrap_or(usize::MAX)
            .min(log.len());
        let end = start.saturating_add(query.limit).min(log.len());
        let running = if query.previous {
            false
        } else {
            match runtime
                .observe(&session.runtime_id)
                .map_err(ApiError::internal)?
            {
                Some(st_runtime::ExecObservation::Running(latest)) => {
                    generation = latest;
                    true
                }
                Some(st_runtime::ExecObservation::Exited(latest)) => {
                    generation = latest;
                    false
                }
                Some(st_runtime::ExecObservation::Indeterminate(reason)) => {
                    return Err(ApiError::internal(reason));
                }
                None => false,
            }
        };
        if end > start || !query.wait || !running || tokio::time::Instant::now() >= deadline {
            return Ok(Json(SessionLogChunk {
                subject,
                runtime_id: session.runtime_id,
                generation_id: generation.generation_id,
                previous: query.previous,
                start_offset: start as u64,
                next_offset: end as u64,
                data_base64: base64::engine::general_purpose::STANDARD.encode(&log[start..end]),
                eof: end == log.len() && !running,
                status: if running { "running" } else { "exited" }.into(),
                exit_code: generation.exit_code,
                exit_signal: generation.exit_signal,
            }));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn screen_session(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
) -> Result<Json<SessionScreen>, ApiError> {
    let session = live_session(&state, &subject, None)?;
    if !session.terminal {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-capability",
            "an exec session has a log instead of a terminal screen",
        )));
    }
    let screen = daemon_pty(&state)
        .and_then(|runtime| runtime.screen(&session.runtime_id))
        .map_err(ApiError::internal)?;
    Ok(Json(SessionScreen {
        subject,
        runtime_id: session.runtime_id,
        incarnation_id: session.incarnation_id,
        screen,
    }))
}

async fn input_session(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<SessionInputRequest>,
) -> Result<Json<SessionControlResponse>, ApiError> {
    input_session_as(&state, subject, request, "requester").await
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
    let effect = daemon_pty(state).and_then(|runtime| match request.mode {
        SessionInputMode::Line => runtime.send_line_if(
            &session.runtime_id,
            &request.value,
            Some(&session.incarnation_id),
        ),
        SessionInputMode::Raw => {
            runtime.send_raw_if(&session.runtime_id, &bytes, Some(&session.incarnation_id))
        }
        SessionInputMode::Key => runtime.send_key_if(
            &session.runtime_id,
            &request.value,
            Some(&session.incarnation_id),
        ),
    });
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

async fn clear_context(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<ContextClearRequest>,
) -> Result<Json<SessionControlResponse>, ApiError> {
    let result_key = format!(
        "session-control-result:context-clear:{subject}:{}",
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
    if !session.terminal || session.driver.as_deref() != Some("claude") {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-capability",
            "context clear requires a terminal Claude driver in st v1",
        )));
    }
    let request_key = format!(
        "session-control-request:context-clear:{subject}:{}",
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
            "harness.context-clear.result",
            &result_key,
            &prior,
            &session,
            Ok(()),
        );
    }
    let request_claim = state
        .store
        .append_claim(&ClaimInput {
            subject: subject.clone(),
            kind: "harness.context-clear.requested".into(),
            actor: Some("requester".into()),
            fields: BTreeMap::from([
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
    let effect = daemon_pty(&state).and_then(|runtime| {
        runtime.send_line_if(&session.runtime_id, "/clear", Some(&session.incarnation_id))
    });
    finish_session_control(
        &state,
        &subject,
        "harness.context-clear.result",
        &result_key,
        &request_claim,
        &session,
        effect,
    )
}

async fn signal_session(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(request): Json<SessionSignalRequest>,
) -> Result<Json<SessionControlResponse>, ApiError> {
    signal_session_as(state, subject, request, "requester").await
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
    let effect = if session.terminal {
        daemon_pty(&state).and_then(|runtime| {
            runtime.signal_if(&session.runtime_id, Some(&session.incarnation_id), signal)
        })
    } else {
        st_runtime::ExecRuntime::new(state.state_dir.join("exec"), state.state_dir.join("logs"))
            .signal_if(&session.runtime_id, Some(&session.incarnation_id), signal)
    };
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

async fn attach_session(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Json(_request): Json<AttachRequest>,
) -> Result<Json<Attachment>, ApiError> {
    let session = live_session(&state, &subject, None)?;
    if !session.terminal {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-capability",
            "terminal attachment requires a terminal session",
        )));
    }
    // Issuing a capability waits for the store writer, which a reconcile pass can hold for
    // seconds; the wait must not hold a runtime worker.
    let store = state.store.clone();
    let (capability_subject, incarnation_id) = (subject.clone(), session.incarnation_id.clone());
    let (capability, expires_at_unix_ms) = blocking_store(move || {
        store.issue_capability(
            "terminal",
            &capability_subject,
            Some(&incarnation_id),
            30_000,
        )
    })
    .await?;
    Ok(Json(Attachment {
        websocket_path: format!(
            "/v1/sessions/terminal/{}?capability={}",
            urlencoding::encode(&subject),
            capability
        ),
        subject,
        runtime_id: session.runtime_id,
        incarnation_id: Some(session.incarnation_id),
        capability,
        expires_at_unix_ms,
    }))
}

/// Name the PTY session of a running terminal on this host, so `st terminals attach` can connect
/// to it directly. It checks what an attachment checks but issues no capability: it only reads,
/// so it answers while a busy reconciler holds the writer, and the attach never waits for this
/// daemon again. The caller proves the incarnation against the PTY itself before attaching.
async fn local_terminal(
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
) -> Result<Json<LocalTerminal>, ApiError> {
    // A read can still wait on a saturated disk; it must not hold a runtime worker meanwhile.
    let (lookup, lookup_subject) = (state.clone(), subject.clone());
    let session = tokio::task::spawn_blocking(move || live_session(&lookup, &lookup_subject, None))
        .await
        .map_err(ApiError::internal)??;
    if !session.terminal {
        return Err(ApiError::bad(St3Error::new(
            "unsupported-capability",
            "terminal attachment requires a terminal session",
        )));
    }
    // An isolated daemon may run with a relative PTY root; the client resolves nothing itself.
    let pty_root = std::path::absolute(&state.pty_root).map_err(ApiError::internal)?;
    Ok(Json(LocalTerminal {
        subject,
        runtime_id: session.runtime_id,
        incarnation_id: session.incarnation_id,
        pty_root,
    }))
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
        .ok_or_else(|| remote_unavailable(&host_id))?;
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

async fn post_gate_result(
    State(state): State<AppState>,
    Json(request): Json<GateResultRequest>,
) -> Result<Json<ClaimRecord>, ApiError> {
    if !matches!(request.verdict.as_str(), "pass" | "fail") {
        return Err(ApiError::bad(St3Error::new(
            "invalid-gate-result",
            "a gate-result verdict must be pass or fail",
        )));
    }
    let capability = state
        .store
        .capability(&request.operation_capability, "gate-result")
        .map_err(ApiError::bad)?;
    if capability.used {
        let prior = state
            .store
            .latest_claim(&capability.subject, Some("gate.result"))
            .map_err(ApiError::internal)?
            .ok_or_else(|| {
                ApiError::bad(St3Error::new(
                    "used-capability",
                    "the gate-result capability was already consumed",
                ))
            })?;
        let same = prior
            .body
            .pointer("/fields/verdict")
            .and_then(Value::as_str)
            == Some(request.verdict.as_str())
            && prior.body.pointer("/fields/reason").and_then(Value::as_str)
                == Some(request.reason.as_str());
        if same {
            return Ok(Json(prior));
        }
        return Err(ApiError::bad(St3Error::new(
            "used-capability",
            "the gate-result capability was already used for another verdict",
        )));
    }
    let input = ClaimInput {
        subject: capability.subject.clone(),
        kind: "gate.result".into(),
        actor: None,
        fields: BTreeMap::from([
            ("verdict".into(), Value::String(request.verdict.clone())),
            ("reason".into(), Value::String(request.reason.clone())),
        ]),
        evidence: request.evidence.clone(),
        expected_subject: None,
        idempotency_key: Some(request.idempotency_key.clone()),
    };
    state
        .store
        .validate_claim_input(&input)
        .map_err(ApiError::bad)?;
    for evidence in &input.evidence {
        if !state
            .store
            .evidence_exists(evidence)
            .map_err(ApiError::internal)?
        {
            return Err(ApiError::bad(St3Error::new(
                "missing-evidence",
                format!("evidence claim `{evidence}` is not stored"),
            )));
        }
    }
    let capability = state
        .store
        .consume_capability(&request.operation_capability, "gate-result")
        .map_err(ApiError::bad)?;
    if capability.used {
        let prior = state
            .store
            .latest_claim(&capability.subject, Some("gate.result"))
            .map_err(ApiError::internal)?
            .ok_or_else(|| {
                ApiError::bad(St3Error::new(
                    "used-capability",
                    "the gate-result capability was already consumed",
                ))
            })?;
        let same = prior
            .body
            .pointer("/fields/verdict")
            .and_then(Value::as_str)
            == Some(request.verdict.as_str())
            && prior.body.pointer("/fields/reason").and_then(Value::as_str)
                == Some(request.reason.as_str());
        if same {
            return Ok(Json(prior));
        }
        return Err(ApiError::bad(St3Error::new(
            "used-capability",
            "the gate-result capability was already used for another verdict",
        )));
    }
    let response = state.store.append_claim(&input).map_err(ApiError::bad)?;
    signal_changed(&state);
    Ok(Json(response))
}

#[derive(Deserialize)]
struct TerminalQuery {
    capability: String,
}

async fn terminal_session(
    websocket: WebSocketUpgrade,
    State(state): State<AppState>,
    AxumPath(subject): AxumPath<String>,
    Query(query): Query<TerminalQuery>,
) -> Result<Response, ApiError> {
    let store = state.store.clone();
    let secret = query.capability;
    let capability = blocking_action(move || store.consume_capability(&secret, "terminal")).await?;
    if capability.used || capability.subject != subject {
        return Err(ApiError::bad(St3Error::new(
            "invalid-capability",
            "the terminal capability is used or names another subject",
        )));
    }
    let status = state
        .store
        .status(Some(&subject))
        .map_err(ApiError::internal)?;
    let actual = status
        .subjects
        .first()
        .and_then(|item| item.actual.as_ref())
        .ok_or_else(|| ApiError::not_found(format!("subject `{subject}` has no live session")))?;
    let fields = actual.get("fields").unwrap_or(actual);
    let runtime_id = fields
        .get("runtime_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::not_found(format!("subject `{subject}` has no runtime")))?
        .to_owned();
    let current_incarnation = fields
        .get("incarnation_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if capability.incarnation_id != current_incarnation {
        return Err(ApiError::bad(St3Error::new(
            "stale-incarnation",
            "the terminal incarnation changed before attachment",
        )));
    }
    Ok(websocket
        .protocols(["st3.terminal.v1"])
        .on_upgrade(move |socket| terminal_proxy(socket, state.pty_root, runtime_id)))
}

async fn terminal_proxy(socket: WebSocket, pty_root: std::path::PathBuf, runtime_id: String) {
    let stream =
        match tokio::net::UnixStream::connect(pty_root.join(format!("{runtime_id}.sock"))).await {
            Ok(stream) => stream,
            Err(_) => return,
        };
    let (mut writer, mut reader) = socket.split();
    let (mut output_stream, mut input_stream) = stream.into_split();
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut bytes = vec![0_u8; 8192];
    loop {
        tokio::select! {
            read = output_stream.read(&mut bytes) => {
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(count) if writer
                        .send(WsMessage::Binary(bytes[..count].to_vec().into()))
                        .await
                        .is_err() => break,
                    Ok(_) => {}
                }
            }
            message = reader.next() => {
                let payload = match message {
                    Some(Ok(WsMessage::Binary(bytes))) => bytes.to_vec(),
                    Some(Ok(WsMessage::Text(text))) => text.as_bytes().to_vec(),
                    Some(Ok(WsMessage::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(WsMessage::Ping(_))) | Some(Ok(WsMessage::Pong(_))) => continue,
                };
                if input_stream.write_all(&payload).await.is_err() {
                    break;
                }
            }
        }
    }
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
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use std::path::PathBuf;

    #[tokio::test]
    async fn a_seat_reads_its_own_desired_record_for_its_status_line() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        state
            .store
            .apply_internal(
                &parse_intent(
                    r#"version 2
agent "eval/named" { name "Quartz"; workspace "/tmp"; harness "claude" {} }
"#,
                    "node",
                )
                .unwrap(),
                "desired-fixture",
            )
            .unwrap();
        let app = router(state);
        let (status, seat) = get_request(app.clone(), "/v1/desired/agent/eval/named").await;
        assert_eq!(status, StatusCode::OK, "{seat}");
        let seat: crate::model::DesiredSubject = serde_json::from_value(seat).unwrap();
        assert_eq!(
            crate::mailbox::seat_label(&seat, Some("gen")),
            "Quartz[gen]"
        );
        let (status, missing) = get_request(app, "/v1/desired/agent/eval/absent").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    }

    #[tokio::test]
    async fn delivery_hold_api_enforces_authority_and_keeps_presence_separate() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        state
            .store
            .apply_internal(
                &parse_intent(
                    r#"version 2
agent "eval/held" { workspace "/tmp"; harness "codex" {} }
agent "eval/channel" { workspace "/tmp"; harness "claude" {} }
"#,
                    "node",
                )
                .unwrap(),
                "hold-fixture",
            )
            .unwrap();
        let store = state.store.clone();
        let app = router(state);
        let now = client_now_ms() as u64;
        let request = json!({"subject":"agent/eval/held", "actor":"person/alex", "held":true,
            "until_unix_ms": now + 60_000, "reason":"quiet interval", "idempotency_key":"hold-api"});
        let (status, claim) = json_request(app.clone(), "/v1/delivery/hold", request.clone()).await;
        assert_eq!(status, StatusCode::OK, "{claim}");
        let (status, replay) =
            json_request(app.clone(), "/v1/delivery/hold", request.clone()).await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(claim["id"], replay["id"]);
        let (_, hold) =
            get_request(app.clone(), "/v1/delivery/hold?subject=agent%2Feval%2Fheld").await;
        assert_eq!(hold["active"], true);
        assert!(
            store
                .latest_claim("agent/eval/held", Some("agent.presence"))
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .latest_claim("agent/eval/held", Some("harness.observed"))
                .unwrap()
                .is_none()
        );
        let mut foreign = request.clone();
        foreign["actor"] = json!("agent/eval/other");
        let (status, error) = json_request(app.clone(), "/v1/delivery/hold", foreign).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error["code"], "delivery-hold-forbidden");
        let mut unsupported = request.clone();
        unsupported["subject"] = json!("agent/eval/channel");
        let (_, error) = json_request(app.clone(), "/v1/delivery/hold", unsupported).await;
        assert_eq!(error["code"], "unsupported-delivery-hold");
        let mut release = request;
        release["held"] = json!(false);
        release["until_unix_ms"] = json!(0);
        release["actor"] = json!("agent/eval/held");
        release["idempotency_key"] = json!("hold-release");
        let (status, result) = json_request(app.clone(), "/v1/delivery/hold", release).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        let (_, hold) = get_request(app, "/v1/delivery/hold?subject=agent%2Feval%2Fheld").await;
        assert_eq!(hold["active"], false);
    }

    #[tokio::test]
    async fn history_pages_and_detail_do_not_cache_the_whole_claim_log() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        for number in 0..8 {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: "daemon/node".into(),
                    kind: "daemon.diagnostic".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("code".into(), Value::String("history-test".into())),
                        ("severity".into(), Value::String("error".into())),
                        ("reason".into(), Value::String(number.to_string())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let snapshot = new_client_snapshot(&state);
        let first = client_history(
            State(state.clone()),
            Extension(snapshot.clone()),
            Query(ClientListQuery {
                limit: Some(1),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(first.items.len(), 1);
        let first_index = first.items[0]["store_index"].as_u64().unwrap();
        let cursor = first.page.next_cursor.unwrap();
        assert!(
            !client_page_cache()
                .lock()
                .unwrap()
                .iter()
                .any(|entry| { entry.snapshot_id == snapshot.id && entry.collection == "history" }),
            "a history page must not retain every claim in the process cache"
        );
        let second = client_history(
            State(state.clone()),
            Extension(snapshot),
            Query(ClientListQuery {
                limit: Some(1),
                cursor: Some(cursor),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(second.items.len(), 1);
        assert!(second.items[0]["store_index"].as_u64().unwrap() < first_index);
        let detail = client_history_detail(State(state), AxumPath(first_index.to_string()))
            .await
            .unwrap()
            .0;
        assert_eq!(detail["store_index"], first_index);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bound_actor_refusals_decode_as_client_api_errors() {
        fn own_seat(_pid: u32) -> Option<String> {
            Some("agent/own".into())
        }
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("api.sock");
        let server_socket = socket.clone();
        // Reaching the handler would succeed. The listener must reject the foreign actor first.
        let app = Router::new().route(
            "/v1/client/glasses/{id}",
            axum::routing::put(|| async { Json(json!({})) }),
        );
        let server = tokio::spawn(async move {
            serve_unix_with_ancestor(&server_socket, None, app, true, own_seat).await
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let error = st3_client::Client::unix_as(&socket, "person/alex")
            .put_glass(
                "0194b2e0-1234-7000-8000-000000000001",
                &st3_client::GlassPut {
                    body: st3_client::GlassBody {
                        name: "Main".into(),
                        layout: st3_client::GlassLayout::Group { tabs: vec![] },
                    },
                    base_revision: None,
                },
                "glass-foreign-actor-test-key",
            )
            .await
            .unwrap_err();
        server.abort();
        let st3_client::ClientError::Api(code, message, envelope) = error else {
            panic!("an actor refusal must be a typed client API error: {error}");
        };
        assert_eq!(code, st3_client::ErrorCode::Forbidden);
        assert!(message.contains("cannot act as"));
        let value = serde_json::to_value(envelope).unwrap();
        assert_eq!(value["api_version"], CLIENT_API_VERSION);
        assert_eq!(value["error_version"], "st3.client.error.v0");
        assert!(
            value["request_id"]
                .as_str()
                .unwrap()
                .starts_with("request/")
        );
        assert_eq!(value["retryable"], false);
        assert!(value["details"].is_object());
    }

    #[tokio::test]
    async fn a_bound_harness_cannot_act_as_another_actor() {
        for path in [
            "/v1/agent-queue-moves",
            "/v1/agents/rename",
            "/v1/agents/restart",
            "/v1/agents/rollout",
            "/v1/agents/start",
            "/v1/agents/suspend",
            "/v1/agents/resume",
            "/v1/agents/native-session",
            "/v1/work/revision/approve/proposal",
            "/v1/mission-runs/example%2Fdemo%2F1/outcome",
            "/v1/mission-runs/example%2Fdemo%2F1/revision",
            "/v1/missions/example%2Fdemo/retire",
            "/v1/subscription-requests/release/subscription-request%2Fexample",
            "/v1/reviews/step-run/example",
        ] {
            for actor in ["agent/peer", "person/operator"] {
                let request = Request::builder()
                    .method("POST")
                    .uri(path)
                    .body(Body::from(
                        json!({"actor": actor, "agent": "agent/worker"}).to_string(),
                    ))
                    .unwrap();
                let error = match guard_bound_request(request, Some("agent/own")).await {
                    Ok(_) => panic!("bound harness acted as {actor} at {path}"),
                    Err(error) => error,
                };
                assert_eq!(error.code, "foreign-agent-actor");
            }
        }
        // Free mode keeps the binding for client-v0 too: a harness names only its own seat.
        for person in ["person/operator", "agent/peer"] {
            let request = Request::builder()
                .method("POST")
                .uri("/v1/client/actions")
                .header(client_v0::LOCAL_PERSON_HEADER, person)
                .body(Body::from("{}"))
                .unwrap();
            let error = guard_bound_request(request, Some("agent/own"))
                .await
                .expect_err("a bound harness acted as another client-v0 actor");
            assert_eq!(error.code, "foreign-agent-actor", "{person}");
        }
        let request = Request::builder()
            .method("POST")
            .uri("/v1/client/actions")
            .header(client_v0::LOCAL_PERSON_HEADER, "agent/own")
            .body(Body::from("{}"))
            .unwrap();
        assert!(
            guard_bound_request(request, Some("agent/own"))
                .await
                .is_ok()
        );
        let request = Request::builder()
            .method("POST")
            .uri("/v1/agent-queue-moves")
            .body(Body::from(json!({"actor": "agent/own"}).to_string()))
            .unwrap();
        assert!(
            guard_bound_request(request, Some("agent/own"))
                .await
                .is_ok()
        );
    }

    /// #901: a seat suspends only once its driver reports it quiet and it has a native session
    /// to resume; a suspension holds the seat against restarts until a resume.
    #[tokio::test]
    async fn suspend_waits_for_a_quiet_seat_with_a_native_session() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let app = router(state.clone());
        let source = format!(
            "version 2\nagent \"test/seat\" {{\n host \"node\"\n workspace {:?}\n harness \"claude\" {{}}\n}}\n",
            root.path().display().to_string()
        );
        let request = apply_request(&state, &source, "person/test", "declare");
        let (status, body) = json_request(
            app.clone(),
            "/v1/intent/apply",
            serde_json::to_value(request).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let subject = "agent/test/seat";
        let incarnation = "4242:2026-10-02T20:00:00.000Z";
        let append = |kind: &str, fields: Value| {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: kind.into(),
                    actor: Some(subject.into()),
                    fields: serde_json::from_value(fields).unwrap(),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        };
        let ask = |path: &'static str, key: &str| {
            json_request(
                app.clone(),
                path,
                json!({"subject": subject, "actor": "person/test", "idempotency_key": key}),
            )
        };
        let (status, body) = ask("/v1/agents/suspend", "stopped").await;
        assert_eq!(body["code"], "suspend-not-running", "{status} {body}");
        append(
            "runtime.observed",
            json!({"status": "running", "runtime_id": "seat", "incarnation_id": incarnation}),
        );
        append(
            "harness.observed",
            json!({"state": "working", "incarnation_id": incarnation, "quiescent": false,
                   "blocking": ["turn-in-flight"]}),
        );
        let (status, body) = ask("/v1/agents/suspend", "busy").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["code"], "suspend-blocked");
        assert_eq!(
            body["details"]["blocking"],
            json!(["native-session-unbound", "turn-in-flight"])
        );
        // Only the seat itself reports its native session.
        let report = |actor: &str| {
            json!({"subject": subject, "actor": actor, "incarnation_id": incarnation,
                   "harness": "claude", "session_id": "native-one", "account_ref": "ada/one"})
        };
        let (_, body) = json_request(
            app.clone(),
            "/v1/agents/native-session",
            report("agent/other"),
        )
        .await;
        assert_eq!(body["code"], "foreign-agent-actor");
        let (status, body) =
            json_request(app.clone(), "/v1/agents/native-session", report(subject)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["body"]["fields"]["account_ref"], "ada/one");
        append(
            "harness.observed",
            json!({"state": "idle", "incarnation_id": incarnation, "quiescent": true,
                   "blocking": []}),
        );
        let (_, body) = ask("/v1/agents/resume", "early").await;
        assert_eq!(body["code"], "not-suspended");
        let (status, body) = ask("/v1/agents/suspend", "quiet").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["body"]["fields"]["action"], "suspend");
        let suspension = crate::suspension::current(&state.store, subject)
            .unwrap()
            .unwrap();
        assert_eq!(suspension.phase, "quiescing");
        assert!(suspension.holds_seat());
        let (_, body) = ask("/v1/agents/suspend", "again").await;
        assert_eq!(body["code"], "already-suspended");
        let (_, body) = ask("/v1/agents/restart", "restart").await;
        assert_eq!(body["code"], "restart-suspended");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_unix_peer_is_bound_to_its_harness_environment() {
        let mut child = std::process::Command::new("sleep")
            .arg("10")
            .env("ST_AGENT", "agent/fixture/worker")
            .spawn()
            .unwrap();
        let mut bound = None;
        for _ in 0..50 {
            bound = harness_ancestor(child.id());
            if bound.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(bound.as_deref(), Some("agent/fixture/worker"));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[tokio::test]
    async fn the_run_start_api_refuses_placeholder_requesters() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        for requester in [None, Some("person/requester".to_owned())] {
            let request = MissionRunRequest {
                mission: "demo".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester,
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "placeholder-run".into(),
            };
            let error = match start_mission_run_action(State(state.clone()), Json(request)).await {
                Ok(_) => panic!("placeholder requester created a run"),
                Err(error) => error,
            };
            assert!(matches!(
                error.code.as_str(),
                "missing-run-requester" | "placeholder-run-requester"
            ));
        }
    }

    #[test]
    fn owner_cursor_expiry_remains_a_typed_retryable_gateway_error() {
        let rejected = crate::peer::ClientReadRejected::new(
            "page-cursor-expired",
            StatusCode::GONE,
            "the owner page expired",
        );
        let error = remote_read_error("host/owner", rejected.into());
        assert_eq!(error.status, StatusCode::GONE);
        assert_eq!(error.code, "page-cursor-expired");
        let rejected = crate::peer::ClientReadRejected::new(
            "validation-failed",
            StatusCode::UNPROCESSABLE_ENTITY,
            "remote terminal input is invalid",
        );
        let terminal_error = remote_read_error("host/owner", rejected.into());
        assert_eq!(terminal_error.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(terminal_error.code, "validation-failed");
        let unavailable = remote_read_error("host/owner", anyhow::anyhow!("transport down"));
        assert_eq!(unavailable.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(unavailable.code, "remote-unavailable");
        assert_eq!(unavailable.details["reason"], "transport-error");
        assert_eq!(unavailable.details["owner_host_id"], "host/owner");
    }

    #[test]
    fn an_unreachable_owner_says_why_and_how_far_the_read_got() {
        let mut rejected = crate::peer::ClientReadRejected::unreachable(
            "dial-failed",
            "owner host/owner did not answer from gateway (dial-failed after 1 attempt(s))",
        );
        rejected.details.insert(
            "attempts".into(),
            json!([{"via": "host/relay", "reason": "dial-failed", "next": [
                {"via": "host/owner", "reason": "dial-failed"}
            ]}]),
        );
        rejected.details.insert("elapsed_ms".into(), 7.into());
        let error = remote_read_error("host/owner", rejected.into());
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.code, "remote-unavailable");
        assert!(!error.message.contains("temporarily"), "{}", error.message);
        assert_eq!(error.details["reason"], "dial-failed");
        assert_eq!(error.details["hops"], 2);
        assert_eq!(error.details["elapsed_ms"], 7);
        assert_eq!(error.details["owner_host_id"], "host/owner");

        let none = remote_unavailable("host/owner");
        assert_eq!(none.code, "remote-unavailable");
        assert_eq!(none.details["reason"], "no-route");
        assert_eq!(none.details["hops"], 0);
        assert!(none.message.starts_with("no route to owner host/owner"));
    }

    #[test]
    fn remote_timeline_cursor_keeps_its_owner_snapshot_out_of_gateway_admission() {
        let root = tempfile::tempdir().unwrap();
        let gateway = state(root.path());
        let local = new_client_snapshot(&gateway);
        let mut owner = local.clone();
        owner.id = "snapshot/owner/9000/ahead".into();
        owner.host_id = "host/owner".into();
        owner.store_index = 9000;

        let admitted = client_request_snapshot(&gateway, Some(owner));
        assert_eq!(admitted.id, local.id);
        assert_eq!(admitted.store_index, local.store_index);
        let admitted_local = client_request_snapshot(&gateway, Some(local.clone()));
        assert_eq!(admitted_local.id, local.id);
        assert_eq!(admitted_local.store_index, local.store_index);
    }

    #[tokio::test]
    async fn slow_request_does_not_change_the_graph() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let before = state.store.index().unwrap();
        record_request_latency(
            "/v1/client/agents",
            "/v1/client/agents",
            "stui",
            Instant::now() - Duration::from_secs(2),
        );
        // The old implementation spawned a blocking write, so give that write
        // time to finish before proving the request caused no graph change.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(state.store.index().unwrap(), before);
        assert!(
            state
                .store
                .latest_claim("daemon/node", Some("daemon.diagnostic"))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn busy_agent_projection_does_not_block_other_async_requests() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let snapshot = new_client_snapshot(&state);
        let store = state.store.clone();
        let (ready_send, ready_recv) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            store.hold_read_connections_for_test(|| {
                ready_send.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(300));
            });
        });
        ready_recv.recv().unwrap();
        let handler = tokio::spawn(client_agents(
            State(state),
            Extension(snapshot),
            Query(ClientListQuery::default()),
        ));
        let started = Instant::now();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(started.elapsed() < Duration::from_millis(150));
        holder.join().unwrap();
        let _ = handler.await.unwrap().unwrap();
    }

    #[test]
    fn repeated_agent_list_does_not_wait_for_busy_read_connections() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let index = state.store.index().unwrap();
        client_agent_resources(&state.store, false, "first", index).unwrap();
        let index = state
            .store
            .append_claim(&ClaimInput {
                subject: "daemon/node".into(),
                kind: "daemon.diagnostic".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("severity".into(), Value::String("error".into())),
                    ("code".into(), Value::String("slow-request".into())),
                    (
                        "reason".into(),
                        Value::String("a request exceeded one second".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap()
            .store_index;
        client_agent_resources(&state.store, false, "after diagnostic", index).unwrap();
        let (ready_send, ready_recv) = std::sync::mpsc::channel();
        let (release_send, release_recv) = std::sync::mpsc::channel();
        let holder = state.store.clone();
        let held = std::thread::spawn(move || {
            holder.hold_read_connections_for_test(|| {
                ready_send.send(()).unwrap();
                release_recv.recv().unwrap();
            });
        });
        ready_recv.recv().unwrap();
        let store = state.store.clone();
        let (result_send, result_recv) = std::sync::mpsc::channel();
        let read = std::thread::spawn(move || {
            result_send
                .send(client_agent_resources(&store, false, "second", index))
                .unwrap();
        });
        let result = result_recv.recv_timeout(Duration::from_millis(250));
        release_send.send(()).unwrap();
        held.join().unwrap();
        read.join().unwrap();
        assert!(result.unwrap().unwrap().is_empty());
    }

    #[test]
    fn message_delivery_distinguishes_pending_acceptance_and_recipient_read() {
        let to = "agent/remote/message-delivery-test";
        assert_eq!(
            message_delivery_value(to, "sent", 1_000, 2_000)["state"],
            "pending"
        );
        let late = message_delivery_value(to, "staged", 1_000, 12_000);
        assert_eq!(late["state"], "waiting");
        assert_eq!(late["phase"], "staged");
        assert_eq!(late["recipient_delivery"], Value::Null);
        let accepted = message_delivery_value(to, "delivered", 1_000, 12_000);
        assert_eq!(accepted["state"], "waiting");
        assert!(
            accepted["reason"]
                .as_str()
                .unwrap()
                .contains("waiting for recipient read")
        );
        assert_eq!(
            message_delivery_value(to, "read", 1_000, 12_000)["state"],
            "read"
        );
    }

    #[test]
    fn waiting_native_seats_retain_delivery_presence() {
        let recipient = "agent/eval/waiting-delivery-presence";
        delivery_presence::record_legacy(recipient, "omp-channel", std::process::id());
        for harness in [
            "ready",
            "working",
            "idle",
            "blocked",
            "indeterminate",
            "unauthenticated",
        ] {
            let mut item = json!({
                "id": recipient, "driver": "omp", "host_id": "local",
                "state": "waiting", "harness_state": harness,
                "blocked_on": "human", "ask": "approval",
            });
            overlay_delivery_presence(&mut item, "local");
            assert_eq!(item["delivery"]["state"], "legacy", "{harness}: {item}");
            assert_eq!(item["state"], "waiting");
            assert_eq!(item["blocked_on"], "human");
            assert_eq!(item["ask"], "approval");
        }
        for (state, host, driver) in [
            ("stopped", "local", "omp"),
            ("failed", "local", "omp"),
            ("desired", "local", "omp"),
            ("waiting", "remote", "omp"),
            ("waiting", "local", "shell"),
        ] {
            let mut item = json!({
                "id": recipient, "driver": driver, "host_id": host,
                "state": state, "harness_state": "working",
            });
            overlay_delivery_presence(&mut item, "local");
            assert!(item.get("delivery").is_none(), "{item}");
            assert_eq!(item["state"], state);
        }
    }

    #[test]
    fn mailbox_reads_do_not_renew_a_native_delivery_beat() {
        let recipient = "agent/eval/ordinary-mailbox-read";
        record_legacy_poll(None, Some(recipient), false);
        assert!(delivery_presence::known(recipient).is_none());
        let peer = NativeDeliveryPeer {
            agent: recipient.into(),
            transport: "omp-channel",
            pid: 37,
            archives_inbox: false,
        };
        record_legacy_poll(Some(&peer), Some("agent/eval/other-mailbox"), false);
        record_legacy_poll(Some(&peer), Some(recipient), true);
        assert!(delivery_presence::known(recipient).is_none());
        record_legacy_poll(Some(&peer), Some(recipient), false);
        assert_eq!(delivery_presence::known(recipient).unwrap().state, "legacy");
    }

    #[test]
    fn native_title_subscriptions_recognize_the_outer_pi_and_omp_drivers() {
        for driver in ["pi", "omp"] {
            let args = vec!["st".into(), "driver".into(), driver.into()];
            let env = vec!["ST_AGENT=agent/eval.worker".into()];
            let peer = native_delivery_identity(37, &args, &env).expect("native title owner");
            assert_eq!(peer.agent, "agent/eval.worker");
            assert_eq!(peer.transport, format!("{driver}-channel"));
            assert!(!peer.archives_inbox);
            assert!(native_delivery_identity(37, &args, &[]).is_none(), "argv alone cannot authorize a seat");
        }
    }

    #[test]
    fn legacy_claude_mcp_is_a_delivery_peer_but_ordinary_mailbox_queries_are_not() {
        let env = vec!["ST_AGENT=agent/example/legacy".into()];
        let args = ["st3", "driver", "claude-mcp"].map(str::to_owned);
        let peer = native_delivery_identity(37, &args, &env).unwrap();
        assert_eq!(peer.transport, "claude-channel");
        assert_eq!(peer.agent, "agent/example/legacy");
        let query = ["st3", "conversations", "ls"].map(str::to_owned);
        assert!(native_delivery_identity(37, &query, &env).is_none());
        assert!(native_delivery_identity(37, &args, &[]).is_none());
    }

    #[tokio::test]
    async fn legacy_native_drivers_renew_delivery_when_their_projection_includes_closed_mail() {
        // Before a63824ef, native drivers polled include_closed=true to archive
        // inbox files. This is the real pre-September-25 request shape, rather
        // than the newer pre-reexec channel's active-mail-only poll.
        let root = tempfile::tempdir().unwrap();
        let app = router(state(root.path()));
        for driver in ["claude", "codex", "opencode"] {
            for endpoint in ["messages/page", "messages"] {
                let recipient = format!(
                    "agent/example/closed-{driver}-{}",
                    endpoint.replace('/', "-")
                );
                let query = format!("/v1/{endpoint}?to={recipient}&include_closed=true&limit=100");
                let (status, _) = get_request(app.clone(), &query).await;
                assert_eq!(status, StatusCode::OK);
                assert!(
                    delivery_presence::known(&recipient).is_none(),
                    "ordinary history query renewed a beat"
                );

                let args = ["st3", "driver", driver].map(str::to_owned);
                let env = vec![format!("ST_AGENT={recipient}")];
                let peer = native_delivery_identity(37, &args, &env).unwrap();
                let other = format!("{recipient}-other");
                let other_query =
                    format!("/v1/{endpoint}?to={other}&include_closed=true&limit=100");
                let (status, _) =
                    get_request(app.clone().layer(Extension(peer.clone())), &other_query).await;
                assert_eq!(status, StatusCode::OK);
                assert!(delivery_presence::known(&other).is_none());
                assert!(delivery_presence::known(&recipient).is_none());
                let (status, _) = get_request(app.clone().layer(Extension(peer)), &query).await;
                assert_eq!(status, StatusCode::OK);
                let assessment = delivery_presence::known(&recipient)
                    .expect("native projection did not renew its beat");
                assert_eq!(assessment.state, "legacy");
                assert_eq!(assessment.polled_seconds_ago, Some(0));
            }
        }
    }

    #[tokio::test]
    async fn channel_history_queries_do_not_renew_delivery() {
        let root = tempfile::tempdir().unwrap();
        let app = router(state(root.path()));
        for driver in ["claude-mcp", "pi-channel", "omp-channel"] {
            for endpoint in ["messages/page", "messages"] {
                let recipient = format!(
                    "agent/example/history-{driver}-{}",
                    endpoint.replace('/', "-")
                );
                let args = ["st3", "driver", driver].map(str::to_owned);
                let env = vec![format!("ST_AGENT={recipient}")];
                let peer = native_delivery_identity(37, &args, &env).unwrap();
                let query = format!("/v1/{endpoint}?to={recipient}&include_closed=true&limit=100");
                let (status, _) = get_request(app.clone().layer(Extension(peer)), &query).await;
                assert_eq!(status, StatusCode::OK);
                assert!(
                    delivery_presence::known(&recipient).is_none(),
                    "channel history query renewed a beat"
                );
            }
        }
    }

    #[test]
    fn doctor_unread_counts_exclude_historical_recipients_and_people() {
        let store = Store::open_memory("amber").unwrap();
        for (id, recipient) in [
            ("current", "agent/example/current"),
            ("retired", "agent/example/retired"),
            ("person", "person/eval"),
        ] {
            store
                .append_claim(&ClaimInput {
                    subject: format!("message/{id}"),
                    kind: "message.sent".into(),
                    actor: Some("person/operator".into()),
                    fields: BTreeMap::from([
                        ("from".into(), json!("person/operator")),
                        ("to".into(), json!(recipient)),
                        ("status".into(), json!("sent")),
                        ("content".into(), json!("probe")),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: Some(id.into()),
                })
                .unwrap();
        }
        let current = BTreeSet::from(["agent/example/current"]);
        let messages = store.operational_messages(None, false).unwrap();
        assert_eq!(
            unread_current_seat_counts(&store, &current, &messages, client_now_ms()).unwrap(),
            (0, 0)
        );
        assert_eq!(
            unread_current_seat_counts(&store, &current, &messages, client_now_ms() + 20_000)
                .unwrap(),
            (1, 0)
        );
        assert_eq!(
            store.messages(None, false).unwrap().len(),
            3,
            "inspection does not erase historical messages"
        );
    }

    pub(super) fn state(root: &Path) -> AppState {
        AppState {
            store: Arc::new(Store::open_memory("node").unwrap()),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: "node".into(),
            state_dir: root.to_path_buf(),
            pty_root: root.join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: PlannerSpec::default(),
        }
    }

    #[test]
    fn doctor_distinguishes_no_fleet_from_explicitly_leaving_a_fleet() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let replication_check = || {
            doctor_report(&state)
                .unwrap()
                .0
                .checks
                .into_iter()
                .find(|check| check.name == "replication")
                .unwrap()
        };
        let check = replication_check();
        assert_eq!(check.status, "pass");
        assert!(check.message.contains("no fleet is configured"));
        assert!(check.message.contains("st fleet create"));
        assert!(check.message.contains("st fleet join"));
        assert!(!check.message.contains("intentionally"));

        fs::write(
            root.path().join("left-fleet.json"),
            r#"{"fleet_id":"previous-fleet","offline":true,"confirmed_by":null}"#,
        )
        .unwrap();
        let check = replication_check();
        assert_eq!(check.status, "pass");
        assert!(check.message.contains("intentionally local-only"));
        assert!(check.message.contains("after leaving its fleet"));
    }

    #[tokio::test]
    async fn rules_audit_an_agents_write_then_refuse_it_once_enforced() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let app = router(state.clone());
        for (name, rule) in crate::rules::lockdown(&[]) {
            let (status, body) = json_request(
                app.clone(),
                "/v1/rules/set",
                json!({"actor": "person/ada", "name": name, "rule": rule}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let (_, rules) = get_request(app.clone(), "/v1/rules").await;
        assert_eq!(rules.as_array().unwrap().len(), 3, "{rules}");
        assert!(
            rules
                .as_array()
                .unwrap()
                .iter()
                .all(|rule| rule["mode"] == "audit"),
            "{rules}"
        );
        let publish = |name: &str| {
            let request = Request::builder()
                .method("POST")
                .uri("/v1/documents")
                .header("content-type", "application/json")
                .extension(BoundAgent("agent/team/web/reviewer".into()))
                .body(Body::from(
                    serde_json::to_vec(&DocumentPutRequest {
                        name: name.into(),
                        bytes: b"notes".to_vec(),
                        expected_document: None,
                        idempotency_key: format!("put:{name}"),
                    })
                    .unwrap(),
                ))
                .unwrap();
            let app = app.clone();
            async move {
                let response = app.oneshot(request).await.unwrap();
                let status = response.status();
                let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                (status, serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null))
            }
        };
        // Inside its namespace nothing is logged; outside, the write proceeds and is logged.
        assert_eq!(publish("doc/team/web/plan").await.0, StatusCode::OK);
        assert_eq!(publish("doc/team/api/plan").await.0, StatusCode::OK);
        let (_, audits) = get_request(app.clone(), "/v1/rules/audit").await;
        let audits = audits.as_array().unwrap();
        assert_eq!(audits.len(), 1, "{audits:?}");
        assert_eq!(audits[0]["rule"], "agents-publish-in-namespace");
        assert_eq!(audits[0]["actor"], "agent/team/web/reviewer");
        assert_eq!(audits[0]["action"], "doc.bound");
        assert_eq!(audits[0]["target"], "doc/team/api/plan");

        // Enforced, the same write is refused with a typed reason and nothing is stored.
        let mut rule = crate::rules::lockdown(&[])
            .into_iter()
            .find(|(name, _)| *name == "agents-publish-in-namespace")
            .unwrap()
            .1;
        rule.mode = smallclaims::rules::Mode::Enforce;
        let (status, body) = json_request(
            app.clone(),
            "/v1/rules/set",
            json!({"actor": "person/ada", "name": "agents-publish-in-namespace", "rule": rule}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, refused) = publish("doc/team/api/later").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
        assert_eq!(refused["code"], "rule-denied", "{refused}");
        assert!(state.store.claims_for("doc/team/api/later", None).unwrap().is_empty());
        assert_eq!(publish("doc/team/web/later").await.0, StatusCode::OK);

        // Only a person sets rules.
        let (status, refused) = json_request(
            app.clone(),
            "/v1/rules/set",
            json!({"actor": "agent/team/web/reviewer", "name": "agents-publish-in-namespace", "rule": rule}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    }

    #[tokio::test]
    async fn isolated_daemon_exposes_only_rollups_in_the_fleet_usage_endpoint() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let app = router(state.clone());
        let subject = "agent/example.worker";
        let request = ClaimInput {
            subject: subject.into(),
            kind: "harness.timeline".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("operation".into(), Value::String("append".into())),
                ("entry_id".into(), Value::String("response-a".into())),
                (
                    "source_id".into(),
                    Value::String("response-a-source".into()),
                ),
                ("sequence".into(), Value::from(1)),
                ("revision".into(), Value::from(1)),
                ("role".into(), Value::String("system".into())),
                ("entry_type".into(), Value::String("usage".into())),
                ("final".into(), Value::Bool(true)),
                ("driver".into(), Value::String("claude".into())),
                ("incarnation_id".into(), Value::String("inc-one".into())),
                (
                    "body".into(),
                    json!({"semantics":"response","turn_id":"turn-a","model":"claude-example",
                    "input_tokens":4,"output_tokens":2,"cache_write_tokens":3,"cached_tokens":20,"total_tokens":29}),
                ),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("usage-api-response-a".into()),
        };
        let (status, observation) = json_request(
            app.clone(),
            "/v1/claims",
            serde_json::to_value(&request).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{observation}");
        assert!(
            observation["id"]
                .as_str()
                .unwrap()
                .starts_with("local-observation/")
        );
        let (status, repeated) = json_request(
            app.clone(),
            "/v1/claims",
            serde_json::to_value(&request).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{repeated}");
        assert_eq!(repeated["id"], observation["id"]);
        let (_, report) = get_request(
            app.clone(),
            &format!("/v1/usage?since_ms=0&until_ms={}", client_now_ms() + 60_000),
        )
        .await;
        assert_eq!(report["rows"].as_array().unwrap().len(), 1);
        assert_eq!(report["rows"][0]["total_tokens"], 29);
        assert_eq!(report["rows"][0]["cache_write_tokens"], 3);
        assert_eq!(report["rows"][0]["cached_tokens"], 20);
        // Clients read the same rows, without the identities st does not know.
        let (status, period) = get_request(
            app.clone(),
            &format!(
                "/v1/client/usage?since_ms=0&until_ms={}",
                client_now_ms() + 60_000
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{period}");
        let period = period.get("value").unwrap_or(&period);
        let row = &period["rows"][0];
        assert_eq!(period["rows"].as_array().unwrap().len(), 1, "{period}");
        assert_eq!(row["agent"], subject);
        assert_eq!(row["total_tokens"], 29);
        assert!(
            row.get("mission_run").is_none() && row.get("step").is_none(),
            "{row}"
        );
        let typed: st3_client::UsagePeriod = serde_json::from_value(period.clone()).unwrap();
        assert_eq!(typed.rows[0].model.as_deref(), Some("claude-example"));
        assert!(
            typed.limits.is_empty(),
            "no harness has reported limits here"
        );
        let backwards = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/client/usage?since_ms=2&until_ms=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(backwards.status(), StatusCode::UNPROCESSABLE_ENTITY);
        // The response timeline and its latest-retention rollup both stay local.
        assert_eq!(
            state.store.local_observations_after(0, 10).unwrap().len(),
            2
        );
        assert_eq!(
            state
                .store
                .claims_for(subject, Some("harness.usage"))
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_local_only_harness_observation_wakes_only_the_client_feed() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let wake_file = root.path().join("replication.wake");
        let mut client_feed = state.event_notify.subscribe();
        let subject = "agent/node.worker";
        let observed = |state_name: &str, observed_at_ms: u64| ClaimInput {
            subject: subject.into(),
            kind: "harness.observed".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("state".into(), Value::String(state_name.into())),
                ("incarnation_id".into(), Value::String("inc-1".into())),
                ("observed_at_ms".into(), Value::from(observed_at_ms)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("wake-{state_name}-{observed_at_ms}")),
        };
        let usage = |tokens: u64| ClaimInput {
            subject: subject.into(),
            kind: "harness.usage".into(),
            actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("driver".into(), Value::String("codex".into())),
                ("incarnation_id".into(), Value::String("inc-1".into())),
                (
                    "semantics".into(),
                    Value::String("context_occupancy".into()),
                ),
                ("context_used_tokens".into(), Value::from(tokens)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(format!("wake-usage-{tokens}")),
        };
        let post = |input: ClaimInput| post_claim(State(state.clone()), Json(input));
        let reconciler_woke = || async {
            tokio::time::timeout(Duration::from_millis(20), state.notify.notified())
                .await
                .is_ok()
        };
        let mut client_feed_woke = || {
            let changed = client_feed.has_changed().unwrap();
            client_feed.borrow_and_update();
            changed
        };

        let change = post(observed("working", 1)).await.unwrap().0;
        assert!(crate::store::local_observation_position(&change).is_none());
        assert!(reconciler_woke().await);
        assert!(wake_file.exists());
        assert!(client_feed_woke());
        fs::remove_file(&wake_file).unwrap();

        let heartbeat = post(observed("working", 300_001)).await.unwrap().0;
        assert!(crate::store::local_observation_position(&heartbeat).is_some());
        assert!(!reconciler_woke().await, "a heartbeat does not reconcile");
        assert!(!wake_file.exists(), "a heartbeat does not wake replication");
        assert!(client_feed_woke(), "clients still see the observation");

        let first_usage = post(usage(10)).await.unwrap().0;
        assert!(crate::store::local_observation_position(&first_usage).is_none());
        assert!(wake_file.exists(), "replicated usage wakes replication");
        assert!(!reconciler_woke().await, "usage never reconciles");
        assert!(client_feed_woke());
        fs::remove_file(&wake_file).unwrap();

        let throttled = post(usage(20)).await.unwrap().0;
        assert!(crate::store::local_observation_position(&throttled).is_some());
        assert!(
            !wake_file.exists(),
            "usage kept local does not wake replication"
        );
        assert!(!reconciler_woke().await);
        assert!(client_feed_woke());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_admission_does_not_block_the_async_runtime_when_store_reads_wait() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        state
            .store
            .put_document("doc/admission-probe", b"probe", &None, "admission-probe")
            .unwrap();
        let store = state.store.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            store.hold_read_connections_for_test(|| {
                ready_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(500));
            });
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let app = router(state);
        let requests: Vec<_> = (0..8)
            .map(|_| {
                let app = app.clone();
                tokio::spawn(async move { get_request(app, "/v1/client/capabilities").await })
            })
            .collect();
        let started = std::time::Instant::now();
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "a blocked SQLite read must not pin the async request acceptor (elapsed {:?})",
            started.elapsed()
        );

        for request in requests {
            let (status, _) = request.await.unwrap();
            assert_eq!(status, StatusCode::OK);
        }
        holder.join().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_terminal_attach_does_not_delay_an_independent_request() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            store.hold_read_connections_for_test(|| {
                ready_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(500));
            });
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let app = router(state);
        let started = Instant::now();
        let attach = tokio::spawn(json_request(
            app.clone(),
            "/v1/sessions/attach/agent/probe",
            json!({}),
        ));
        tokio::task::yield_now().await;
        let (status, _) = get_request(app, "/v1/health").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "health waited {:?} behind a blocked terminal attach",
            started.elapsed()
        );

        holder.join().unwrap();
        let (status, _) = attach.await.unwrap();
        assert!(!status.is_success());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn control_reads_answer_during_a_long_write_and_busy_query_pool() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        // A disk-backed store uses WAL like the daemon. Shared-cache memory stores
        // deliberately make readers wait for an uncommitted writer.
        state.store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "node").unwrap());
        let store = state.store.clone();
        let (read_ready_tx, read_ready_rx) = std::sync::mpsc::channel();
        let read_holder = std::thread::spawn(move || {
            store.hold_read_connections_for_test(|| {
                read_ready_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_secs(30));
            });
        });
        read_ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let store = state.store.clone();
        let (write_ready_tx, write_ready_rx) = std::sync::mpsc::channel();
        let write_holder = std::thread::spawn(move || {
            store.hold_write_transaction_for_test(|| {
                write_ready_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_secs(30));
            });
        });
        write_ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let app = router(state);
        for path in ["/v1/status", "/v1/client/agents", "/v1/client/machines"] {
            let started = Instant::now();
            let response =
                tokio::time::timeout(Duration::from_millis(100), get_request(app.clone(), path))
                    .await;
            assert!(
                response.is_ok(),
                "{path} waited {:?} behind a long write or query",
                started.elapsed()
            );
            assert_eq!(response.unwrap().0, StatusCode::OK);
        }
        read_holder.join().unwrap();
        write_holder.join().unwrap();
    }

    fn probe_claim(key: &str, state: &str) -> ClaimInput {
        ClaimInput {
            subject: "agent/probe".into(),
            kind: "harness.observed".into(),
            actor: Some("agent/probe".into()),
            fields: BTreeMap::from([("state".into(), Value::String(state.into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some(key.into()),
        }
    }

    /// A page whose read a commit lands in, as another thread commits it midway.
    async fn page_read_across_a_commit(
        State(state): State<AppState>,
        Extension(snapshot): Extension<ClientSnapshot>,
        Query(query): Query<ClientListQuery>,
    ) -> Result<ClientPageResponse, ApiError> {
        client_snapshot_page(&state, snapshot, "probes", &query, |state, snapshot| {
            let newest = |store: &Store| -> anyhow::Result<u64> {
                Ok(store
                    .claims_page(None, None, 0, None, true, 1)?
                    .claims
                    .first()
                    .map_or(0, |claim| claim.store_index))
            };
            let before = newest(&state.store)?;
            let store = state.store.clone();
            std::thread::spawn(move || store.append_claim(&probe_claim("during", "working")))
                .join()
                .unwrap()?;
            anyhow::ensure!(state.store.index()? > snapshot.store_index);
            Ok(vec![json!({
                "id": "probe/page",
                "read_at": snapshot.store_index,
                "before": before,
                "after": newest(&state.store)?,
            })])
        })
        .await
    }

    /// A first page reads inside one snapshot: a commit that lands while it reads neither tears
    /// it nor refuses it, and the envelope names the snapshot the page was read in.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_first_page_answers_from_its_snapshot_when_a_commit_lands_during_it() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        // A file store, as the daemon runs: in shared-cache memory a reader locks writers out.
        state.store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "node").unwrap());
        state
            .store
            .append_claim(&probe_claim("first", "idle"))
            .unwrap();
        let admitted_at = state.store.index().unwrap();
        // A page read at the store's first claim names that snapshot, not the newer one the
        // request was admitted at.
        async fn page_read_earlier(
            State(state): State<AppState>,
            Query(query): Query<ClientListQuery>,
        ) -> Result<ClientPageResponse, ApiError> {
            let snapshot = client_snapshot_at(&state, 1);
            let page = client_page_read(&state, &snapshot, "earlier", Vec::new(), &query, true)?;
            Ok((Extension(snapshot), Json(page)))
        }
        let app = Router::new()
            .route("/v1/client/probes", get(page_read_across_a_commit))
            .route("/v1/client/earlier", get(page_read_earlier))
            .layer(from_fn_with_state(
                (state.clone(), ClientTransportBoundary::Unix),
                response_envelope,
            ))
            .with_state(state.clone());
        let envelope = |path: &'static str| {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                let status = response.status();
                let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                (status, serde_json::from_slice::<Value>(&bytes).unwrap())
            }
        };
        let (status, page) = envelope("/v1/client/probes").await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let item = &page["value"]["items"][0];
        assert_eq!(item["read_at"], admitted_at);
        assert_eq!(page["snapshot"]["store_index"], admitted_at);
        // The page saw neither the claim committed while it read nor anything after its snapshot.
        assert_eq!(item["before"], admitted_at);
        assert_eq!(item["after"], admitted_at);
        assert!(state.store.index().unwrap() > admitted_at);

        state
            .store
            .append_claim(&probe_claim("second", "idle"))
            .unwrap();
        let (status, page) = envelope("/v1/client/earlier").await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["snapshot"], json!(client_snapshot_at(&state, 1)));
    }

    /// A seat's mailbox poll, one subject's status, a client's admission, and replication and
    /// fleet status each open a read connection of their own. They answer while the writer is
    /// held, more reads than the pool keeps idle are held, and long snapshots run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn small_reads_answer_while_the_writer_and_long_reads_are_held() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        state.store = Arc::new(Store::open(&root.path().join("claims.sqlite3"), "node").unwrap());
        // A local batch that no exchange has sealed yet: sealing it would need the writer.
        state
            .store
            .append_claim(&ClaimInput {
                subject: "agent/probe".into(),
                kind: "harness.observed".into(),
                actor: Some("agent/probe".into()),
                fields: BTreeMap::from([("state".into(), Value::String("idle".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let hold = Duration::from_secs(5);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let mut holders = Vec::new();
        let store = state.store.clone();
        let ready = ready_tx.clone();
        holders.push(std::thread::spawn(move || {
            store.hold_write_transaction_for_test(|| {
                ready.send(()).unwrap();
                std::thread::sleep(hold);
            });
        }));
        let store = state.store.clone();
        let ready = ready_tx.clone();
        holders.push(std::thread::spawn(move || {
            store.hold_read_connections_for_test(|| {
                ready.send(()).unwrap();
                std::thread::sleep(hold);
            });
        }));
        for _ in 0..8 {
            let (store, ready) = (state.store.clone(), ready_tx.clone());
            holders.push(std::thread::spawn(move || {
                store
                    .read_snapshot(|_| {
                        ready.send(()).unwrap();
                        std::thread::sleep(hold);
                        Ok(())
                    })
                    .unwrap();
            }));
        }
        for _ in 0..holders.len() {
            ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }

        // As the daemon does when it starts, so no read makes the first diagnostic report.
        start_operation_report(&state);
        let app = router(state);
        for path in [
            "/v1/messages/page?include_closed=false&limit=100&to=agent%2Fprobe",
            "/v1/status?subject=agent%2Fprobe",
            "/v1/client/now",
            "/v1/replication/status",
            "/v1/internal/fleet/membership",
            "/v1/client/operations",
        ] {
            let started = Instant::now();
            let response =
                tokio::time::timeout(Duration::from_millis(250), get_request(app.clone(), path))
                    .await;
            assert!(
                response.is_ok(),
                "{path} waited {:?} behind the writer or other reads",
                started.elapsed()
            );
            assert_eq!(response.unwrap().0, StatusCode::OK, "{path}");
        }
        for holder in holders {
            holder.join().unwrap();
        }
    }

    #[test]
    fn health_response_does_not_queue_for_a_blocking_thread() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

            let root = tempfile::tempdir().unwrap();
            let app = router(state(root.path()));
            let started = Instant::now();
            let response =
                tokio::time::timeout(Duration::from_millis(250), get_request(app, "/v1/health"))
                    .await;
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            assert!(
                response.is_ok(),
                "health waited {:?} for the busy blocking pool",
                started.elapsed()
            );
            assert_eq!(response.unwrap().0, StatusCode::OK);
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn isolated_daemon_answers_health_under_stalled_attaches() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let socket = root.path().join("st3.sock");
        let server_socket = socket.clone();
        let server_state = state.clone();
        let server =
            tokio::spawn(async move { serve_unix(&server_socket, router(server_state)).await });
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let store = state.store.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            store.hold_read_connections_for_test(|| {
                ready_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(1_200));
            });
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let client = crate::client::Client::new(crate::client::Endpoint::Unix(socket));
        let pending = (0..8)
            .map(|_| {
                let client = client.clone();
                tokio::spawn(async move {
                    let started = Instant::now();
                    let _: anyhow::Result<Value> = client
                        .post("/v1/sessions/attach/agent/probe", &json!({}))
                        .await;
                    started.elapsed()
                })
            })
            .collect::<Vec<_>>();
        tokio::time::sleep(Duration::from_millis(40)).await;
        let mut latencies = Vec::new();
        for _ in 0..30 {
            let started = Instant::now();
            let _: Value = client.get("/v1/health").await.unwrap();
            latencies.push(started.elapsed());
        }
        latencies.sort();
        assert!(
            latencies[29] < Duration::from_secs(1),
            "health p99 was {:?}",
            latencies[29]
        );
        let mut attach_latencies = Vec::new();
        for request in pending {
            attach_latencies.push(request.await.unwrap());
        }
        assert!(
            attach_latencies
                .iter()
                .all(|latency| *latency < Duration::from_secs(1)),
            "terminal attaches waited behind background reads: {attach_latencies:?}"
        );
        holder.join().unwrap();
        let doctor: DoctorReport = client.get("/v1/doctor").await.unwrap();
        assert!(doctor.checks.iter().any(|check| {
            check.name.starts_with("request-latency/") && check.message.contains("p99")
        }));
        let (status, latency) = get_request(router(state), "/v1/client/request-latency").await;
        assert_eq!(status, StatusCode::OK);
        assert!(latency["routes"].as_array().unwrap().iter().any(|route| {
            route["route"] == "/v1/health" && route["count"].as_u64().unwrap_or_default() >= 30
        }));
        server.abort();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "current_thread")]
    async fn slow_peer_identity_lookup_does_not_stop_existing_api_connections() {
        use http_body_util::{BodyExt as _, Empty};
        use hyper::client::conn::http1::SendRequest;

        fn slow_ancestor(_pid: u32) -> Option<String> {
            std::thread::sleep(Duration::from_millis(500));
            None
        }

        async fn health(sender: &mut SendRequest<Empty<axum::body::Bytes>>) {
            let response = sender
                .send_request(
                    Request::builder()
                        .uri("/v1/health")
                        .header("host", "local")
                        .body(Empty::new())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            response.into_body().collect().await.unwrap();
        }

        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("st3.sock");
        let server_socket = socket.clone();
        let app = router(state(root.path()));
        let server = tokio::spawn(async move {
            serve_unix_with_ancestor(&server_socket, None, app, true, slow_ancestor).await
        });
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        health(&mut sender).await;

        let _slow_peer = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let started = Instant::now();
        tokio::time::sleep(Duration::from_millis(20)).await;
        health(&mut sender).await;
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "an unrelated identity lookup stalled the API for {:?}",
            started.elapsed()
        );
        server.abort();
    }

    #[test]
    fn cached_page_cursor_survives_graph_change_and_stays_person_scoped() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let snapshot = new_client_snapshot(&state);
        let query = ClientListQuery {
            limit: Some(2),
            person: Some("person/alex".into()),
            ..ClientListQuery::default()
        };
        let first = client_page(
            &state,
            &snapshot,
            "messages",
            vec![
                json!({"id":"message/1"}),
                json!({"id":"message/2"}),
                json!({"id":"message/3"}),
            ],
            &query,
        )
        .unwrap();
        let cursor = first
            .page
            .next_cursor
            .expect("three rows require a second page");
        state
            .store
            .append_claim(&crate::model::ClaimInput {
                subject: "custom/client/pairing-page-churn".into(),
                kind: "custom.client.pairing-completed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("credential_hash".into(), Value::String("unused".into())),
                    (
                        "session_actor".into(),
                        Value::String("client/unused".into()),
                    ),
                    ("person_id".into(), Value::String("person/alex".into())),
                    ("scopes".into(), json!(["read.projections"])),
                    ("expires_at_unix_ms".into(), json!(u64::MAX)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert_ne!(
            new_client_snapshot(&state).store_index,
            snapshot.store_index
        );
        let continuation = ClientListQuery {
            cursor: Some(cursor),
            ..query
        };
        let second = client_page(&state, &snapshot, "messages", Vec::new(), &continuation).unwrap();
        assert_eq!(second.items[0]["id"], "message/3");
        let other_person = ClientListQuery {
            person: Some("person/other".into()),
            ..continuation
        };
        assert_eq!(
            client_page(&state, &snapshot, "messages", Vec::new(), &other_person)
                .unwrap_err()
                .code,
            "page-cursor-expired"
        );
    }

    #[test]
    fn managed_session_cache_tracks_a_new_runtime_incarnation() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let subject = "agent/cache-test";
        for incarnation in ["first", "second"] {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "runtime.observed".into(),
                    actor: Some(subject.into()),
                    fields: BTreeMap::from([
                        ("status".into(), Value::String("running".into())),
                        ("runtime_id".into(), Value::String("cache-runtime".into())),
                        ("incarnation_id".into(), Value::String(incarnation.into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            let snapshot = new_client_snapshot(&state);
            let sessions = client_session_resources(
                &state.store,
                false,
                &snapshot.created_at,
                snapshot.store_index,
                None,
                false,
            )
            .unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0]["revision"], incarnation);
        }
    }

    #[tokio::test]
    async fn document_history_prefix_filters_before_limit_and_has_a_next_page() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        for index in 0..202 {
            let name = format!("doc/early/{index:03}");
            state
                .store
                .put_document(&name, b"early", &None, &format!("early-{index}"))
                .unwrap();
        }
        let mut head = None;
        for index in 0..2 {
            let version = state
                .store
                .put_document(
                    "doc/late/target",
                    format!("version-{index}").as_bytes(),
                    &head,
                    &format!("late-{index}"),
                )
                .unwrap();
            head = Some(version.binding_claim_id);
        }
        let app = router(state);
        let (status, first) = get_request(
            app.clone(),
            "/v1/documents?prefix=doc%2Flate&history=true&limit=1",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["items"][0]["name"], "doc/late/target");
        assert_eq!(first["has_more"], true);
        assert!(first["next_cursor"].is_string(), "{first}");
        let cursor = first["next_cursor"].as_str().unwrap();
        let (status, second) = get_request(
            app,
            &format!(
                "/v1/documents?prefix=doc%2Flate&history=true&limit=1&cursor={}",
                urlencoding::encode(cursor)
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(second["items"][0]["name"], "doc/late/target");
        assert_ne!(first["items"][0]["hash"], second["items"][0]["hash"]);
        assert_eq!(second["has_more"], false);
    }

    fn observe_terminal(store: &Store, subject: &str, incarnation: &str, terminal: bool) {
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("runtime_id".into(), Value::String("worker-runtime".into())),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                    ("status".into(), Value::String("running".into())),
                    ("terminal".into(), Value::Bool(terminal)),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
    }

    /// example-linux, 2026-09-29: under CI load an attach waited behind the store writer until its
    /// WebSocket handshake gave up. Naming a local terminal must not wait for the writer.
    #[tokio::test]
    async fn a_local_terminal_is_named_by_reads_alone_while_the_writer_is_busy() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        // An isolated daemon may be started with a relative PTY root.
        state.pty_root = PathBuf::from("isolated/pty");
        observe_terminal(
            &state.store,
            "agent/worker",
            "4242:2026-09-29T08:00:00.000Z",
            true,
        );
        let store = state.store.clone();
        let index = store.index().unwrap();
        let app = router(state);
        let (held, holding) = std::sync::mpsc::channel();
        let holder_store = store.clone();
        let holder = std::thread::spawn(move || {
            holder_store.hold_writer_for_test(|| {
                held.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(1_500));
            });
        });
        holding.recv().unwrap();

        let started = std::time::Instant::now();
        let (status, terminal) = get_request(app, "/v1/sessions/local-terminal/agent/worker").await;
        let elapsed = started.elapsed();
        holder.join().unwrap();

        assert_eq!(status, StatusCode::OK, "{terminal}");
        assert!(
            elapsed < Duration::from_millis(750),
            "naming a local terminal waited {elapsed:?} for the writer"
        );
        assert_eq!(terminal["subject"], "agent/worker");
        assert_eq!(terminal["runtime_id"], "worker-runtime");
        assert_eq!(terminal["incarnation_id"], "4242:2026-09-29T08:00:00.000Z");
        assert_eq!(
            terminal["pty_root"],
            std::env::current_dir()
                .unwrap()
                .join("isolated/pty")
                .display()
                .to_string()
        );
        assert_eq!(store.index().unwrap(), index, "it must write nothing");
    }

    #[tokio::test]
    async fn a_local_terminal_keeps_the_attachment_checks() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        observe_terminal(
            &state.store,
            "agent/headless",
            "1:2026-09-29T08:00:00.000Z",
            false,
        );
        let owner = Store::open_memory("owner-node").unwrap();
        observe_terminal(&owner, "agent/remote", "2:2026-09-29T08:00:00.000Z", true);
        state
            .store
            .import_replication("owner-node", &owner.export_replication(0).unwrap())
            .unwrap();
        observe_terminal(
            &state.store,
            "agent/local",
            "3:2026-09-29T08:00:00.000Z",
            true,
        );
        // A paired client never learns a PTY path: the gateway serves only client v0.
        let (status, refused) = get_request(
            fabric_router(state.clone()),
            "/v1/sessions/local-terminal/agent/local",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
        let app = router(state);

        let (status, refused) =
            get_request(app.clone(), "/v1/sessions/local-terminal/agent/headless").await;
        assert!(status.is_client_error(), "{refused}");
        assert_eq!(refused["code"], "unsupported-capability");

        let (status, refused) =
            get_request(app.clone(), "/v1/sessions/local-terminal/agent/remote").await;
        assert!(status.is_client_error(), "{refused}");
        assert_eq!(refused["code"], "runtime-not-local");

        // An unknown subject is refused exactly as its WebSocket attachment is.
        let (status, refused) =
            get_request(app.clone(), "/v1/sessions/local-terminal/agent/missing").await;
        let (attach_status, attach_refused) =
            json_request(app, "/v1/sessions/attach/agent/missing", json!({})).await;
        assert!(status.is_client_error(), "{refused}");
        assert_eq!(status, attach_status);
        assert_eq!(refused["code"], attach_refused["code"], "{refused}");
    }

    #[tokio::test]
    async fn a_claim_waiting_for_the_store_writer_leaves_the_api_answering() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        let app = router(state);
        let (held, holding) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            store.hold_writer_for_test(|| {
                held.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(1_500));
            });
        });
        holding.recv().unwrap();

        // This test runs on one thread. A claim that waited for the writer on it would hold the
        // whole runtime, even this sleep, until the writer was released.
        let started = std::time::Instant::now();
        let claim = tokio::spawn(json_request(
            app.clone(),
            "/v1/claims",
            json!({
                "subject": "custom/acme/fact",
                "kind": "custom.acme.found",
                "fields": {},
            }),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (status, _) = get_request(app, "/v1/health").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            started.elapsed() < Duration::from_millis(750),
            "health waited {:?} behind a claim that was waiting for the writer",
            started.elapsed()
        );

        holder.join().unwrap();
        let (status, body) = claim.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    async fn json_request(app: Router, path: &str, value: Value) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&value).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let envelope: Value = serde_json::from_slice(&bytes).unwrap();
        if path.starts_with("/v1/client/") {
            assert_eq!(envelope["api_version"], "st3.client.v0");
            assert_eq!(envelope["snapshot"]["host_id"], "host/node");
            assert!(envelope["request_id"].as_str().unwrap().contains('-'));
            assert!(envelope["snapshot"]["store_index"].is_u64());
        } else {
            assert_eq!(envelope["api_version"], "st3.v1");
            assert_eq!(envelope["snapshot_host"], "node");
            assert!(envelope["request_id"].as_str().unwrap().contains('-'));
            assert!(envelope["store_index"].is_u64());
        }
        let value = if status.is_success() {
            envelope["value"].clone()
        } else {
            envelope
        };
        (status, value)
    }

    #[tokio::test]
    async fn an_exact_work_retry_survives_provider_exit_after_commit() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let response: StepRunView = serde_json::from_value(json!({
            "subject": "step-run/run/work",
            "run": "mission-run/run",
            "generation": "run-generation/run",
            "step": "work",
            "definition_hash": "definition",
            "status": "verifying",
            "attempt": 1,
            "assigned_to": "agent/node.worker",
            "agentless": false,
            "title": null,
            "worker_reported": true,
            "claimant": null,
            "claim_incarnation": null,
            "claim_expires_at_unix_ms": null,
            "execution_elapsed_ms": 1,
            "readiness_epoch": 1,
            "blocked_reason": null,
            "not_before_unix_ms": null,
            "created_at_unix_ms": 1,
            "updated_at_unix_ms": 2
        }))
        .unwrap();
        state
            .store
            .cache_idempotency_response("work-retry", &response)
            .unwrap();

        let (status, body) = json_request(
            router(state),
            "/v1/work/complete/step-run/run/work",
            serde_json::to_value(WorkRequest {
                actor: Some("agent/node.worker".into()),
                incarnation: Some("ended-incarnation".into()),
                summary: Some("complete".into()),
                reason: None,
                evidence: Vec::new(),
                idempotency_key: "work-retry".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["subject"], response.subject);
        assert_eq!(body["status"], "verifying");
    }

    async fn get_request(app: Router, path: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let envelope: Value = serde_json::from_slice(&bytes).unwrap();
        if path.starts_with("/v1/client/") {
            assert_eq!(envelope["api_version"], "st3.client.v0");
            assert_eq!(envelope["snapshot"]["host_id"], "host/node");
            assert!(envelope["request_id"].as_str().unwrap().contains('-'));
            assert!(envelope["snapshot"]["store_index"].is_u64());
        } else {
            assert_eq!(envelope["api_version"], "st3.v1");
            assert_eq!(envelope["snapshot_host"], "node");
            assert!(envelope["request_id"].as_str().unwrap().contains('-'));
            assert!(envelope["store_index"].is_u64());
        }
        let value = if status.is_success() {
            envelope["value"].clone()
        } else {
            envelope
        };
        (status, value)
    }

    #[test]
    fn replication_heartbeats_do_not_process_the_backlog() {
        assert!(!replication_receive_has_new_data(0));
        assert!(replication_receive_has_new_data(1));
    }

    #[tokio::test]
    async fn visible_change_updates_clients_and_peer_without_waking_reconciler() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let mut events = state.event_notify.subscribe();
        signal_visible_change(&state);
        events.changed().await.unwrap();
        assert_eq!(*events.borrow(), 1);
        assert!(state.state_dir.join("replication.wake").exists());
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                state.notify.notified(),
            )
            .await
            .is_err()
        );
        signal_changed(&state);
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            state.notify.notified(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn only_work_wake_messages_wake_the_reconciler() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let app = router(state.clone());
        let woke = || {
            let notify = state.notify.clone();
            async move {
                tokio::time::timeout(Duration::from_millis(50), notify.notified())
                    .await
                    .is_ok()
            }
        };
        let _ = woke().await;
        let mut events = state.event_notify.subscribe();
        let send = |key: &str, tags: &[&str]| {
            serde_json::to_value(MessageSendRequest {
                idempotency_key: key.into(),
                from: "person/example".into(),
                to: "agent/receiver".into(),
                content: "A note.".into(),
                title: None,
                in_reply_to: None,
                tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
                attachments: Vec::new(),
            })
            .unwrap()
        };
        // A conversation and its lifecycle reach clients but never wake the reconciler.
        let (status, sent) =
            json_request(app.clone(), "/v1/messages", send("talk", &["chat"])).await;
        assert_eq!(status, StatusCode::OK, "{sent}");
        assert!(!woke().await, "a conversation message woke the reconciler");
        assert!(events.has_changed().unwrap());
        events.borrow_and_update();
        let id = sent["value"]["subject"]
            .as_str()
            .or(sent["subject"].as_str())
            .unwrap()
            .trim_start_matches("message/")
            .to_owned();
        let (status, claim) = json_request(
            app.clone(),
            &format!("/v1/messages/{id}/claims"),
            json!({"lifecycle": "delivered", "actor": "agent/receiver", "idempotency_key": "talk-delivered"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{claim}");
        assert!(
            !woke().await,
            "a conversation message's lifecycle woke the reconciler"
        );
        assert!(events.has_changed().unwrap());
        events.borrow_and_update();
        // A repeat, or a transition the message has already passed, changes nothing a reader
        // can see and wakes no reader (#1085).
        for (lifecycle, key) in [("delivered", "talk-delivered"), ("staged", "talk-staged")] {
            let (status, claim) = json_request(
                app.clone(),
                &format!("/v1/messages/{id}/claims"),
                json!({"lifecycle": lifecycle, "actor": "agent/receiver", "idempotency_key": key}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{claim}");
            assert!(
                !events.has_changed().unwrap(),
                "a no-op {lifecycle} woke readers"
            );
        }
        // A work wake does.
        let (status, sent) = json_request(
            app.clone(),
            "/v1/messages",
            send("wake", &["st3-work:step-run/example/work"]),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{sent}");
        assert!(woke().await, "a work wake did not wake the reconciler");
    }

    #[tokio::test]
    async fn quiet_replication_wakes_skip_full_graph_projection() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let Json(first) = replication_wake(State(state.clone())).await.unwrap();
        assert_eq!(first["projection_attempted"], true);
        assert_eq!(first["projected"], true);
        let Json(second) = replication_wake(State(state)).await.unwrap();
        assert_eq!(second["projection_attempted"], false);
        assert_eq!(second["projected"], true);
        assert_eq!(second["admitted"], 0);
        assert_eq!(second["repairs"], 0);
    }

    fn materialize_run_agents(state: &AppState, run: &MissionRunView) {
        let mission = state
            .store
            .mission_spec(
                run.mission.strip_prefix("mission/").unwrap_or(&run.mission),
                Some(&run.revision),
            )
            .unwrap()
            .unwrap();
        let Some(source) = mission.declarations_kdl.as_deref() else {
            return;
        };
        let intent = crate::graph::parse_execution_intent(source, &state.node, &run.id).unwrap();
        state
            .store
            .apply_internal(&intent, &format!("test-materialize:{}", run.id))
            .unwrap();
    }

    fn apply_request(state: &AppState, source: &str, actor: &str, key: &str) -> ApplyRequest {
        let intent = parse_intent(source, &state.node).unwrap();
        let preview = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        ApplyRequest {
            intent: crate::model::IntentInput {
                kdl: source.into(),
                source_name: None,
            },
            expected_subjects: preview.subject_tokens,
            idempotency_key: key.into(),
            actor: Some(actor.into()),
        }
    }

    #[tokio::test]
    async fn an_event_waiter_cannot_consume_the_reconciler_signal() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let mut event = state.event_notify.subscribe();

        signal_changed(&state);

        tokio::time::timeout(Duration::from_millis(50), event.changed())
            .await
            .expect("the event waiter did not wake")
            .expect("the event sender closed");
        tokio::time::timeout(Duration::from_millis(50), state.notify.notified())
            .await
            .expect("the reconciler signal was lost");
    }

    #[tokio::test]
    async fn an_event_wait_ignores_a_wake_without_a_matching_event() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let waiter_state = state.clone();
        let waiter = tokio::spawn(async move {
            events(
                State(waiter_state),
                Query(EventQuery {
                    after: 0,
                    subject: None,
                    owner_run: None,
                    wait: Some(true),
                    timeout_ms: None,
                }),
            )
            .await
            .unwrap()
            .0
        });
        tokio::time::sleep(Duration::from_millis(10)).await;

        signal_changed(&state);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!waiter.is_finished(), "an empty wake ended the event wait");

        state
            .store
            .append_claim(&ClaimInput {
                subject: "custom/test/event".into(),
                kind: "custom.test.recorded".into(),
                actor: None,
                fields: BTreeMap::new(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        signal_changed(&state);
        let observed = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("the event wait did not finish")
            .unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].kind, "custom.test.recorded");
    }

    #[test]
    fn agents_list_the_subagents_their_harness_runs_now() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let source = format!(
            "version 2\nagent \"busy\" {{ workspace {0:?}; command \"true\" }}\n\
             agent \"quiet\" {{ workspace {0:?}; command \"true\" }}\n",
            root.path().display().to_string()
        );
        let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
        let planned = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source,
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &planned.subject_tokens, "subagents")
            .unwrap();
        let now = client_now_ms() as u64;
        let claim = |kind: &str, id: &str, fields: &[(&str, Value)]| {
            let mut all = BTreeMap::from([("subagent_id".to_owned(), json!(id))]);
            for (name, value) in fields {
                all.insert((*name).to_owned(), value.clone());
            }
            state
                .store
                .append_client_claim(&ClaimInput {
                    subject: "agent/node.busy".into(),
                    kind: kind.into(),
                    actor: Some("agent/node.busy".into()),
                    fields: all,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        };
        let appear = |id: &str, lease: u64| {
            claim(
                "subagent.appeared",
                id,
                &[
                    ("driver", json!("claude")),
                    ("incarnation_id", json!("inc-1")),
                    ("subagent_type", json!("Explore")),
                    ("description", json!("map the code")),
                    ("session_id", json!("session-1")),
                    ("step_run", json!("step-run/run-1/build")),
                    ("started_at_unix_ms", json!(1_790_931_600_000_u64)),
                    ("lease_expires_at_unix_ms", json!(lease)),
                ],
            )
        };
        appear("running", now + 600_000);
        appear("lapsed", now - 1);
        appear("finished", now + 600_000);
        claim(
            "subagent.ended",
            "finished",
            &[("outcome", json!("completed"))],
        );

        let agents =
            client_agent_resources(&state.store, false, "now", state.store.index().unwrap())
                .unwrap();
        let agent = |id: &str| agents.iter().find(|agent| agent["id"] == id).unwrap();
        assert_eq!(
            agent("agent/node.busy")["subagents"],
            json!([{
                "id": "running", "subagent_type": "Explore", "description": "map the code",
                "driver": "claude", "session_id": "session-1",
                "work_id": "step-run/run-1/build",
                "started_at": "2026-10-02T09:00:00.000Z",
                "lease_expires_at": client_timestamp(u128::from(now + 600_000)),
            }])
        );
        assert_eq!(agent("agent/node.quiet")["subagents"], json!([]));
        // Clients read it as the typed field.
        let typed: st3_client::Resource =
            serde_json::from_value(agent("agent/node.busy").clone()).unwrap();
        let st3_client::Resource::Agent(typed) = typed else {
            panic!("an agent resource");
        };
        assert_eq!(typed.subagents.len(), 1);
    }

    #[test]
    fn member_faults_are_visible_in_agents_and_doctor_until_recovery() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let source = format!(
            r#"version 2
agent "bad" {{ workspace {:?}; command "true" }}
agent "good" {{ workspace {:?}; command "true" }}
"#,
            root.path().display().to_string(),
            root.path().display().to_string()
        );
        let intent = crate::graph::parse_test_intent(&source, "node").unwrap();
        let planned = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source,
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &planned.subject_tokens, "fault-surface")
            .unwrap();
        for status in ["failed", "resolved"] {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: "agent/node.bad".into(),
                    kind: "runtime.reconcile-decision".into(),
                    actor: None,
                    fields: BTreeMap::from([
                        ("key".into(), Value::String("member-reconcile".into())),
                        (
                            "decision".into(),
                            Value::String(
                                if status == "failed" {
                                    "member-fault"
                                } else {
                                    "member-recovered"
                                }
                                .into(),
                            ),
                        ),
                        (
                            "reason".into(),
                            Value::String(
                                "render refuses to change tracked file .claude/settings.local.json"
                                    .into(),
                            ),
                        ),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            let agents =
                client_agent_resources(&state.store, false, "now", state.store.index().unwrap())
                    .unwrap();
            let bad = agents.iter().find(|a| a["id"] == "agent/node.bad").unwrap();
            let good = agents
                .iter()
                .find(|a| a["id"] == "agent/node.good")
                .unwrap();
            assert!(good["fault"].is_null());
            assert_ne!(good["state"], "failed");
            let report = doctor_report(&state).unwrap().0;
            let check = report
                .checks
                .iter()
                .find(|c| c.name == "member-reconcile/agent/node.bad");
            if status == "failed" {
                assert_eq!(bad["state"], "failed");
                assert!(
                    bad["fault"]
                        .as_str()
                        .unwrap()
                        .contains("settings.local.json")
                );
                assert_eq!(check.unwrap().status, "fail");
                assert!(check.unwrap().message.contains("settings.local.json"));
            } else {
                assert!(bad["fault"].is_null());
                assert_ne!(bad["state"], "failed");
                assert!(check.is_none());
            }
        }
    }

    #[tokio::test]
    async fn health_and_doctor_report_runtime_metadata() {
        let root = tempfile::tempdir().unwrap();
        let app = router(state(root.path()));
        let (status, health) = get_request(app.clone(), "/v1/health").await;
        assert_eq!(status, StatusCode::OK, "{health}");
        assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
        assert!(health["isolation"].is_string());

        let (status, doctor) = get_request(app, "/v1/doctor").await;
        assert_eq!(status, StatusCode::OK, "{doctor}");
        assert!(doctor["status"].is_string());
        assert!(
            doctor["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|check| check["name"] == "runtime-ownership")
        );
        let disk = doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "disk-space")
            .expect("doctor reports disk space");
        assert!(
            disk["message"].as_str().unwrap().contains("GiB free"),
            "{disk}"
        );
    }

    #[tokio::test]
    async fn doctor_and_replication_status_explain_fabric_refusals_without_errors() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        state.fleet_id = Some("94cd11ba-c582-4558-9c84-c3bda922eb6d".into());
        state.configured_peers = vec!["cobalt".into()];
        let app = router(state.clone());
        let (status, recorded) = json_request(app.clone(), "/v1/internal/replication/peer-failure", json!({
            "peer": "cobalt", "status": "refused", "error": "refused by that member's Fabric grants (service st3-peer-v1); replication can continue through other members"
        })).await;
        assert_eq!(status, StatusCode::OK, "{recorded}");
        let (status, replication) = get_request(app.clone(), "/v1/replication/status").await;
        assert_eq!(status, StatusCode::OK, "{replication}");
        assert_eq!(replication["peers"][0]["status"], "refused");
        assert!(replication["peers"][0]["last_error"].is_null());
        assert!(
            replication["peers"][0]["refusal_reason"]
                .as_str()
                .unwrap()
                .contains("Fabric grants")
        );
        let (status, doctor) = get_request(app, "/v1/doctor").await;
        assert_eq!(status, StatusCode::OK, "{doctor}");
        let check = doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "replication")
            .unwrap();
        assert_eq!(check["status"], "pass", "{check}");
        assert!(
            check["message"]
                .as_str()
                .unwrap()
                .contains("cobalt: refused by that member's Fabric grants")
        );
    }

    /// Two members apart, as during a partition, can each accept the same idempotency key for
    /// a different request (#1026). Both claims stand once they meet; doctor says which, and a
    /// retry with the key is refused instead of answering with either.
    #[tokio::test]
    async fn doctor_names_an_idempotency_key_two_members_used_for_different_requests() {
        const FLEET: &str = "94cd11ba-c582-4558-9c84-c3bda922eb6d";
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        state.store.bind_fleet(FLEET).unwrap();
        let note = |text: &str| ClaimInput {
            subject: "custom/partition/note".into(),
            kind: "custom.partition.note".into(),
            actor: Some("person/tester".into()),
            fields: BTreeMap::from([("text".into(), Value::String(text.into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("partition-key".into()),
        };
        let app = router(state.clone());
        let (_, doctor) = get_request(app.clone(), "/v1/doctor").await;
        let check = |doctor: &Value| {
            doctor["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|check| check["name"] == "idempotency-keys")
                .cloned()
                .unwrap()
        };
        assert_eq!(check(&doctor)["status"], "pass");

        state.store.append_claim(&note("written here")).unwrap();
        let other = Store::open_memory("birch").unwrap();
        other.bind_fleet(FLEET).unwrap();
        other.append_claim(&note("written there")).unwrap();
        let exchange = other
            .export_replication_exchange(FLEET, &state.store.replication_inventory().unwrap())
            .unwrap();
        state
            .store
            .receive_replication_exchange("birch", FLEET, &exchange)
            .unwrap();
        state.store.validate_replication_backlog().unwrap();
        state.store.project_replication_backlog().unwrap();

        let (_, doctor) = get_request(app, "/v1/doctor").await;
        let check = check(&doctor);
        assert_eq!(check["status"], "warn", "{check}");
        let message = check["message"].as_str().unwrap();
        assert!(message.starts_with("1 idempotency key was used"), "{check}");
        assert!(
            message.contains("custom/partition/note by birch"),
            "{check}"
        );
        assert!(
            message.contains(&format!(
                "custom/partition/note by {}",
                state.store.origin()
            )),
            "{check}"
        );
        let retry = state.store.append_claim(&note("written here")).unwrap_err();
        assert_eq!(retry.code, "idempotency-conflict", "{retry:?}");
    }

    #[test]
    fn doctor_reports_waiting_claims_without_a_replication_fault() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        state.fleet_id = Some("fleet/waiting".into());
        state.configured_peers = vec!["alder".into()];
        let mut registry = st3_schema::registry().clone();
        registry.claims.remove("doc.bound").unwrap();
        state.store.set_claim_registry(registry);
        state.store.bind_fleet("fleet/waiting").unwrap();
        let source = Store::open_memory("alder").unwrap();
        source.bind_fleet("fleet/waiting").unwrap();
        source
            .put_document("doc/release", b"release", &None, "release")
            .unwrap();
        let exchange = source
            .export_replication_exchange(
                "fleet/waiting",
                &state.store.replication_inventory().unwrap(),
            )
            .unwrap();
        state
            .store
            .receive_replication_exchange("alder", "fleet/waiting", &exchange)
            .unwrap();
        assert_eq!(
            state.store.validate_replication_backlog().unwrap().unknown,
            1
        );
        assert!(state.store.project_replication_backlog().unwrap());
        let exchange = source.export_replication_summary("fleet/waiting").unwrap();
        state
            .store
            .receive_replication_exchange("alder", "fleet/waiting", &exchange)
            .unwrap();
        let report = doctor_report(&state).unwrap().0;
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "replication")
            .unwrap();
        assert_eq!(check.status, "pass");
        assert!(check.message.contains("1 claims waiting for a newer build"));
        assert!(
            !report
                .checks
                .iter()
                .any(|check| check.name == "shared-projections")
        );
    }

    #[test]
    fn doctor_names_shared_tables_that_differ_at_equal_inventory() {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        state.fleet_id = Some("fleet/audit".into());
        state.configured_peers = vec!["alder".into()];
        state.store.bind_fleet("fleet/audit").unwrap();
        let source = Store::open_memory("alder").unwrap();
        source.bind_fleet("fleet/audit").unwrap();
        let original = source.export_replication_summary("fleet/audit").unwrap();
        let mut different = original.clone();
        different
            .projection_digests
            .insert("planning_sessions".into(), "different".into());
        state
            .store
            .receive_replication_exchange("alder", "fleet/audit", &different)
            .unwrap();
        let report = doctor_report(&state).unwrap().0;
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "shared-projections")
            .unwrap();
        assert_eq!(check.status, "fail");
        assert!(check.message.contains("alder/planning_sessions"));
        state
            .store
            .receive_replication_exchange("alder", "fleet/audit", &original)
            .unwrap();
        assert!(
            !doctor_report(&state)
                .unwrap()
                .0
                .checks
                .iter()
                .any(|check| check.name == "shared-projections")
        );
    }

    #[test]
    fn doctor_lists_attention_items_open_for_more_than_a_day() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        for (key, title) in [
            ("first", "Renew the signing key"),
            ("second", "Pick a region"),
        ] {
            state
                .store
                .record_operational_failure(
                    &format!("disk-{key}"),
                    &AttentionRequest {
                        reviewer: "person/alex".into(),
                        title: title.into(),
                        reason: "A person needs to decide.".into(),
                        severity: "warning".into(),
                        targets: vec![format!("daemon/{key}")],
                        actor: "daemon/runtime".into(),
                        idempotency_key: key.into(),
                    },
                )
                .unwrap();
        }
        assert!(state.store.attention_items(None).unwrap().is_empty());
        let items = doctor_attention_items(&state.store, client_now_ms()).unwrap();
        assert_eq!(items.len(), 2);
        let requested = items
            .iter()
            .map(|item| item.requested_at_unix_ms)
            .max()
            .unwrap();

        let fresh = stale_attention_check(&items, requested + 3_600_000);
        assert_eq!(fresh.status, "pass");
        let stale = stale_attention_check(&items, requested + 2 * 86_400_000 + 3 * 3_600_000);
        assert_eq!(
            (stale.name.as_str(), stale.status.as_str()),
            ("attention-age", "warn")
        );
        assert!(
            stale
                .message
                .starts_with("2 attention items have been open for more than a day"),
            "{}",
            stale.message
        );
        assert!(
            stale
                .message
                .contains("daemon/first for no owning agent, open 2d 3h: Renew the signing key"),
            "{}",
            stale.message
        );
        let report = doctor_report(&state).unwrap().0;
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.name == "attention-age" && check.status == "pass")
        );
    }

    #[test]
    fn doctor_shows_what_spends_the_shared_github_budget() {
        use crate::resource::{
            GithubBudget, GithubBudgetReport, GithubSpenderReport, GithubUsageReport,
        };
        let now = 1_790_000_000_000_u128;
        let quiet = github_usage_checks(&GithubUsageReport::default(), now);
        assert_eq!(quiet.len(), 1);
        assert_eq!(
            (quiet[0].name.as_str(), quiet[0].status.as_str()),
            ("github-budget", "pass")
        );

        let spender = |name: &str, counted| GithubSpenderReport {
            spender: name.into(),
            sent: counted + 10,
            not_modified: 10,
            refused: 0,
            counted_last_hour: counted,
            last_sent_at_unix_ms: now - 5_000,
        };
        let usage = |remaining| GithubUsageReport {
            spenders: vec![
                spender("observer/orchid-listing", 40),
                spender("observer/lichen-ref", 2),
            ],
            budgets: vec![GithubBudgetReport {
                budget: GithubBudget {
                    resource: "core".into(),
                    limit: 5000,
                    remaining,
                    used: 5000 - remaining,
                    reset_at_unix_ms: now + 600_000,
                    reported_at_unix_ms: now - 5_000,
                },
                counted_here: 42,
            }],
        };
        let checks = github_usage_checks(&usage(4000), now);
        let names = checks
            .iter()
            .map(|check| (check.name.as_str(), check.status.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                ("github-budget/core", "pass"),
                ("github-requests/observer/orchid-listing", "pass"),
                ("github-requests/observer/lichen-ref", "pass"),
            ]
        );
        assert!(
            checks[0]
                .message
                .contains("observers on this host sent 42 of the 1000 counted in this window"),
            "{}",
            checks[0].message
        );
        assert!(
            checks[1]
                .message
                .starts_with("40 counted requests in the last hour; since the daemon started 50 sent, 10 not modified"),
            "{}",
            checks[1].message
        );
        // Under a tenth of the budget left warns until the window resets.
        assert_eq!(github_usage_checks(&usage(400), now)[0].status, "warn");
        assert_eq!(
            github_usage_checks(&usage(400), now + 600_000)[0].status,
            "pass"
        );
    }

    #[tokio::test]
    async fn replication_receive_accepts_a_request_above_axums_default_body_limit() {
        let root = tempfile::tempdir().unwrap();
        let value = json!({
            "peer": "source",
            "fleet_id": "fleet",
            "exchange": {
                "peer": "source",
                "fleet_id": "fleet",
                "schema_digest": st3_schema::registry().digest(),
                "authority_digest": "",
                "graph_digest": "",
                "inventory": { "digest": "", "envelopes": [] },
                "envelopes": []
            }
        });
        let mut body = serde_json::to_vec(&value).unwrap();
        body.resize(2 * 1024 * 1024 + 1, b' ');
        let response = router(state(root.path()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/internal/replication/receive")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn replication_export_accepts_a_request_above_axums_default_body_limit() {
        let root = tempfile::tempdir().unwrap();
        let value = json!({
            "fleet_id": "fleet",
            "inventory": { "digest": "", "envelopes": [] },
            "summary_only": false
        });
        let mut body = serde_json::to_vec(&value).unwrap();
        body.resize(2 * 1024 * 1024 + 1, b' ');
        let response = router(state(root.path()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/internal/replication/export")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// The replication worker asks the daemon whether it needs a checkpoint's manifest, and
    /// hands it a whole manifest to adopt, which can be far above axum's default body limit.
    #[tokio::test]
    async fn the_worker_asks_for_and_hands_over_checkpoint_manifests() {
        let root = tempfile::tempdir().unwrap();
        let app = router(state(root.path()));
        let post = |uri: &str, body: Vec<u8>| {
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        };
        let response = app
            .clone()
            .oneshot(post(
                "/v1/internal/replication/checkpoint-need",
                b"{}".to_vec(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let need: Value = serde_json::from_slice(&body).unwrap();
        assert!(need["value"].is_null(), "{need}");

        let manifest = crate::store::CheckpointManifest {
            checkpoint: "checkpoint/2026-09-27".into(),
            cut_unix_ms: crate::store::checkpoint_cut("checkpoint/2026-09-27").unwrap(),
            ..Default::default()
        };
        let mut body = serde_json::to_vec(&manifest).unwrap();
        body.resize(8 * 1024 * 1024, b' ');
        let response = app
            .oneshot(post("/v1/internal/replication/checkpoint-adopt", body))
            .await
            .unwrap();
        // Well past the default limit, and refused only because nothing certified it here.
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            error.to_string().contains("checkpoint-not-stable"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn api_rejects_the_removed_plan_routes() {
        let root = tempfile::tempdir().unwrap();
        for path in ["/v1/plans/example", "/v1/plan-runs/example"] {
            let response = router(state(root.path()))
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }
    #[tokio::test]
    async fn mission_resolves_a_bare_document_to_immutable_bytes() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let version = state
            .store
            .put_document("doc/task", b"hello", &None, "document")
            .unwrap();
        let app = router(state);
        let (status, body) = json_request(
            app.clone(),
            "/v1/intent/mission",
            serde_json::to_value(MissionRequest {
                intent: crate::model::IntentInput {
                    kdl: r#"version 2
 message "task" { to "worker"; content "doc/task" } "#
                        .into(),
                    source_name: None,
                },
                at_index: None,
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.pointer("/resolved_intent/kdl")
                .and_then(Value::as_str)
                .unwrap()
                .contains(&format!("doc/task@{}", version.hash))
        );
    }

    async fn preview_and_apply(app: Router, kdl: &str) -> (Value, StatusCode, Value) {
        let (status, preview) = json_request(
            app.clone(),
            "/v1/intent/mission",
            serde_json::to_value(MissionRequest {
                intent: crate::model::IntentInput {
                    kdl: kdl.into(),
                    source_name: None,
                },
                at_index: None,
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{preview}");
        let (status, applied) = json_request(
            app,
            "/v1/intent/apply",
            serde_json::to_value(ApplyRequest {
                intent: crate::model::IntentInput {
                    kdl: kdl.into(),
                    source_name: None,
                },
                expected_subjects: serde_json::from_value(preview["subject_tokens"].clone())
                    .unwrap(),
                idempotency_key: format!("apply-{}", hex::encode(Sha256::digest(kdl))),
                actor: Some("person/alex".into()),
            })
            .unwrap(),
        )
        .await;
        (preview, status, applied)
    }

    #[tokio::test]
    async fn publication_refuses_a_reference_that_does_not_resolve_without_a_write() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        let app = router(state);
        let before = store.index().unwrap();
        let kdl = r#"version 2
mission "work" state="ready" {
  goal "Complete the work."
  step "do-work" { assigned-to "agent/example/nobody" }
}"#;
        let refusal =
            "mission `mission/work` references missing eligible agent `agent/example/nobody`";

        let (preview, status, error) = preview_and_apply(app.clone(), kdl).await;
        assert_eq!(preview["blockers"], json!([refusal]));
        assert_eq!(preview["warnings"], json!([]));
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
        assert_eq!(error["code"], "unresolved-reference");
        assert_eq!(error["details"]["refusals"], json!([refusal]));
        assert_eq!(store.index().unwrap(), before);

        let declared =
            format!("{kdl}\nagent \"example/nobody\" {{ workspace \"/tmp\"; command \"true\" }}");
        let (preview, status, applied) = preview_and_apply(app, &declared).await;
        assert_eq!(preview["blockers"], json!([]));
        assert_eq!(status, StatusCode::OK, "{applied}");
    }

    #[tokio::test]
    async fn publication_refuses_a_render_that_would_fail_on_this_host() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        let app = router(state);
        let workspace = tempfile::tempdir().unwrap();
        let workspace_path = workspace.path().display().to_string();
        for args in [&["init", "-q"][..], &["add", "tracked"][..]] {
            if args[0] == "add" {
                std::fs::write(workspace.path().join("tracked"), "original\n").unwrap();
            }
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(workspace.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }

        let (_, status, applied) = preview_and_apply(
            app.clone(),
            &format!(
                "version 2\nagent \"one\" {{ workspace {workspace_path:?}; command \"true\"; render {{ file \"shared\" \"one\" }} }}"
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{applied}");
        let before = store.index().unwrap();

        let kdl = format!(
            r#"version 2
agent "two" {{ workspace {workspace_path:?}; command "true"; render {{ file "shared" "two" }} }}
agent "three" {{ workspace {workspace_path:?}; command "true"; render {{ file "tracked" "changed" }} }}
agent "four" {{ workspace {workspace_path:?}; command "true"; render {{ file "own" "a"; file "own" "b" }} }}
"#
        );
        let (preview, status, error) = preview_and_apply(app, &kdl).await;
        let shared = workspace.path().join("shared");
        let own = workspace.path().join("own");
        let tracked = workspace.path().join("tracked");
        let refusals = json!([
            format!(
                "prepare render for agent/node.four in {workspace_path}: render operations disagree about {}",
                own.display()
            ),
            format!(
                "prepare render for agent/node.three in {workspace_path}: render refuses to change tracked file {}",
                tracked.display()
            ),
            format!(
                "render owners agent/node.one and agent/node.two disagree about {}",
                shared.display()
            ),
        ]);
        assert_eq!(preview["blockers"], refusals);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
        assert_eq!(error["code"], "render-refused");
        assert_eq!(error["details"]["refusals"], refusals);
        assert_eq!(store.index().unwrap(), before);
        assert!(!shared.exists());
    }

    #[tokio::test]
    async fn st_doctor_reports_graph_references_that_no_longer_resolve() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        state
            .store
            .apply_internal(
                &crate::graph::parse_intent(
                    r#"version 2
mission "work" state="ready" {
  goal "Complete the work."
  step "do-work" { assigned-to "agent/example/nobody" }
}"#,
                    "node",
                )
                .unwrap(),
                "published before the reference checks",
            )
            .unwrap();
        let (status, doctor) = get_request(router(state), "/v1/doctor").await;
        assert_eq!(status, StatusCode::OK, "{doctor}");
        let check = doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "graph-references")
            .expect("doctor checks graph references");
        assert_eq!(check["status"], "warn");
        assert_eq!(
            check["message"],
            "1 reference no longer resolves: mission `mission/work` references missing eligible agent `agent/example/nobody`"
        );
    }

    #[tokio::test]
    async fn preview_and_publish_reject_invalid_nested_declarations_without_a_write() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        let app = router(state);
        let before = store.index().unwrap();
        let kdl = r#"version 2
mission "invalid-message" state="ready" {
  goal "Reject this declaration before the mission runs."
  step "send" {
    agentless
    message "notice" {
      to "agent/worker"
      subject "This field is not valid."
      body "This field is not valid."
    }
  }
}"#;

        let (preview_status, preview_error) = json_request(
            app.clone(),
            "/v1/intent/mission",
            serde_json::to_value(MissionRequest {
                intent: crate::model::IntentInput {
                    kdl: kdl.into(),
                    source_name: Some("invalid-message.kdl".into()),
                },
                at_index: None,
            })
            .unwrap(),
        )
        .await;
        assert_eq!(preview_status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(preview_error["code"], "unknown-child");
        assert_eq!(store.index().unwrap(), before);

        let (publish_status, publish_error) = json_request(
            app,
            "/v1/intent/apply",
            serde_json::to_value(ApplyRequest {
                intent: crate::model::IntentInput {
                    kdl: kdl.into(),
                    source_name: Some("invalid-message.kdl".into()),
                },
                expected_subjects: BTreeMap::new(),
                idempotency_key: "reject-invalid-nested-message".into(),
                actor: Some("person/alex".into()),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(publish_status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(publish_error["code"], "unknown-child");
        assert_eq!(store.index().unwrap(), before);
    }

    #[test]
    fn launch_decision_responses_preserve_type_membership_uniqueness_and_rank_order() {
        let options = vec![
            LaunchDecisionOption {
                id: "a".into(),
                label: "A".into(),
                description: Some("first option".into()),
            },
            LaunchDecisionOption {
                id: "b".into(),
                label: "B".into(),
                description: None,
            },
        ];
        assert!(valid_launch_decision_response(
            &LaunchDecisionType::Boolean,
            &[],
            &LaunchDecisionResponse::Boolean(true)
        ));
        assert!(valid_launch_decision_response(
            &LaunchDecisionType::SingleChoice,
            &options,
            &LaunchDecisionResponse::SingleChoice("a".into())
        ));
        assert!(!valid_launch_decision_response(
            &LaunchDecisionType::SingleChoice,
            &options,
            &LaunchDecisionResponse::SingleChoice("missing".into())
        ));
        assert!(valid_launch_decision_response(
            &LaunchDecisionType::MultipleChoice,
            &options,
            &LaunchDecisionResponse::MultipleChoice(vec!["b".into(), "a".into()])
        ));
        assert!(!valid_launch_decision_response(
            &LaunchDecisionType::MultipleChoice,
            &options,
            &LaunchDecisionResponse::MultipleChoice(vec!["a".into(), "a".into()])
        ));
        assert!(valid_launch_decision_response(
            &LaunchDecisionType::Rank,
            &options,
            &LaunchDecisionResponse::Rank(vec!["b".into(), "a".into()])
        ));
        assert!(!valid_launch_decision_response(
            &LaunchDecisionType::Rank,
            &options,
            &LaunchDecisionResponse::Rank(vec!["a".into()])
        ));
        assert!(!valid_launch_decision_response(
            &LaunchDecisionType::Rank,
            &options,
            &LaunchDecisionResponse::MultipleChoice(vec!["a".into(), "b".into()])
        ));
    }

    #[test]
    fn launch_candidates_accept_durable_seats_and_implicit_loop_missions() {
        let source = r#"
version 2
mission "planned/work" state="ready" {
  goal "Keep a worker available and improve the result."
  agent "worker" {
    workspace "."
    command "true"
    restart "always"
  }
  loop "improve" {
    max-rounds 2
    round { completion { when "all-steps-exhausted" } }
  }
}
"#;
        let intent = parse_intent(source, "node").unwrap();
        assert_eq!(intent.missions.len(), 2);
        assert!(intent.subjects.is_empty());
        assert!(launch_candidate_mission(&intent, "planned/work").is_ok());

        let extra = parse_intent(
            r#"
version 2
mission "planned/work" state="ready" { goal "Do the requested work." }
mission "unrequested/work" state="ready" { goal "Do unrelated work." }
"#,
            "node",
        )
        .unwrap();
        let error = launch_candidate_mission(&extra, "planned/work").unwrap_err();
        assert_eq!(error.code, "wrong-launch-mission");
    }

    #[tokio::test]
    async fn planner_choice_is_frozen_per_launch_and_survives_projection_rebuild() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let database = root.path().join("graph.sqlite3");
        let mut first = state(root.path());
        first.store = Arc::new(Store::open(&database, "node").unwrap());
        let first_app = router(first.clone());
        let (status, first_launch) = json_request(
            first_app,
            "/v1/launches",
            serde_json::to_value(PlanningSessionStartRequest {
                mission: "first-planner".into(),
                run: None,
                request: b"Plan the first mission.".to_vec(),
                workspace: workspace.display().to_string(),
                requester: Some("person/alex".into()),
                provider: None,
                model: None,
                effort: None,
                idempotency_key: "first-planner-session".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first_launch}");
        assert_eq!(first_launch["planner_config"]["provider"], "codex");
        assert_eq!(first_launch["planner_config"]["model"], "gpt-6-sol");
        assert_eq!(first_launch["planner_config"]["effort"], "medium");

        let mut second = first.clone();
        second.planner_default = PlannerSpec {
            provider: "claude".into(),
            model: Some("opus".into()),
            effort: Some("high".into()),
        };
        let (status, second_launch) = json_request(
            router(second),
            "/v1/launches",
            serde_json::to_value(PlanningSessionStartRequest {
                mission: "second-planner".into(),
                run: None,
                request: b"Plan the second mission.".to_vec(),
                workspace: workspace.display().to_string(),
                requester: Some("person/alex".into()),
                provider: None,
                model: None,
                effort: None,
                idempotency_key: "second-planner-session".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second_launch}");
        assert_eq!(second_launch["planner_config"]["provider"], "claude");
        assert_eq!(second_launch["planner_config"]["model"], "opus");
        let first_id = first_launch["id"].as_str().unwrap();
        let second_id = second_launch["id"].as_str().unwrap();
        drop(first);
        let rebuilt = Store::open(&database, "node").unwrap();
        assert_eq!(
            rebuilt
                .planning_session(first_id)
                .unwrap()
                .unwrap()
                .planner_config,
            PlannerSpec::default()
        );
        assert_eq!(
            rebuilt
                .planning_session(second_id)
                .unwrap()
                .unwrap()
                .planner_config
                .provider,
            "claude"
        );
    }

    /// Free mode (#799): an agent starts a planner-backed launch as its own requester.
    #[tokio::test]
    async fn an_agent_starts_a_launch_as_its_requester() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let (status, started) = json_request(
            router(state(root.path())),
            "/v1/launches",
            serde_json::to_value(PlanningSessionStartRequest {
                mission: "example/planned".into(),
                run: None,
                request: b"Plan a harmless task.".to_vec(),
                workspace: workspace.display().to_string(),
                requester: Some("agent/example/chief".into()),
                provider: None,
                model: None,
                effort: None,
                idempotency_key: "agent-launch-session".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{started}");
        assert_eq!(started["requester"], "agent/example/chief");
    }

    #[tokio::test]
    async fn planning_requires_an_exact_preview_and_publishes_without_a_run() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let marker = workspace.join("marker.txt");
        fs::write(&marker, "unchanged\n").unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        let app = router(state);

        let (status, started) = json_request(
            app.clone(),
            "/v1/launches",
            serde_json::to_value(PlanningSessionStartRequest {
                mission: "planned/work".into(),
                run: None,
                request: b"Mission a two-step release without changing this workspace.".to_vec(),
                workspace: workspace.display().to_string(),
                requester: Some("alex".into()),
                provider: None,
                model: Some("gpt-5.6-sol".into()),
                effort: Some("medium".into()),
                idempotency_key: "planning-session-test".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{started}");
        let session = started["id"].as_str().unwrap();
        let planner = started["planner"].as_str().unwrap();
        assert!(planner.ends_with(&format!("/planner.{}", &session[..10])));
        assert_eq!(started["requester"], "person/alex");
        let request_reference = started["request"].as_str().unwrap();
        assert!(request_reference.starts_with("doc/planning/"));
        let (request_name, request_hash) = request_reference.rsplit_once('@').unwrap();
        assert_eq!(
            store
                .get_document(request_name, request_hash)
                .unwrap()
                .unwrap(),
            b"Mission a two-step release without changing this workspace."
        );
        let planner_seat = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|desired| desired.subject == planner)
            .unwrap();
        assert!(planner_seat.owner_run.is_none());
        let planner_launch = planner_seat.member.as_ref().unwrap().launch.clone();
        assert!(matches!(
            &planner_launch,
            crate::model::LaunchSpec::Argv(arguments)
                if arguments.iter().any(|argument| argument == "--dangerously-bypass-approvals-and-sandbox")
                    && arguments.iter().any(|argument| argument == "--dangerously-bypass-hook-trust")
        ));
        assert!(store.mission_spec("planned/work", None).unwrap().is_none());
        assert!(store.active_mission_runs().unwrap().is_empty());

        let first = br#"
version 2

  mission "planned/work" state="ready" {
    goal "Publish the planned result."
    step "inspect" { goal "Inspect the source." }
    step "change" {
      goal "Make the approved change."
      depends-on { step "inspect" completed }
    }
  }

"#;
        let mut side_effect = br#"
version 2
  agent "side-effect" {
    workspace "/tmp"
    harness "codex" {}
  }
"#
        .to_vec();
        side_effect.extend_from_slice(&first[b"version 2\n".len()..]);
        let (status, rejected) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/submit"),
            serde_json::to_value(PlanningCandidateSubmitRequest {
                actor: planner.into(),
                markdown: b"# Mission with a side effect".to_vec(),
                kdl: side_effect,
                idempotency_key: "planning-candidate-side-effect".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rejected}");
        assert_eq!(rejected["code"], "wrong-launch-mission");
        assert!(
            store
                .desired_subjects()
                .unwrap()
                .into_iter()
                .all(|desired| desired.subject != "agent/node.side-effect")
        );

        let (status, submitted) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/submit"),
            serde_json::to_value(PlanningCandidateSubmitRequest {
                actor: planner.into(),
                markdown: b"# Mission\n\n1. Inspect.\n2. Change.\n".to_vec(),
                kdl: first.to_vec(),
                idempotency_key: "planning-candidate-one".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{submitted}");
        assert_eq!(submitted["candidate"]["revision"], 1);
        assert!(store.mission_spec("planned/work", None).unwrap().is_none());
        let (status, resubmitted) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/submit"),
            serde_json::to_value(PlanningCandidateSubmitRequest {
                actor: planner.into(),
                markdown: b"# Mission\n\n1. Inspect.\n2. Change.\n".to_vec(),
                kdl: first.to_vec(),
                idempotency_key: "planning-candidate-one-network-retry".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resubmitted}");
        assert_eq!(resubmitted["candidate"]["revision"], 1);
        assert_eq!(
            store
                .claims_for(
                    &format!("planning-session/{session}"),
                    Some("planning-session.candidate-submitted"),
                )
                .unwrap()
                .len(),
            1
        );

        let (status, previewed) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/preview"),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{previewed}");
        let first_hash = previewed["preview"]["hash"].as_str().unwrap().to_owned();
        let graph = previewed["preview"]["graph"].as_str().unwrap();
        assert!(graph.contains("inspect [root]"), "{graph}");
        assert!(graph.contains("change [after inspect]"), "{graph}");
        assert!(
            previewed["preview"]["diff"]
                .as_str()
                .unwrap()
                .contains("mission/planned/work")
        );
        let (_, attention) = get_request(app.clone(), "/v1/attention?person=alex").await;
        assert_eq!(attention.as_array().unwrap().len(), 1);
        assert_eq!(attention[0]["kind"], "launch-approval");
        assert_eq!(
            attention[0]["subject"],
            format!("planning-session/{session}")
        );
        assert_eq!(attention[0]["actions"][0]["argv"][1], "launch");
        assert_eq!(attention[0]["actions"][1]["argv"][1], "launch");
        assert_eq!(attention[0]["actions"][2]["argv"][1], "launch");

        let (status, revised) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/revise"),
            serde_json::to_value(PlanningRevisionRequest {
                actor: "person/alex".into(),
                feedback: b"Add a verification step.".to_vec(),
                idempotency_key: "planning-revision-one".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{revised}");
        assert_eq!(revised["status"], "revision-requested");
        assert!(revised.get("preview").is_none());
        assert!(store.mission_spec("planned/work", None).unwrap().is_none());
        let (_, attention) = get_request(app.clone(), "/v1/attention?person=alex").await;
        assert_eq!(attention, json!([]));

        let second = br#"
version 2

  mission "planned/work" state="ready" {
    goal "Publish the planned and verified result."
    step "inspect" { goal "Inspect the source." }
    step "change" {
      goal "Make the approved change."
      depends-on { step "inspect" completed }
    }
    step "verify" {
      goal "Verify the result."
      depends-on { step "change" completed }
    }
  }

"#;
        let (status, resubmitted) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/submit"),
            serde_json::to_value(PlanningCandidateSubmitRequest {
                actor: planner.into(),
                markdown: b"# Mission\n\n1. Inspect.\n2. Change.\n3. Verify.\n".to_vec(),
                kdl: second.to_vec(),
                idempotency_key: "planning-candidate-two".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{resubmitted}");
        assert_eq!(resubmitted["candidate"]["revision"], 2);

        let (status, previewed) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/preview"),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{previewed}");
        let current_hash = previewed["preview"]["hash"].as_str().unwrap().to_owned();
        assert_ne!(current_hash, first_hash);
        let (status, variants) = get_request(
            app.clone(),
            &format!("/v1/client/launches/{session}/variants"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{variants}");
        let variant = &variants["items"][0];
        let preview_token = variant["preview_token"].as_str().unwrap().to_owned();
        assert!(preview_token.starts_with("lpv0:"));
        assert_eq!(current_hash, preview_token);
        assert_eq!(variant["normalized_mission"]["id"], "planned/work");
        assert_eq!(
            variant["preview"]["goal"],
            "Publish the planned and verified result."
        );
        assert_eq!(variant["preview"]["steps"].as_array().unwrap().len(), 3);
        assert_eq!(variant["preview"]["steps"][2]["depends"], json!(["change"]));
        let (status, launch_page) = get_request(app.clone(), "/v1/client/launches").await;
        assert_eq!(status, StatusCode::OK, "{launch_page}");
        assert_eq!(launch_page["items"][0]["preview"], variant["preview"]);
        let (status, attention_page) =
            get_request(app.clone(), "/v1/client/attention?person=person%2Falex").await;
        assert_eq!(status, StatusCode::OK, "{attention_page}");
        assert_eq!(
            attention_page["items"][0]["launch_id"],
            format!("launch/{session}")
        );
        assert_eq!(attention_page["items"][0]["preview"], variant["preview"]);
        assert_eq!(attention_page["items"][0]["preview_token"], preview_token);
        let document_name = store.planning_session(session).unwrap().unwrap().request;
        let (status, document) = get_request(
            app.clone(),
            &format!(
                "/v1/client/documents/content?name={}",
                urlencoding::encode(&document_name)
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{document}");
        assert_eq!(document["reference"], document_name);
        assert!(
            document["bytes"]
                .as_array()
                .is_some_and(|bytes| !bytes.is_empty())
        );
        assert!(
            !serde_json::to_string(&variant["normalized_mission"])
                .unwrap()
                .contains("declarations_kdl")
        );
        assert_eq!(
            variant["visualization"]["views"],
            json!([
                "graph",
                "timeline",
                "swimlane",
                "revision",
                "risk",
                "live-progress"
            ])
        );
        assert_eq!(
            variant["visualization"]["timeline"]["entries"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            variant["visualization"]["swimlanes"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            variant["structured_diff"]["changes"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let (status, decision) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/decisions"),
            serde_json::to_value(LaunchDecisionRequest {
                actor: planner.into(),
                question: "Which verification level?".into(),
                decision_type: LaunchDecisionType::SingleChoice,
                options: vec![
                    LaunchDecisionOption {
                        id: "focused".into(),
                        label: "Focused".into(),
                        description: Some("Run the focused verification suite".into()),
                    },
                    LaunchDecisionOption {
                        id: "full".into(),
                        label: "Full".into(),
                        description: None,
                    },
                ],
                idempotency_key: "launch-decision-verification".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{decision}");
        let decision_id = decision["id"].as_str().unwrap();
        let decision_path = decision_id.trim_start_matches("launch-decision/");
        let (status, answered) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/decisions/{decision_path}/answer"),
            serde_json::to_value(LaunchDecisionAnswerRequest {
                actor: "person/alex".into(),
                response: LaunchDecisionResponse::SingleChoice("full".into()),
                explanation: Some("release candidate requires the full suite".into()),
                expected_revision: 1,
                idempotency_key: "launch-decision-verification-answer".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answered}");
        assert_eq!(answered["state"], "answered");
        assert_eq!(
            answered["response"],
            json!({"type":"single-choice","value":"full"})
        );
        assert_eq!(
            answered["explanation"],
            "release candidate requires the full suite"
        );
        let (status, immutable) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/decisions/{decision_path}/answer"),
            serde_json::to_value(LaunchDecisionAnswerRequest {
                actor: "person/alex".into(),
                response: LaunchDecisionResponse::SingleChoice("focused".into()),
                explanation: None,
                expected_revision: 1,
                idempotency_key: "launch-decision-verification-conflict".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{immutable}");
        assert_eq!(immutable["code"], "launch-decision-immutable");
        let (_, attention) = get_request(app.clone(), "/v1/attention?person=alex").await;
        assert_eq!(attention.as_array().unwrap().len(), 1);
        assert_eq!(attention[0]["kind"], "launch-approval");

        let (status, unauthorized) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/approve"),
            serde_json::to_value(PlanningApprovalRequest {
                actor: "person/intruder".into(),
                preview_hash: current_hash.clone(),
                idempotency_key: "planning-approve-unauthorized".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{unauthorized}");
        assert_eq!(unauthorized["code"], "launch-review-not-authorized");
        assert!(store.mission_spec("planned/work", None).unwrap().is_none());

        let (status, stale) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/approve"),
            serde_json::to_value(PlanningApprovalRequest {
                actor: "person/alex".into(),
                preview_hash: first_hash,
                idempotency_key: "planning-approve-stale".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{stale}");
        assert_eq!(stale["code"], "stale-launch-preview");
        assert!(store.mission_spec("planned/work", None).unwrap().is_none());

        let (status, approved) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/approve"),
            serde_json::to_value(PlanningApprovalRequest {
                actor: "person/alex".into(),
                preview_hash: preview_token.clone(),
                idempotency_key: "planning-approve-current".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{approved}");
        assert_eq!(approved["status"], "approved");
        assert_eq!(
            approved["published_revision"],
            approved["candidate"]["mission_revision"]
        );
        assert!(store.mission_spec("planned/work", None).unwrap().is_some());
        // The planner is a durable top-level seat, not a synthetic standing
        // mission. Approval removes that seat directly and leaves no planner
        // mission run behind to clean up.
        assert!(store.active_mission_runs().unwrap().is_empty());
        assert_eq!(fs::read_to_string(&marker).unwrap(), "unchanged\n");
        assert_eq!(fs::read_dir(&workspace).unwrap().count(), 1);
        let (_, attention) = get_request(app.clone(), "/v1/attention?person=alex").await;
        assert_eq!(attention, json!([]));

        let documents = store
            .latest_claim(
                &format!("planning-session/{session}"),
                Some("planning-session.approved"),
            )
            .unwrap()
            .expect("the published mission does not link its documents");
        for field in ["markdown", "kdl"] {
            let reference = documents
                .body
                .pointer(&format!("/fields/{field}"))
                .and_then(Value::as_str)
                .unwrap();
            let (name, hash) = reference.rsplit_once('@').unwrap();
            assert!(store.get_document(name, hash).unwrap().is_some());
        }
        let stopped_planner = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|desired| desired.subject == planner)
            .expect("approval did not publish the planner stop tombstone");
        assert_eq!(stopped_planner.kind, "stop");

        let (status, approved_again) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/approve"),
            serde_json::to_value(PlanningApprovalRequest {
                actor: "person/alex".into(),
                preview_hash: approved["preview"]["hash"].as_str().unwrap().into(),
                idempotency_key: "planning-approve-retry".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{approved_again}");
        assert_eq!(approved_again["status"], "approved");
        let (status, started_run) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/approve-and-launch"),
            serde_json::to_value(LaunchApproveAndStartRequest {
                actor: "person/alex".into(),
                preview_hash: preview_token.clone(),
                workspace: workspace.display().to_string(),
                inputs: BTreeMap::new(),
                idempotency_key: "approved-launch-combined".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{started_run}");
        assert_eq!(
            started_run["mission_run"]["revision"],
            approved["published_revision"]
        );
        assert_eq!(started_run["launch"]["status"], "approved");
        let (status, started_again) = json_request(
            app,
            &format!("/v1/launches/{session}/approve-and-launch"),
            serde_json::to_value(LaunchApproveAndStartRequest {
                actor: "person/alex".into(),
                preview_hash: preview_token,
                workspace: workspace.display().to_string(),
                inputs: BTreeMap::new(),
                idempotency_key: "approved-launch-combined".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{started_again}");
        assert_eq!(
            started_again["mission_run"]["subject"],
            started_run["mission_run"]["subject"]
        );
        assert_eq!(
            store
                .claims_for("mission/planned/work", Some("mission.published"))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .claims_for(
                    &format!("planning-session/{session}"),
                    Some("planning-session.approved"),
                )
                .unwrap()
                .len(),
            1
        );
        for kind in [
            "planning-session.started",
            "planning-session.candidate-submitted",
            "planning-session.previewed",
            "planning-session.revision-requested",
            "planning-session.approved",
        ] {
            assert!(
                !store
                    .claims_for(&format!("planning-session/{session}"), Some(kind))
                    .unwrap()
                    .is_empty(),
                "missing {kind}"
            );
        }
        let started_event = store
            .claims_for(
                &format!("planning-session/{session}"),
                Some("planning-session.started"),
            )
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(
            started_event
                .body
                .pointer("/fields/request")
                .and_then(Value::as_str),
            Some(request_reference)
        );
        let before = serde_json::to_value(store.planning_session(session).unwrap()).unwrap();
        store.rebuild_claim_projections().unwrap();
        let after = serde_json::to_value(store.planning_session(session).unwrap()).unwrap();
        assert_eq!(after, before);
    }

    #[tokio::test]
    async fn declarative_planning_approval_stops_its_session_planner() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        let request = store
            .put_document(
                "doc/planning/direct/request",
                b"Mission one inspection.",
                &None,
                "direct-planning-request",
            )
            .unwrap();
        let session = "planning/direct/one";
        let source = format!(
            r#"version 2
planning-session {session:?} {{
  mission "planned/direct"
  request {:?}
  workspace {:?}
  requester "person/operator"
  planner "codex" {{ model "gpt-5.6-sol"; effort "medium" }}
}}
"#,
            format!("{}@{}", request.name, request.hash),
            workspace.display().to_string(),
        );
        let intent = parse_intent(&source, "node").unwrap();
        let preview = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source,
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply_as(
                &intent,
                &preview.subject_tokens,
                "direct-planning-session",
                Some("person/operator"),
            )
            .unwrap();
        let planner =
            crate::graph::planning_planner_subject(&format!("planning-session/{session}"));
        assert_eq!(
            store.selected_desired_kind(&planner).unwrap().as_deref(),
            Some("agent")
        );

        let app = router(state);
        let encoded_session = urlencoding::encode(session);
        let candidate = br#"version 2
mission "planned/direct" state="ready" {
  goal "Inspect the release."
  step "inspect" { goal "Inspect the release input." }
}
"#;
        let (status, submitted) = json_request(
            app.clone(),
            &format!("/v1/launches/{encoded_session}/submit"),
            serde_json::to_value(PlanningCandidateSubmitRequest {
                actor: planner.clone(),
                markdown: b"# Mission\n\nInspect the release input.\n".to_vec(),
                kdl: candidate.to_vec(),
                idempotency_key: "direct-planning-candidate".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{submitted}");
        let (status, previewed) = json_request(
            app.clone(),
            &format!("/v1/launches/{encoded_session}/preview"),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{previewed}");
        let preview_hash = previewed["preview"]["hash"].as_str().unwrap();
        let (status, approved) = json_request(
            app,
            &format!("/v1/launches/{encoded_session}/approve"),
            serde_json::to_value(PlanningApprovalRequest {
                actor: "person/operator".into(),
                preview_hash: preview_hash.into(),
                idempotency_key: "direct-planning-approval".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{approved}");
        assert_eq!(approved["status"], "approved");
        assert_eq!(
            store.selected_desired_kind(&planner).unwrap().as_deref(),
            Some("stop")
        );
    }

    #[test]
    fn an_agent_known_only_from_its_own_harness_observations_is_history() {
        let root = tempfile::tempdir().unwrap();
        let store = state(root.path()).store;
        let subject = "agent/diagnostic-run/sig.base";
        store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "harness.observed".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("idle".into())),
                    ("driver".into(), Value::String("codex".into())),
                    ("incarnation_id".into(), Value::String("sig-1".into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("sig-base-observed".into()),
            })
            .unwrap();
        let current =
            client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
        assert!(
            current.iter().all(|agent| agent["id"] != subject),
            "{current:?}"
        );
        let history =
            client_agent_resources(&store, true, "snapshot", store.index().unwrap()).unwrap();
        let agent = history.iter().find(|agent| agent["id"] == subject).unwrap();
        assert_eq!(agent["operational"]["layer"], "history");
        assert_eq!(agent["operational"]["reasons"], json!(["undeclared"]));
    }

    #[test]
    fn client_work_projection_includes_agentless_mission_steps() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let state = state(root.path());
        let kdl = r#"version 2
mission "visible-agentless" state="ready" {
  goal "Keep agentless work visible in Control."
  step "steward" { title "Keep watch"; agentless }
}"#;
        let intent = parse_intent(kdl, "node").unwrap();
        let preview = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: kdl.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &preview.subject_tokens, "visible-agentless")
            .unwrap();
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "visible-agentless".into(),
                revision: None,
                workspace: workspace.display().to_string(),
                requester: Some("person/operator".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "visible-agentless-run".into(),
            })
            .unwrap();
        let resources = client_work_resources(
            &state.store,
            None,
            true,
            client_now_ms(),
            state.store.index().unwrap(),
        )
        .unwrap();
        let step = resources
            .iter()
            .find(|item| item["mission_run_id"] == run.subject)
            .unwrap();
        assert_eq!(step["path"], "steward");
        assert_eq!(step["title"], "Keep watch");
        assert_eq!(step["assigned_to"], Value::Null);
        assert_eq!(step["last_progress"], Value::Null);
        assert_eq!(step["agentless"], true);
        assert!(
            client_work_resources(
                &state.store,
                Some("agent/other"),
                true,
                client_now_ms(),
                state.store.index().unwrap()
            )
            .unwrap()
            .is_empty()
        );
    }

    /// A work item is the resource the work history lists for it, read without enriching every
    /// other step the store has run.
    #[test]
    fn a_work_item_reads_only_its_own_step() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let state = state(root.path());
        let kdl = r#"version 2
mission "many-runs" state="ready" {
  goal "Run often."
  concurrent-runs max=100
  step "build" { assigned-to "agent/builder" }
  step "watch" { agentless }
}"#;
        let intent = parse_intent(kdl, "node").unwrap();
        let preview = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: kdl.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &preview.subject_tokens, "many-runs")
            .unwrap();
        for run in 0..8 {
            state
                .store
                .create_mission_run(&MissionRunRequest {
                    mission: "many-runs".into(),
                    revision: None,
                    workspace: workspace.display().to_string(),
                    requester: Some("person/operator".into()),
                    mode: None,
                    inputs: BTreeMap::new(),
                    idempotency_key: format!("many-runs-{run}"),
                })
                .unwrap();
        }
        let (now, index) = (client_now_ms(), state.store.index().unwrap());
        let history = client_work_resources(&state.store, None, true, now, index).unwrap();
        assert_eq!(history.len(), 16);
        for item in &history {
            let id = item["id"].as_str().unwrap();
            let detail = client_work_item(&state.store, id, None, now, index)
                .unwrap()
                .unwrap();
            assert_eq!(&detail, item, "{id}");
        }
        let id = history[0]["id"].as_str().unwrap();
        crate::store::STEPS_ENRICHED.with(|enriched| enriched.set(0));
        client_work_item(&state.store, id, None, now, index).unwrap();
        assert_eq!(crate::store::STEPS_ENRICHED.with(std::cell::Cell::get), 1);
        let (assigned, assignee) = history
            .iter()
            .find_map(|item| Some((item["id"].as_str()?, item["assigned_to"].as_str()?)))
            .unwrap();
        assert!(
            client_work_item(&state.store, assigned, Some("agent/other"), now, index)
                .unwrap()
                .is_none()
        );
        assert!(
            client_work_item(&state.store, assigned, Some(assignee), now, index)
                .unwrap()
                .is_some()
        );
        assert!(
            client_work_item(&state.store, "step-run/none/build", None, now, index)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn steady_mission_and_work_pages_resume_after_unrelated_commits() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let state = state(root.path());
        for number in 0..7 {
            let name = format!("paged-{number}");
            let kdl = format!(
                r#"version 2
mission "{name}" state="ready" {{
  goal "Run a bounded page."
  step "build" {{ assigned-to "agent/builder" }}
}}"#
            );
            let intent = parse_intent(&kdl, "node").unwrap();
            let preview = state
                .store
                .mission(
                    &intent,
                    crate::model::IntentInput {
                        kdl,
                        source_name: None,
                    },
                )
                .unwrap();
            state
                .store
                .apply(&intent, &preview.subject_tokens, &name)
                .unwrap();
            state
                .store
                .create_mission_run(&MissionRunRequest {
                    mission: name.clone(),
                    revision: None,
                    workspace: workspace.display().to_string(),
                    requester: Some("person/operator".into()),
                    mode: None,
                    inputs: BTreeMap::new(),
                    idempotency_key: format!("run-{name}"),
                })
                .unwrap();
        }
        for collection in ["missions", "work"] {
            let expected = if collection == "missions" {
                state.store.mission_collection_ids(true, 0, 100).unwrap()
            } else {
                client_work_resources(
                    &state.store,
                    None,
                    true,
                    client_now_ms(),
                    state.store.index().unwrap(),
                )
                .unwrap()
                .into_iter()
                .map(|item| item["id"].as_str().unwrap().to_owned())
                .collect()
            };
            let app = router(state.clone());
            let mut cursor: Option<String> = None;
            let mut received = Vec::new();
            loop {
                state
                    .store
                    .append_claim(&probe_claim(
                        &format!("churn-{collection}-{}", received.len()),
                        "idle",
                    ))
                    .unwrap();
                let path = format!(
                    "/v1/client/{collection}?limit=2&history=true{}",
                    cursor
                        .as_deref()
                        .map(|cursor| format!("&cursor={}", urlencoding::encode(cursor)))
                        .unwrap_or_default()
                );
                let (status, response) = get_request(app.clone(), &path).await;
                assert_eq!(status, StatusCode::OK, "{response}");
                let page = &response;
                received.extend(
                    page["items"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|item| item["id"].as_str().unwrap().to_owned()),
                );
                cursor = page["page"]["next_cursor"].as_str().map(str::to_owned);
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(received, expected, "{collection}");
        }
    }

    /// Each page of the work history is that page of the whole history, for every reader, and
    /// reading it enriches only the steps it shows.
    #[test]
    fn work_history_pages_read_only_what_they_show() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let state = state(root.path());
        let kdl = r#"version 2
mission "paged" state="ready" {
  goal "Run often."
  concurrent-runs max=100
  step "build" { assigned-to "agent/builder" }
  step "watch" { agentless }
}"#;
        let intent = parse_intent(kdl, "node").unwrap();
        let preview = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: kdl.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &preview.subject_tokens, "paged")
            .unwrap();
        let (mut builds, mut builder) = (Vec::new(), String::new());
        for run in 0..6 {
            let view = state
                .store
                .create_mission_run(&MissionRunRequest {
                    mission: "paged".into(),
                    revision: None,
                    workspace: workspace.display().to_string(),
                    requester: Some("person/operator".into()),
                    mode: None,
                    inputs: BTreeMap::new(),
                    idempotency_key: format!("paged-{run}"),
                })
                .unwrap();
            let build = view.steps.iter().find(|step| step.step == "build").unwrap();
            builder = build
                .assigned_to
                .clone()
                .expect("the build step is assigned");
            builds.push(build.subject.clone());
        }
        // Two builds change after the rest, so the history's newest-first order differs
        // from the order the steps were made in; the builder claims the first.
        for build in [&builds[4], &builds[0]] {
            state.store.set_step_state(build, "ready", None).unwrap();
        }
        state
            .store
            .work_action(
                &builds[0],
                "claim",
                &crate::model::WorkRequest {
                    actor: Some(builder.clone()),
                    incarnation: Some("builder-1".into()),
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: "claim-paged".into(),
                },
            )
            .unwrap();
        let (now, index) = (client_now_ms(), state.store.index().unwrap());
        let bare = builder.strip_prefix("agent/").unwrap();
        for actor in [
            None,
            Some(builder.as_str()),
            Some(bare),
            Some("agent/other"),
        ] {
            let whole = client_work_resources(&state.store, actor, true, now, index).unwrap();
            assert_eq!(
                whole.len(),
                match actor {
                    None => 12,
                    Some("agent/other") => 0,
                    Some(_) => 6,
                },
                "{actor:?}"
            );
            for limit in [1, 4, 5, 12, 50] {
                let mut offset = 0;
                loop {
                    let (page, has_more) =
                        client_work_history_page(&state.store, actor, now, index, offset, limit)
                            .unwrap();
                    let end = (offset + limit).min(whole.len());
                    assert_eq!(page, whole[offset..end], "{actor:?} {limit} {offset}");
                    assert_eq!(has_more, end < whole.len(), "{actor:?} {limit} {offset}");
                    offset = end;
                    if !has_more {
                        break;
                    }
                }
            }
        }
        crate::store::STEPS_ENRICHED.with(|enriched| enriched.set(0));
        let (page, has_more) =
            client_work_history_page(&state.store, None, now, index, 4, 3).unwrap();
        assert_eq!((page.len(), has_more), (3, true));
        assert_eq!(crate::store::STEPS_ENRICHED.with(std::cell::Cell::get), 3);
    }

    #[tokio::test]
    async fn one_targeted_planning_approval_also_approves_the_run_revision() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        let initial = r#"version 2
mission "targeted" state="ready" revisions="human-only" {
  goal "Use the initial mission."
  step "work" { agentless }
}
"#;
        let intent = parse_intent(initial, "node").unwrap();
        let preview = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: initial.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &preview.subject_tokens, "targeted-mission")
            .unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "targeted".into(),
                revision: None,
                workspace: workspace.display().to_string(),
                requester: Some("person/operator".into()),
                mode: None,
                inputs: BTreeMap::new(),
                idempotency_key: "targeted-run".into(),
            })
            .unwrap();
        let app = router(state);
        let (status, session) = json_request(
            app.clone(),
            "/v1/launches",
            serde_json::to_value(PlanningSessionStartRequest {
                mission: "ignored".into(),
                run: Some(run.subject.clone()),
                request: b"Add the final verification.".to_vec(),
                workspace: workspace.display().to_string(),
                requester: Some("person/operator".into()),
                provider: None,
                model: None,
                effort: None,
                idempotency_key: "targeted-session".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{session}");
        let session_id = session["id"].as_str().unwrap();
        let planner = session["planner"].as_str().unwrap();
        let candidate = br#"version 2
mission "targeted" state="ready" revisions="human-only" {
  goal "Use the reviewed mission."
  step "work" { agentless }
  step "verify" {
    agentless
    depends-on { step "work" completed }
  }
}
"#;
        let (status, submitted) = json_request(
            app.clone(),
            &format!("/v1/launches/{session_id}/submit"),
            serde_json::to_value(PlanningCandidateSubmitRequest {
                actor: planner.into(),
                markdown: b"# Targeted mission\n".to_vec(),
                kdl: candidate.to_vec(),
                idempotency_key: "targeted-candidate".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{submitted}");
        let preview_hash = submitted["preview"]["hash"].as_str().unwrap();
        let (status, approved) = json_request(
            app,
            &format!("/v1/launches/{session_id}/approve"),
            serde_json::to_value(PlanningApprovalRequest {
                actor: "person/operator".into(),
                preview_hash: preview_hash.into(),
                idempotency_key: "targeted-approval".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{approved}");
        assert_eq!(approved["status"], "approved");
        let revised = store.mission_run(&run.id).unwrap().unwrap();
        assert_ne!(revised.generation, run.generation);

        let approvals = store
            .events_after(0, None)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "revision-proposal.approved")
            .count();
        assert_eq!(approvals, 1);
    }

    #[tokio::test]
    async fn planning_compares_named_variants_and_proposes_from_one_generation() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        let source = r#"
version 2

  mission "variants" state="ready" {
    goal "Use the initial mission."
    step "work" { goal "Use the initial goal." }
  }

"#;
        let intent = parse_intent(source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &planned.subject_tokens, "variant-initial-mission")
            .unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "variants".into(),
                revision: None,
                workspace: workspace.display().to_string(),
                requester: Some("person/alex".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "variant-run".into(),
            })
            .unwrap();
        let app = router(state);
        let (status, started) = json_request(
            app.clone(),
            "/v1/launches",
            serde_json::to_value(PlanningSessionStartRequest {
                mission: "ignored-when-run-is-present".into(),
                run: Some(run.subject.clone()),
                request: b"Compare a compact mission with an extended mission.".to_vec(),
                workspace: workspace.display().to_string(),
                requester: Some("person/alex".into()),
                provider: None,
                model: None,
                effort: None,
                idempotency_key: "variant-session".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{started}");
        assert_eq!(started["target_mission_run"], run.subject);
        assert_eq!(started["source_generation"], run.generation);
        let session = started["id"].as_str().unwrap();
        let planner = started["planner"].as_str().unwrap();
        let compact = br#"
version 2

  mission "variants" state="ready" {
    goal "Use the compact mission."
    step "work" { goal "Use the compact goal." }
  }

"#;
        let extended = br#"
version 2

  mission "variants" state="ready" {
    goal "Use the extended mission."
    step "work" { goal "Use the extended goal." }
    step "verify" {
      depends-on { step "work" completed }
      goal "Verify the extended result."
    }
  }

"#;
        for (index, (name, kdl)) in [
            ("compact", compact.as_slice()),
            ("extended", extended.as_slice()),
        ]
        .into_iter()
        .enumerate()
        {
            let (status, submitted) = json_request(
                app.clone(),
                &format!("/v1/launches/{session}/variants/{name}/submit"),
                serde_json::to_value(PlanningCandidateSubmitRequest {
                    actor: planner.into(),
                    markdown: format!("# {name} mission\n").into_bytes(),
                    kdl: kdl.to_vec(),
                    idempotency_key: format!("variant-submit-{name}"),
                })
                .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{submitted}");
            assert_eq!(submitted["variants"].as_array().unwrap().len(), index + 1);
            let (status, previewed) = json_request(
                app.clone(),
                &format!("/v1/launches/{session}/variants/{name}/preview"),
                json!({}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{previewed}");
        }
        assert_eq!(
            store
                .claims_for(
                    &format!("planning-session/{session}"),
                    Some("planning-session.candidate-submitted"),
                )
                .unwrap()
                .len(),
            2
        );
        let (status, compared) = get_request(
            app.clone(),
            &format!("/v1/launches/{session}/variants/compact/compare/extended"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{compared}");
        assert_eq!(compared["left"]["name"], "compact");
        assert_eq!(compared["right"]["name"], "extended");

        let (status, proposed) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/variants/extended/propose"),
            serde_json::to_value(PlanningProposalRequest {
                actor: "person/alex".into(),
                reason: "the extended variant has the required check".into(),
                idempotency_key: "variant-propose-extended".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{proposed}");
        assert_eq!(proposed["status"], "applied");
        assert_ne!(proposed["mission_run"]["generation"], run.generation);

        let (status, retried) = json_request(
            app.clone(),
            &format!("/v1/launches/{session}/variants/extended/propose"),
            serde_json::to_value(PlanningProposalRequest {
                actor: "person/alex".into(),
                reason: "the extended variant has the required check".into(),
                idempotency_key: "variant-propose-extended".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{retried}");
        assert_eq!(
            retried["mission_run"]["generation"],
            proposed["mission_run"]["generation"]
        );

        let (status, stale) = json_request(
            app,
            &format!("/v1/launches/{session}/variants/compact/propose"),
            serde_json::to_value(PlanningProposalRequest {
                actor: "person/alex".into(),
                reason: "try the stale compact variant".into(),
                idempotency_key: "variant-propose-compact".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{stale}");
        assert_eq!(stale["code"], "stale-launch-generation");
    }

    #[tokio::test]
    async fn message_ingress_rejects_empty_content_before_it_enters_the_fifo() {
        let root = tempfile::tempdir().unwrap();
        let app = router(state(root.path()));
        let (status, body) = json_request(
            app,
            "/v1/messages",
            serde_json::to_value(MessageSendRequest {
                idempotency_key: "empty-message".into(),
                from: "agent/sender".into(),
                to: "agent/receiver".into(),
                content: " \n".into(),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                attachments: Vec::new(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["code"], "empty-message");
    }

    #[tokio::test]
    async fn a_runtime_work_message_uses_a_valid_daemon_sender() {
        let root = tempfile::tempdir().unwrap();
        let app = router(state(root.path()));
        let (status, body) = json_request(
            app,
            "/v1/messages",
            serde_json::to_value(MessageSendRequest {
                idempotency_key: "runtime-work-message".into(),
                from: "daemon/runtime".into(),
                to: "agent/worker".into(),
                content: "A durable mission step is ready.".into(),
                title: Some("Mission step ready".into()),
                in_reply_to: None,
                tags: vec!["st3-work:step-run/run/build@1@1@incarnation".into()],
                attachments: Vec::new(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["from"], "daemon/runtime");
        assert_eq!(body["to"], "agent/worker");
    }

    #[tokio::test]
    async fn a_requester_message_uses_a_full_person_subject() {
        let root = tempfile::tempdir().unwrap();
        let app = router(state(root.path()));
        let (status, body) = json_request(
            app,
            "/v1/messages",
            serde_json::to_value(MessageSendRequest {
                idempotency_key: "requester-message".into(),
                from: "requester".into(),
                to: "agent/worker".into(),
                content: "Please do the work.".into(),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                attachments: Vec::new(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["from"], "person/requester");
    }

    #[tokio::test]
    async fn replying_settles_the_original_across_native_delivery_transports() {
        let root = tempfile::tempdir().unwrap();
        let app = router(state(root.path()));
        for transport in [
            "omp-channel",
            "pi-channel",
            "claude-channel",
            "codex-app-server",
            "opencode-server",
        ] {
            let (status, original) = json_request(
                app.clone(),
                "/v1/messages",
                serde_json::to_value(MessageSendRequest {
                    idempotency_key: format!("replay-original-{transport}"),
                    from: "agent/sender".into(),
                    to: "agent/receiver".into(),
                    content: "Please answer once.".into(),
                    title: None,
                    in_reply_to: None,
                    tags: Vec::new(),
                    attachments: Vec::new(),
                })
                .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{original}");
            let subject = original["subject"].as_str().unwrap();
            let path = format!(
                "/v1/messages/{}/claims",
                subject.trim_start_matches("message/")
            );
            for lifecycle in ["staged", "delivered"] {
                let (status, claim) = json_request(
                    app.clone(),
                    &path,
                    serde_json::to_value(MessageLifecycleRequest {
                        lifecycle: lifecycle.into(),
                        actor: Some("agent/receiver".into()),
                        transport: Some(transport.into()),
                        runtime_id: None,
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: format!("replay-{transport}-{lifecycle}"),
                    })
                    .unwrap(),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{claim}");
            }
            let reply = serde_json::to_value(MessageSendRequest {
                idempotency_key: format!("replay-reply-{transport}"),
                from: "agent/receiver".into(),
                to: "agent/sender".into(),
                content: "Answered.".into(),
                title: None,
                in_reply_to: Some(subject.into()),
                tags: Vec::new(),
                attachments: Vec::new(),
            })
            .unwrap();
            let (status, sent) = json_request(app.clone(), "/v1/messages", reply.clone()).await;
            assert_eq!(status, StatusCode::OK, "{sent}");
            let (status, repeated) = json_request(app.clone(), "/v1/messages", reply).await;
            assert_eq!(status, StatusCode::OK, "{repeated}");
            // The repeat is told it found the first reply, not that it sent one.
            assert_eq!(sent["already_sent"], false, "{sent}");
            assert_eq!(repeated["already_sent"], true, "{repeated}");
            assert_eq!(repeated["subject"], sent["subject"]);
            assert_eq!(repeated["sent_at"], sent["sent_at"]);
            let (_, settled) = get_request(
                app.clone(),
                &format!(
                    "/v1/messages/read/{}",
                    subject.trim_start_matches("message/")
                ),
            )
            .await;
            assert_eq!(settled["status"], "closed", "{transport}: {settled}");
        }
        let (_, mailbox) =
            get_request(app, "/v1/messages/page?to=agent%2Freceiver&limit=100").await;
        assert!(mailbox["items"].as_array().unwrap().is_empty(), "{mailbox}");
    }

    #[tokio::test]
    async fn a_sender_followup_does_not_settle_the_recipient_message() {
        let root = tempfile::tempdir().unwrap();
        let app = router(state(root.path()));
        let send = |key: &str, from: &str, to: &str, parent: Option<String>| {
            serde_json::to_value(MessageSendRequest {
                idempotency_key: key.into(),
                from: from.into(),
                to: to.into(),
                content: "Please respond.".into(),
                title: None,
                in_reply_to: parent,
                tags: Vec::new(),
                attachments: Vec::new(),
            })
            .unwrap()
        };
        let (_, first) = json_request(
            app.clone(),
            "/v1/messages",
            send("followup-original", "agent/sender", "agent/receiver", None),
        )
        .await;
        let subject = first["subject"].as_str().unwrap();
        let (status, _) = json_request(
            app.clone(),
            "/v1/messages",
            send(
                "followup-same-sender",
                "agent/sender",
                "agent/receiver",
                Some(subject.into()),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, original) = get_request(
            app,
            &format!(
                "/v1/messages/read/{}",
                subject.trim_start_matches("message/")
            ),
        )
        .await;
        assert_eq!(original["status"], "sent");
    }

    #[tokio::test]
    async fn message_lifecycle_requires_the_exact_recipient_actor() {
        let root = tempfile::tempdir().unwrap();
        let app = router(state(root.path()));
        let (status, message) = json_request(
            app.clone(),
            "/v1/messages",
            serde_json::to_value(MessageSendRequest {
                idempotency_key: "recipient-authority-message".into(),
                from: "agent/sender".into(),
                to: "person/receiver".into(),
                content: "Please review this.".into(),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                attachments: Vec::new(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{message}");
        let message_id = message["subject"]
            .as_str()
            .unwrap()
            .trim_start_matches("message/");
        let path = format!("/v1/messages/{message_id}/claims");

        let (status, missing) = json_request(
            app.clone(),
            &path,
            serde_json::to_value(MessageLifecycleRequest {
                lifecycle: "read".into(),
                actor: None,
                transport: None,
                runtime_id: None,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: "missing-message-actor".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{missing}");
        assert_eq!(missing["code"], "missing-message-actor");

        let (status, wrong) = json_request(
            app.clone(),
            &path,
            serde_json::to_value(MessageLifecycleRequest {
                lifecycle: "read".into(),
                actor: Some("person/intruder".into()),
                transport: None,
                runtime_id: None,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: "wrong-message-actor".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{wrong}");
        assert_eq!(wrong["code"], "wrong-message-recipient");

        let (status, staged) = json_request(
            app.clone(),
            &path,
            serde_json::to_value(MessageLifecycleRequest {
                lifecycle: "staged".into(),
                actor: Some("person/receiver".into()),
                transport: Some("codex-app-server".into()),
                runtime_id: Some("runtime/receiver".into()),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: "staged-by-recipient-driver".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{staged}");
        assert_eq!(staged["body"]["fields"]["status"], "staged");
        assert_eq!(staged["body"]["fields"]["recipient"], "person/receiver");
        assert_eq!(staged["body"]["fields"]["transport"], "codex-app-server");
        assert_eq!(staged["body"]["fields"]["runtime_id"], "runtime/receiver");

        let (status, read) = json_request(
            app.clone(),
            &path,
            serde_json::to_value(MessageLifecycleRequest {
                lifecycle: "delivered".into(),
                actor: Some("person/receiver".into()),
                transport: None,
                runtime_id: None,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: "right-message-actor".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{read}");
        assert_eq!(read["actor"], "person/receiver");

        let legacy = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages/close/obsolete")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(legacy.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn eval_upload_posts_its_staged_documents_before_apply() {
        let root = tempfile::tempdir().unwrap();
        let eval_dir = tempfile::tempdir().unwrap();
        let bytes = b"hello from the eval";
        let hash = hex::encode(Sha256::digest(bytes));
        fs::create_dir_all(eval_dir.path().join(".st3-documents")).unwrap();
        fs::write(eval_dir.path().join(".st3-documents").join(&hash), bytes).unwrap();
        fs::write(
            eval_dir.path().join("eval.kdl"),
            format!(
                r#"
version 2

  agent "eval/demo/worker" {{
    workspace "${{EVAL_ROOT}}"
    command "true"
    restart "never"
  }}

  mission "eval/demo" state="ready" timeout="2m" {{
    goal "Complete mission eval/demo."
    baseline "document-content" {{ has "doc/evals/demo/task@{hash}" "hello" }}
    step "document" {{
      agentless
      title "The document exists"

        message "task" {{ to "person/worker"; content "doc/evals/demo/task@{hash}" }}

    }}
    completion {{ when "all-steps-exhausted" }}
  }}

"#
            ),
        )
        .unwrap();
        let bundle = crate::archive::archive_eval(eval_dir.path()).unwrap();
        let bundle_hash = hex::encode(Sha256::digest(&bundle));
        let state = state(root.path());
        let store = state.store.clone();
        let app = router(state);
        let (status, body) = json_request(
            app.clone(),
            "/v1/evals",
            serde_json::to_value(EvalStartRequest {
                name: "demo".into(),
                bundle_hash,
                bundle,
                inputs: BTreeMap::new(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["mission_run"]
                .as_str()
                .unwrap()
                .starts_with("mission-run/")
        );
        let eval_run = body["mission_run"].as_str().unwrap();
        let (status, eval_status) = get_request(
            app.clone(),
            &format!("/v1/evals/{}", urlencoding::encode(eval_run)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{eval_status}");
        assert_eq!(eval_status["mission_run"], body["mission_run"]);
        assert_eq!(eval_status["lifecycle"], "running");
        assert!(eval_status.get("active_checkpoint").is_none());
        let root = body["mission_run"].as_str().unwrap();
        let eval_seat = store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|subject| subject.subject == "agent/eval/demo/worker")
            .expect("the eval did not apply its mission-less seat");
        assert_eq!(eval_seat.owner_run.as_deref(), Some(root));
        let (status, mission_runs) = get_request(
            app,
            &format!("/v1/mission-runs?root={}", urlencoding::encode(root)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{mission_runs}");
        assert_eq!(mission_runs.as_array().unwrap().len(), 1);
        assert_eq!(mission_runs[0]["subject"], root);
        assert_eq!(
            store
                .get_document("doc/evals/demo/task", &hash)
                .unwrap()
                .unwrap(),
            bytes
        );
    }

    #[tokio::test]
    async fn the_same_eval_bundle_can_run_again_with_a_new_workspace() {
        let root = tempfile::tempdir().unwrap();
        let eval_dir = tempfile::tempdir().unwrap();
        fs::write(
            eval_dir.path().join("eval.kdl"),
            r#"version 2
mission "eval/repeat" state="ready" timeout="2m" {
  concurrent-runs max=2
  goal "Run the same archived eval in a fresh workspace."
  completion { when "all-steps-exhausted" }
  step "done" {
    agentless
    gate "the current eval workspace exists" {
      exec "test -d '${EVAL_ROOT}'"
      host "local"
      workspace "${EVAL_ROOT}"
      time-limit "1m"
    }
  }
}
"#,
        )
        .unwrap();
        let bundle = crate::archive::archive_eval(eval_dir.path()).unwrap();
        let bundle_hash = hex::encode(Sha256::digest(&bundle));
        let app = router(state(root.path()));

        let request = || {
            serde_json::to_value(EvalStartRequest {
                name: "repeat".into(),
                bundle_hash: bundle_hash.clone(),
                bundle: bundle.clone(),
                inputs: BTreeMap::new(),
            })
            .unwrap()
        };
        let (first_status, first) = json_request(app.clone(), "/v1/evals", request()).await;
        let (second_status, second) = json_request(app, "/v1/evals", request()).await;

        assert_eq!(first_status, StatusCode::OK, "{first}");
        assert_eq!(second_status, StatusCode::OK, "{second}");
        assert_ne!(first["mission_run"], second["mission_run"]);
    }

    #[tokio::test]
    async fn eval_entries_require_a_bounded_twenty_minute_timeout() {
        for (timeout, code) in [
            ("", "missing-eval-timeout"),
            (" timeout=\"21m\"", "eval-timeout-too-large"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let eval_dir = tempfile::tempdir().unwrap();
            fs::write(
                eval_dir.path().join("eval.kdl"),
                format!(
                    r#"version 2
mission "eval/deadline" state="ready"{timeout} {{
  goal "Prove the eval deadline is explicit and bounded."
  completion {{ when "all-steps-exhausted" }}
  step "done" {{ agentless }}
}}
"#
                ),
            )
            .unwrap();
            let bundle = crate::archive::archive_eval(eval_dir.path()).unwrap();
            let bundle_hash = hex::encode(Sha256::digest(&bundle));
            let app = router(state(root.path()));

            let (status, body) = json_request(
                app,
                "/v1/evals",
                serde_json::to_value(EvalStartRequest {
                    name: "deadline".into(),
                    bundle_hash,
                    bundle,
                    inputs: BTreeMap::new(),
                })
                .unwrap(),
            )
            .await;

            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(body["code"], code, "{body}");
        }
    }

    #[test]
    fn eval_fixtures_are_owned_without_replacing_selected_desired_state() {
        let current = parse_intent(
            r#"version 2
host "node" { document "doc/hosts/production@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }
"#,
            "node",
        )
        .unwrap()
        .subjects
        .into_values()
        .collect::<Vec<_>>();
        let mut conflicting = parse_intent(
            r#"version 2
host "local" { document "doc/hosts/eval@bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" }
"#,
            "node",
        )
        .unwrap();

        let error = scope_eval_desired_subjects(&mut conflicting, &current, "mission-run/eval-run")
            .unwrap_err();
        assert_eq!(error.code, "eval-desired-conflict");
        assert_eq!(current[0].subject, "host/node");
        assert_eq!(
            current[0].desired["children"][0]["arguments"][0],
            "doc/hosts/production@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );

        let mut fixture = parse_intent(
            r#"version 2
resource "eval/fixture" { kind "filesystem.file" }
agent "eval/fixture/worker" { workspace "/tmp"; command "true"; restart "never" }
"#,
            "node",
        )
        .unwrap();
        scope_eval_desired_subjects(&mut fixture, &current, "mission-run/eval-run").unwrap();
        assert_eq!(
            fixture.subjects["resource/eval/fixture"]
                .owner_run
                .as_deref(),
            Some("mission-run/eval-run")
        );
        assert_eq!(
            fixture.subjects["agent/eval/fixture/worker"]
                .owner_run
                .as_deref(),
            Some("mission-run/eval-run")
        );
    }

    #[test]
    fn eval_reuses_an_identical_selected_declaration_without_owning_it() {
        let mut intent = parse_intent(
            r#"version 2
resource "shared" { kind "filesystem.file" }
"#,
            "node",
        )
        .unwrap();
        let current = intent.subjects.values().cloned().collect::<Vec<_>>();

        scope_eval_desired_subjects(&mut intent, &current, "mission-run/eval-run").unwrap();

        assert!(intent.subjects.is_empty());
    }

    #[tokio::test]
    async fn an_eval_cannot_replace_selected_host_metadata() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let production = state
            .store
            .put_document(
                "doc/hosts/node",
                b"Production host facts.\n",
                &None,
                "production-host",
            )
            .unwrap();
        let source = format!(
            "version 2\nhost \"node\" {{ document \"doc/hosts/node@{}\" }}\n",
            production.hash
        );
        let intent = parse_intent(&source, "node").unwrap();
        let preview = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source,
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &preview.subject_tokens, "publish-production-host")
            .unwrap();

        let eval_dir = tempfile::tempdir().unwrap();
        let eval_host = b"Eval host facts.\n";
        let eval_hash = hex::encode(Sha256::digest(eval_host));
        fs::create_dir_all(eval_dir.path().join(".st3-documents")).unwrap();
        fs::write(
            eval_dir.path().join(".st3-documents").join(&eval_hash),
            eval_host,
        )
        .unwrap();
        fs::write(
            eval_dir.path().join("eval.kdl"),
            format!(
                r#"version 2
host "local" {{ document "doc/hosts/eval@{eval_hash}" }}
mission "eval/host-conflict" state="ready" timeout="1m" {{
  goal "Do not replace production host metadata."
  completion {{ when "all-steps-exhausted" }}
  step "done" {{ agentless }}
}}
"#
            ),
        )
        .unwrap();
        let bundle = crate::archive::archive_eval(eval_dir.path()).unwrap();
        let bundle_hash = hex::encode(Sha256::digest(&bundle));
        let app = router(state.clone());

        let (status, body) = json_request(
            app,
            "/v1/evals",
            serde_json::to_value(EvalStartRequest {
                name: "host-conflict".into(),
                bundle_hash,
                bundle,
                inputs: BTreeMap::new(),
            })
            .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["code"], "eval-desired-conflict");
        let selected = state
            .store
            .desired_subjects()
            .unwrap()
            .into_iter()
            .find(|desired| desired.subject == "host/node")
            .unwrap();
        assert_eq!(
            selected.desired["children"][0]["arguments"][0],
            format!("doc/hosts/node@{}", production.hash)
        );
    }

    #[tokio::test]
    async fn an_eval_can_publish_ready_helper_missions() {
        let root = tempfile::tempdir().unwrap();
        let eval_dir = tempfile::tempdir().unwrap();
        fs::write(
            eval_dir.path().join("eval.kdl"),
            r#"
version 2

mission "eval/demo/helper" state="ready" {
  goal "Supply one helper mission."
  completion { when "all-steps-exhausted" }
  step "done" { agentless }
}

mission "eval/demo" state="ready" timeout="2m" {
  goal "Run the eval entry mission."
  completion { when "all-steps-exhausted" }
  step "done" { agentless }
}
"#,
        )
        .unwrap();
        let bundle = crate::archive::archive_eval(eval_dir.path()).unwrap();
        let bundle_hash = hex::encode(Sha256::digest(&bundle));
        let state = state(root.path());
        let store = state.store.clone();
        let app = router(state);

        let (status, body) = json_request(
            app,
            "/v1/evals",
            serde_json::to_value(EvalStartRequest {
                name: "demo".into(),
                bundle_hash,
                bundle,
                inputs: BTreeMap::new(),
            })
            .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        let run = store
            .mission_run(body["mission_run"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(run.mission, "mission/eval/demo");
    }

    #[tokio::test]
    async fn manual_work_wake_is_idempotent_and_uses_the_durable_driver_inbox() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let source = r#"
version 2

mission "wake" state="ready" {
  goal "Exercise manual work wake."
  agent "worker" { workspace "/tmp"; command "true" }
  step "work" { assigned-to "agent/${ST_MISSION_RUN}/worker" }
}
"#;
        let intent = parse_intent(source, "node").unwrap();
        let planned = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &planned.subject_tokens, "manual-wake-source")
            .unwrap();
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "wake".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "manual-wake-run".into(),
            })
            .unwrap();
        materialize_run_agents(&state, &run);
        let step = &run.steps[0];
        let agent = format!("agent/{}/worker", run.id);
        state
            .store
            .set_step_state(&step.subject, "ready", None)
            .unwrap();
        state
            .store
            .append_claim(&ClaimInput {
                subject: agent.clone(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    ("runtime_id".into(), Value::String("wake-worker".into())),
                    (
                        "incarnation_id".into(),
                        Value::String("wake-incarnation".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("manual-wake-runtime".into()),
            })
            .unwrap();
        state
            .store
            .append_claim(&ClaimInput {
                subject: agent.clone(),
                kind: "harness.observed".into(),
                actor: Some(agent.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("idle".into())),
                    ("driver".into(), Value::String("codex".into())),
                    (
                        "incarnation_id".into(),
                        Value::String("wake-incarnation".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("manual-wake-harness".into()),
            })
            .unwrap();

        let app = router(state.clone());
        let request = serde_json::to_value(WorkWakeRequest {
            actor: "person/operator".into(),
            reason: "the operator requested another delivery".into(),
            idempotency_key: "manual-wake-request".into(),
        })
        .unwrap();
        let path = format!("/v1/work/wake/{}", urlencoding::encode(&step.subject));
        let (status, first) = json_request(app.clone(), &path, request.clone()).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["status"], "sent");
        assert!(
            first["tags"]
                .as_array()
                .unwrap()
                .contains(&Value::String("st3-wake-source:manual".into()))
        );
        let (status, repeated) = json_request(app, &path, request).await;
        assert_eq!(status, StatusCode::OK, "{repeated}");
        assert_eq!(repeated["subject"], first["subject"]);
        assert_eq!(state.store.messages(Some(&agent), true).unwrap().len(), 1);
        let app = router(state.clone());
        let (status, denied) = json_request(
            app.clone(),
            &path,
            serde_json::to_value(WorkWakeRequest {
                actor: "daemon/runtime".into(),
                reason: "neither a person nor an agent attempted a wake".into(),
                idempotency_key: "manual-wake-foreign".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{denied}");
        assert_eq!(denied["code"], "invalid-wake-actor");
        // Free mode: any agent wakes work assigned to another seat.
        for (n, actor) in [(2, "agent/other"), (3, "person/operator")] {
            let (status, wake) = json_request(
                app.clone(),
                &path,
                serde_json::to_value(WorkWakeRequest {
                    actor: actor.into(),
                    reason: "retry the manual delivery".into(),
                    idempotency_key: format!("manual-wake-request-{n}"),
                })
                .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{wake}");
        }
        let projected = state.store.step_run(&step.subject).unwrap().unwrap();
        let wake = projected.wake.expect("wake projection");
        assert_eq!(
            wake.attempts, 0,
            "manual wakes must not use automatic attempts"
        );
        assert_eq!(wake.assignee_state, "idle");
    }

    #[test]
    fn agents_name_their_queued_steps_and_missions_carry_open_run_steps() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        let source = r#"
version 2
mission "labelled" state="ready" {
  goal "Name steps where clients read them."
  agent "worker" { workspace "/tmp"; harness "codex" {} }
  step "first" { title "Say hello"; goal "Greet the fleet."; assigned-to "agent/${ST_MISSION_RUN}/worker" }
  step "second" { goal "Wave goodbye."; depends-on "first"; assigned-to "agent/${ST_MISSION_RUN}/worker" }
}
"#;
        let intent = parse_intent(source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &planned.subject_tokens, "labelled-source")
            .unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "labelled".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "labelled-run".into(),
            })
            .unwrap();
        materialize_run_agents(&state, &run);
        let first = run
            .steps
            .iter()
            .find(|step| step.step == "first")
            .unwrap()
            .subject
            .clone();
        store.set_step_state(&first, "ready", None).unwrap();
        let index = store.index().unwrap();

        let agents = client_agent_resources(&store, false, "snapshot", index).unwrap();
        let next = &agents[0]["next_work"];
        assert_eq!(next["id"], first);
        assert_eq!(next["mission_id"], "mission/labelled");
        assert_eq!(next["mission_run_id"], run.subject);
        assert_eq!(next["path"], "first");
        assert_eq!(next["title"], "Say hello");
        assert_eq!(next["goal"], "Greet the fleet.");
        assert_eq!(next["state"], "ready");
        assert_eq!(agents[0]["upcoming_work"], json!([next]));
        assert_eq!(agents[0]["current_work"], json!([]));

        // The list carries the open run's steps, so a client never joins work to missions.
        let missions = client_v0::mission_resources(&store, index, false, None).unwrap();
        let steps = missions[0]["run_details"][0]["steps"].as_array().unwrap();
        assert_eq!(
            steps
                .iter()
                .map(|step| (
                    step["path"].as_str().unwrap(),
                    step["state"].as_str().unwrap()
                ))
                .collect::<Vec<_>>(),
            [("first", "ready"), ("second", "waiting")]
        );
        assert_eq!(steps[0]["goals"], json!(["Greet the fleet."]));
        assert_eq!(steps[0]["assignee"], format!("agent/{}/worker", run.id));
    }

    #[test]
    fn agent_state_requires_current_harness_evidence_for_native_drivers() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        let source = r#"
version 2
mission "agent-health" state="ready" {
  goal "Exercise agent health projection."
  agent "worker" { workspace "/tmp"; harness "codex" {} }
  step "queued" { assigned-to "agent/${ST_MISSION_RUN}/worker" }
}
"#;
        let intent = parse_intent(source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &planned.subject_tokens, "agent-health-source")
            .unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "agent-health".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "agent-health-run".into(),
            })
            .unwrap();
        materialize_run_agents(&state, &run);
        let subject = format!("agent/{}/worker", run.id);
        let queued = run.steps[0].subject.clone();
        store.set_step_state(&queued, "ready", None).unwrap();
        store
            .append_claim(&ClaimInput {
                subject: subject.clone(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    ("runtime_id".into(), Value::String("node.worker".into())),
                    (
                        "incarnation_id".into(),
                        Value::String("incarnation-1".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("agent-health-runtime".into()),
            })
            .unwrap();
        let resources =
            client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
        assert_eq!(resources[0]["state"], "starting");
        assert_eq!(resources[0]["host_id"], "host/node");
        assert!(resources[0]["last_activity_at"].is_null());
        assert_eq!(resources[0]["next_work_id"], queued);
        assert_eq!(resources[0]["upcoming_work_ids"], json!([queued]));
        assert_eq!(resources[0]["queued_work_count"], 1);

        store
            .append_claim(&ClaimInput {
                subject: subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("idle".into())),
                    ("driver".into(), Value::String("codex".into())),
                    (
                        "incarnation_id".into(),
                        Value::String("incarnation-1".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("agent-health-ready".into()),
            })
            .unwrap();
        let resources =
            client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
        assert_eq!(resources[0]["state"], "running");

        store
            .append_claim(&ClaimInput {
                subject: subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(subject.clone()),
                fields: BTreeMap::from([
                    ("state".into(), Value::String("ended".into())),
                    ("driver".into(), Value::String("codex".into())),
                    (
                        "incarnation_id".into(),
                        Value::String("incarnation-1".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("agent-health-ended".into()),
            })
            .unwrap();
        let resources =
            client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
        assert_eq!(resources[0]["state"], "failed");
        store
            .append_claim(&ClaimInput {
                subject: subject.clone(),
                kind: "runtime.observed".into(),
                actor: None,
                fields: BTreeMap::from([
                    ("status".into(), Value::String("failed".into())),
                    ("runtime_id".into(), Value::String("node.worker".into())),
                    (
                        "incarnation_id".into(),
                        Value::String("incarnation-1".into()),
                    ),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("agent-health-crashed".into()),
            })
            .unwrap();
        let resources =
            client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
        assert_eq!(
            resources.len(),
            1,
            "current unhealthy agents must remain visible"
        );
        assert_eq!(resources[0]["state"], "failed");
        assert_eq!(resources[0]["operational"]["layer"], "current");
    }

    #[test]
    fn human_blocking_overrides_activity_but_not_runtime_or_harness_fences() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        // A run-owned seat carries its declared harness driver, so a new incarnation without
        // a harness observation is `starting` rather than a driverless wrapper's `running`.
        let source = r#"
version 2
mission "agent-human" state="ready" {
  goal "Exercise human blocking projection."
  agent "worker" { workspace "/tmp"; harness "omp" {} }
  step "queued" { assigned-to "agent/${ST_MISSION_RUN}/worker" }
}
"#;
        let intent = parse_intent(source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &planned.subject_tokens, "agent-human-source")
            .unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "agent-human".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "agent-human-run".into(),
            })
            .unwrap();
        materialize_run_agents(&state, &run);
        let subject = format!("agent/{}/worker", run.id);
        let append = |kind: &str, fields: Value| {
            store
                .append_claim(&ClaimInput {
                    subject: subject.clone(),
                    kind: kind.into(),
                    actor: None,
                    fields: serde_json::from_value(fields).unwrap(),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        };
        let observe_runtime = |status: &str| {
            append(
                "runtime.observed",
                json!({
                    "status": status, "runtime_id": "node.worker", "incarnation_id": "human-1",
                }),
            );
        };
        let observe_harness = |activity: &str| {
            append(
                "harness.observed",
                json!({
                    "state": activity, "driver": "omp", "incarnation_id": "human-1",
                    "blocked_on": "human", "ask": "permission", "reason": "Deploy production?",
                }),
            );
        };
        // Exercise the canonical graph projection independently of process-local delivery health.
        let agent = || {
            client_agent_resources_uncached(&store, true, store.index().unwrap())
                .unwrap()
                .into_iter()
                .find(|agent| agent["id"] == subject.as_str())
                .unwrap()
        };
        observe_runtime("running");
        for activity in ["working", "idle", "ready"] {
            observe_harness(activity);
            assert_eq!(agent()["state"], "waiting", "{activity}");
        }
        append("harness.observed", json!({
            "state": "working", "driver": "omp", "incarnation_id": "human-1",
            "blocked_on": null, "ask": null, "reason": null, "input_buffer": null, "exit": null,
        }));
        let answered: st3_client::Agent = serde_json::from_value(agent()).unwrap();
        assert_eq!(answered.state, "running");
        assert_eq!(answered.harness_state.as_deref(), Some("working"));
        assert!(answered.blocked_on.is_none());
        assert!(answered.ask.is_none());
        assert!(answered.reason.is_none());
        // The harness schema's terminal activity keeps precedence over a stale ask.
        observe_harness("ended");
        assert_eq!(agent()["state"], "failed");
        // Indeterminate activity keeps its existing waiting verdict; clients must not
        // present it as an answerable human ask.
        observe_harness("indeterminate");
        assert_eq!(agent()["state"], "waiting");
        assert_eq!(agent()["harness_state"], "indeterminate");
        observe_harness("working");
        observe_runtime("stopped");
        assert_eq!(agent()["state"], "stopped");
        observe_runtime("starting");
        assert_eq!(agent()["state"], "starting");
        observe_runtime("running");
        append("runtime.reconcile-decision", json!({
            "key": "member-reconcile", "decision": "member-fault", "reason": "Cannot reconcile seat",
        }));
        assert_eq!(agent()["state"], "failed");
        append("runtime.reconcile-decision", json!({
            "key": "member-reconcile", "decision": "member-started",
        }));
        append("runtime.observed", json!({
            "status": "running", "runtime_id": "node.worker", "incarnation_id": "human-2",
        }));
        // Before a new incarnation's first observation, the previous ask is fenced out.
        assert_eq!(agent()["state"], "starting", "{}", agent());
        assert!(agent()["blocked_on"].is_null());
        append("harness.observed", json!({
            "state": "idle", "driver": "omp", "incarnation_id": "human-2",
            "blocked_on": null, "ask": null, "reason": null, "input_buffer": null, "exit": null,
        }));
        observe_harness("working");
        let resumed: st3_client::Agent = serde_json::from_value(agent()).unwrap();
        assert_eq!(resumed.state, "running");
        assert_eq!(resumed.harness_state.as_deref(), Some("idle"));
        assert_eq!(resumed.incarnation_id.as_deref(), Some("human-2"));
        assert!(resumed.blocked_on.is_none());
        assert!(resumed.ask.is_none());
        assert!(resumed.reason.is_none());
    }

    #[test]
    fn an_unauthenticated_harness_is_waiting_not_starting() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        let source = r#"
version 2
mission "agent-auth" state="ready" {
  goal "Exercise an unauthenticated harness."
  agent "worker" { workspace "/tmp"; harness "claude" {} }
  step "queued" { assigned-to "agent/${ST_MISSION_RUN}/worker" }
}
"#;
        let intent = parse_intent(source, "node").unwrap();
        let planned = store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply(&intent, &planned.subject_tokens, "agent-auth-source")
            .unwrap();
        let run = store
            .create_mission_run(&MissionRunRequest {
                mission: "agent-auth".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "agent-auth-run".into(),
            })
            .unwrap();
        materialize_run_agents(&state, &run);
        let subject = format!("agent/{}/worker", run.id);
        for (kind, fields, key) in [
            (
                "runtime.observed",
                BTreeMap::from([
                    ("status".into(), Value::String("running".into())),
                    ("runtime_id".into(), Value::String("node.worker".into())),
                    ("incarnation_id".into(), Value::String("worker-1".into())),
                ]),
                "agent-auth-runtime",
            ),
            (
                "harness.diagnostic",
                BTreeMap::from([
                    ("severity".into(), Value::String("error".into())),
                    ("status".into(), Value::String("unauthenticated".into())),
                    ("code".into(), Value::String("provider-auth-expired".into())),
                    (
                        "reason".into(),
                        Value::String("Claude reports an expired login".into()),
                    ),
                    ("incarnation_id".into(), Value::String("worker-1".into())),
                ]),
                "agent-auth-expired",
            ),
        ] {
            store
                .append_claim(&ClaimInput {
                    subject: subject.clone(),
                    kind: kind.into(),
                    actor: (kind == "harness.diagnostic").then(|| subject.clone()),
                    fields,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(key.into()),
                })
                .unwrap();
        }
        let resources =
            client_agent_resources(&store, false, "snapshot", store.index().unwrap()).unwrap();
        assert_eq!(resources[0]["harness_state"], "unauthenticated");
        assert_eq!(resources[0]["state"], "waiting");
    }

    #[tokio::test]
    async fn a_run_agent_revises_its_mission() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let source = r#"
version 2

  mission "revision" state="ready" {
    goal "Complete mission revision."
     agent "sup" {
       workspace "."
       command "true"
       mission-authority { revise "revision" }
     }
    step "work" { goal "First goal." }
  }

"#;
        let intent = parse_intent(source, "node").unwrap();
        let planned = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &planned.subject_tokens, "revision-mission-one")
            .unwrap();
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "revision".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "revision-run".into(),
            })
            .unwrap();
        materialize_run_agents(&state, &run);
        let actor = format!("agent/{}/sup", run.id);
        let app = router(state.clone());
        let replacement = r#"
version 2

  mission "revision" state="ready" {
    goal "Complete mission revision."
     agent "sup" {
       workspace "."
       command "true"
       mission-authority { revise "revision" }
     }
    step "work" { goal "Corrected goal." }
  }

"#;
        let (status, revised) = json_request(
            app,
            &format!("/v1/mission-runs/{}/revision", run.id),
            serde_json::to_value(MissionRevisionRequest {
                intent: crate::model::IntentInput {
                    kdl: replacement.into(),
                    source_name: None,
                },
                actor,
                reason: "the first goal was incomplete".into(),
                idempotency_key: "revision-two".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{revised}");
        assert_eq!(revised["status"], "applied");
        assert_ne!(revised["mission_run"]["revision"], run.revision);
        assert_eq!(revised["mission_run"]["root_revision"], run.root_revision);
        assert_eq!(revised["mission_run"]["steps"][0]["status"], "pending");
        assert_eq!(state.store.desired_subjects().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_mission_revision_refuses_a_reference_that_does_not_resolve() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let source = r#"version 2
mission "revision" state="ready" {
  goal "Complete mission revision."
  step "work" { agentless }
}"#;
        state
            .store
            .apply_internal(&parse_intent(source, "node").unwrap(), "revision-mission")
            .unwrap();
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "revision".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "revision-run".into(),
            })
            .unwrap();
        let before = state.store.index().unwrap();
        let (status, error) = json_request(
            router(state.clone()),
            &format!("/v1/mission-runs/{}/revision", run.id),
            serde_json::to_value(MissionRevisionRequest {
                intent: crate::model::IntentInput {
                    kdl: source.replace("agentless", r#"assigned-to "agent/example/nobody""#),
                    source_name: None,
                },
                actor: "person/test".into(),
                reason: "hand the work to an agent".into(),
                idempotency_key: "revision-missing-agent".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
        assert_eq!(error["code"], "mission-revision-blocked");
        assert!(
            error["message"].as_str().unwrap().contains(
                "mission `mission/revision` references missing eligible agent `agent/example/nobody`"
            ),
            "{error}"
        );
        assert_eq!(state.store.index().unwrap(), before);
    }

    #[tokio::test]
    async fn any_agent_retries_failed_work_as_itself() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let source = r#"
version 2

  mission "retry" state="ready" {
    goal "Retry one failed check."
    agent "worker" { workspace "."; command "true" }
    step "check" { goal "Run the check." }
  }

"#;
        let intent = parse_intent(source, "node").unwrap();
        let planned = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &planned.subject_tokens, "retry-mission")
            .unwrap();
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "retry".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "retry-run".into(),
            })
            .unwrap();
        materialize_run_agents(&state, &run);
        let check = run.steps[0].subject.clone();
        state
            .store
            .set_step_state(&check, "failed", Some("the check failed"))
            .unwrap();
        let retry = |actor: String, key: &str| {
            serde_json::to_value(WorkRetryRequest {
                actor,
                reason: "the check host is back".into(),
                idempotency_key: key.into(),
            })
            .unwrap()
        };
        let path = format!("/v1/work/retry/{}", urlencoding::encode(&check));

        let (status, denied) = json_request(
            router(state.clone()),
            &path,
            retry("daemon/runtime".into(), "retry-unknown"),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{denied}");
        assert_eq!(denied["code"], "retry-authority-denied");

        let worker = format!("agent/{}/worker", run.id);
        let (status, retried) = json_request(
            router(state.clone()),
            &path,
            retry(worker.clone(), "retry-worker"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{retried}");
        assert_eq!(retried["steps"][0]["status"], "pending");
        assert_eq!(retried["steps"][0]["attempt"], 2);
        assert!(
            state
                .store
                .claims_for(&check, None)
                .unwrap()
                .iter()
                .any(|claim| claim.actor.as_deref() == Some(worker.as_str())),
            "the retry records the agent as its actor"
        );
    }

    #[tokio::test]
    async fn any_agent_sets_a_run_outcome_and_retires_its_mission() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let source = r#"
version 2

  mission "shipped" state="ready" {
    goal "Ship one change."
    agent "worker" { workspace "."; command "true" }
    step "ship" { goal "Ship the change." }
  }

"#;
        let intent = parse_intent(source, "node").unwrap();
        let planned = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &planned.subject_tokens, "shipped-mission")
            .unwrap();
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "shipped".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "shipped-run".into(),
            })
            .unwrap();
        materialize_run_agents(&state, &run);
        state
            .store
            .set_mission_run_state(&run.id, "failed", "terminal", Some("a gate failed"))
            .unwrap();
        let outcome = |actor: String, key: &str| {
            serde_json::to_value(MissionRunOutcomeRequest {
                actor,
                status: "completed".into(),
                reason: "the change shipped after the gate was fixed".into(),
                idempotency_key: key.into(),
            })
            .unwrap()
        };
        let path = format!(
            "/v1/mission-runs/{}/outcome",
            urlencoding::encode(&run.subject)
        );

        let worker = format!("agent/{}/worker", run.id);
        let (status, completed) = json_request(
            router(state.clone()),
            &path,
            outcome(worker.clone(), "outcome-worker"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{completed}");
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["outcome"]["previous_status"], "failed");
        assert_eq!(completed["outcome"]["actor"], worker);

        let retire = |actor: String, key: &str| {
            serde_json::to_value(MissionRetireRequest {
                actor,
                idempotency_key: key.into(),
            })
            .unwrap()
        };
        let path = format!("/v1/missions/{}/retire", urlencoding::encode("shipped"));
        let (status, retired) = json_request(
            router(state.clone()),
            &path,
            retire(worker, "retire-worker"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{retired}");
        assert_eq!(retired["state"], "retired");
    }

    #[tokio::test]
    async fn a_run_revision_accepts_a_loop_round_embedded_mission() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let source = r#"
version 2
mission "loop-review" state="ready" revision-cutover="restart-active" {
  goal "Review the source."
  loop "review" {
    max-rounds 2
    round {
      completion { when "all-steps-exhausted" }
      step "write" { goal "Write the review." }
    }
  }
}
"#;
        let intent = parse_intent(source, "node").unwrap();
        let planned = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &planned.subject_tokens, "loop-review-initial")
            .unwrap();
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "loop-review".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "loop-review-run".into(),
            })
            .unwrap();
        let replacement = source.replace("Write the review.", "Write a corrected review.");
        let (status, revised) = json_request(
            router(state),
            &format!("/v1/mission-runs/{}/revision", run.id),
            serde_json::to_value(MissionRevisionRequest {
                intent: crate::model::IntentInput {
                    kdl: replacement,
                    source_name: None,
                },
                actor: "person/test".into(),
                reason: "Move the review to its corrected definition".into(),
                idempotency_key: "loop-review-revision".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{revised}");
        assert_eq!(revised["status"], "applied");
        assert_ne!(revised["mission_run"]["revision"], run.revision);
    }

    #[tokio::test]
    async fn seat_rename_follows_free_mode_and_preserves_other_fields() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let source = r#"version 2
agent "test/target" { workspace "."; command "true"; name "Initial seat" }
"#;
        let request = apply_request(&state, source, "person/test", "rename-fixture");
        let _ = apply(State(state.clone()), Json(request)).await.unwrap();
        let original = state.store.desired_subject_with_writer("agent/test/target").unwrap().unwrap();
        let initial = client_agent_resources(&state.store, false, "before", state.store.index().unwrap()).unwrap();
        assert_eq!(initial.iter().find(|agent| agent["id"] == "agent/test/target").unwrap()["name"], "Initial seat");
        let app = router(state.clone());
        let (status, _) = json_request(app.clone(), "/v1/agents/rename", json!({
            "subject": "test/target", "name": "Denied", "actor": "daemon/test",
            "idempotency_key": "invalid-actor",
        })).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(state.store.desired_subject_with_writer("agent/test/target").unwrap().unwrap(), original);
        for (name, key) in [(Some("Renamed seat"), "rename"), (None, "clear")] {
            let (status, body) = json_request(app.clone(), "/v1/agents/rename", json!({
                "subject": "test/target", "name": name, "actor": "agent/test/ungranted",
                "idempotency_key": key,
            })).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let (mut desired, writer) = state.store.desired_subject_with_writer("agent/test/target").unwrap().unwrap();
            let agents = client_agent_resources(&state.store, false, "after", state.store.index().unwrap()).unwrap();
            assert_eq!(agents.iter().find(|agent| agent["id"] == "agent/test/target").unwrap()["name"],
                name.unwrap_or("test/target"));
            assert_eq!(writer, original.1);
            desired.set_display_name(original.0.member.as_ref().unwrap().display_name.as_deref()).unwrap();
            assert_eq!(desired, original.0);
        }
    }

    /// Free mode (2026-10-01): within a fleet an agent may do what its person may do. A seat
    /// with no declaration and no grant publishes, starts, revises, retires and declares
    /// anywhere, stops another seat and itself, and every write records the agent as actor.
    #[tokio::test]
    async fn an_ungranted_agent_publishes_starts_revises_and_stops_anywhere_as_itself() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let app = router(state.clone());
        let agent = "agent/example/anyone";
        let apply = |source: String, key: &'static str| {
            let app = app.clone();
            let request = serde_json::to_value(apply_request(&state, &source, agent, key)).unwrap();
            async move {
                let (status, body) = json_request(app, "/v1/intent/apply", request).await;
                assert_eq!(status, StatusCode::OK, "{key}: {body}");
                body
            }
        };
        let writer = |subject: &str| {
            state
                .store
                .latest_claim(subject, None)
                .unwrap()
                .and_then(|claim| claim.actor)
        };

        let mission = r#"version 2
mission "elsewhere/jobs/one" state="ready" {
  concurrent-runs
  goal "Run in a namespace no grant names."
  step "work" { goal "First goal." }
}
"#;
        apply(mission.into(), "free-publish").await;
        assert_eq!(writer("mission/elsewhere/jobs/one").as_deref(), Some(agent));

        let revision = state
            .store
            .mission_spec("elsewhere/jobs/one", None)
            .unwrap()
            .unwrap()
            .revision;
        apply(
            format!(
                "version 2\nmission-run \"elsewhere/jobs/one/run\" {{\n  mission {:?}\n  workspace {:?}\n  requester {agent:?}\n}}\n",
                format!("mission/elsewhere/jobs/one@{revision}"),
                root.path().display().to_string(),
            ),
            "free-start",
        )
        .await;
        let run = state
            .store
            .mission_run("elsewhere/jobs/one/run")
            .unwrap()
            .unwrap();
        assert_eq!(run.requester, agent);

        let (status, revised) = json_request(
            app.clone(),
            &format!("/v1/mission-runs/{}/revision", urlencoding::encode(&run.id)),
            serde_json::to_value(MissionRevisionRequest {
                intent: crate::model::IntentInput {
                    kdl: mission.replace("First goal.", "Corrected goal."),
                    source_name: None,
                },
                actor: agent.into(),
                reason: "the first goal was incomplete".into(),
                idempotency_key: "free-revise".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{revised}");
        assert_eq!(revised["status"], "applied");

        // Declaring a seat with authority blocks succeeds; the preview says they are ignored.
        let seat = r#"version 2
agent "example/other" {
  workspace "."
  command "true"
  mission-authority { publish "example/*" }
  seat-authority { stop "example/*" }
}
"#;
        let (status, previewed) = json_request(
            app.clone(),
            "/v1/intent/mission",
            serde_json::to_value(crate::model::MissionRequest {
                intent: crate::model::IntentInput {
                    kdl: seat.into(),
                    source_name: None,
                },
                at_index: None,
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{previewed}");
        assert!(
            previewed["warnings"].as_array().unwrap().iter().any(|warning| {
                warning.as_str().unwrap()
                    == "free-mode: `agent/example/other` declares `mission-authority`, `seat-authority`, which st ignores; within a fleet every agent may do what its person may do"
            }),
            "{previewed}"
        );
        apply(seat.into(), "free-declare").await;
        assert_eq!(writer("agent/example/other").as_deref(), Some(agent));

        // It stops another seat, then itself.
        apply(
            "version 2\nstop \"agent/example/other\"\n".into(),
            "free-stop-other",
        )
        .await;
        apply(format!("version 2\nstop {agent:?}\n"), "free-stop-self").await;
        for subject in ["agent/example/other", agent] {
            let desired = state
                .store
                .desired_subjects()
                .unwrap()
                .into_iter()
                .find(|desired| desired.subject == subject)
                .unwrap();
            assert_eq!(desired.kind, "stop", "{subject}");
            assert_eq!(writer(subject).as_deref(), Some(agent), "{subject}");
        }

        // It cancels the run it started, then retires the mission once cleanup ends.
        apply(
            format!(
                "version 2\nmission-run {:?} {{ cancellation \"free-cancel\" {{ reason \"the work was superseded\" }} }}\n",
                run.subject
            ),
            "free-cancel",
        )
        .await;
        assert_eq!(
            state.store.mission_run(&run.id).unwrap().unwrap().phase,
            "cleanup-cancelled"
        );
        state
            .store
            .set_mission_run_state(&run.id, "cancelled", "terminal", Some("cleanup completed"))
            .unwrap();
        let (status, retired) = json_request(
            app,
            &format!(
                "/v1/missions/{}/retire",
                urlencoding::encode("elsewhere/jobs/one")
            ),
            serde_json::to_value(MissionRetireRequest {
                actor: agent.into(),
                idempotency_key: "free-retire".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{retired}");
        assert_eq!(retired["state"], "retired");
    }

    #[tokio::test]
    async fn a_claimed_step_publishes_one_attempt_bound_ready_mission() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let source = r#"
version 2

  mission "bootstrap" state="ready" {
    goal "Complete mission bootstrap."
    agent "planner" {
      workspace "."
      command "true"
      mission-authority { publish "project/*" }
    }
    step "compile" {
      assigned-to "agent/${ST_MISSION_RUN}/planner"
      produces-mission "project/work"
    }
  }

"#;
        let intent = parse_intent(source, "node").unwrap();
        let planned = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &planned.subject_tokens, "publish-bootstrap-output")
            .unwrap();
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "bootstrap".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "run-bootstrap-output".into(),
            })
            .unwrap();
        materialize_run_agents(&state, &run);
        let actor = format!("agent/{}/planner", run.id);
        let step = run.steps[0].clone();
        state
            .store
            .set_step_state(&step.subject, "ready", None)
            .unwrap();
        let produced = r#"
version 2

  mission "project/work" state="ready" {
    goal "Complete mission project/work."
    step "inspect" { title "Inspect the project" }
    step "implement" { depends-on { step "inspect" completed } }
  }

"#;
        let app = router(state.clone());
        let (status, output) = json_request(
            app.clone(),
            &format!("/v1/work/mission/{}", urlencoding::encode(&step.subject)),
            serde_json::to_value(MissionProductionRequest {
                intent: crate::model::IntentInput {
                    kdl: produced.into(),
                    source_name: Some("generated.kdl".into()),
                },
                actor: actor.clone(),
                incarnation: Some("test".into()),
                idempotency_key: "reject-unclaimed-output".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{output}");
        assert_eq!(output["code"], "work-not-claimed");
        state
            .store
            .work_action(
                &step.subject,
                "claim",
                &WorkRequest {
                    actor: Some(actor.clone()),
                    incarnation: Some("test".into()),
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: "claim-bootstrap-output".into(),
                },
            )
            .unwrap();
        let wrong = r#"
version 2

  mission "project/other" state="ready" { goal "Complete mission project/other."; goal "Complete mission project/other."; step "inspect" { } }

"#;
        let (status, output) = json_request(
            app.clone(),
            &format!("/v1/work/mission/{}", urlencoding::encode(&step.subject)),
            serde_json::to_value(MissionProductionRequest {
                intent: crate::model::IntentInput {
                    kdl: wrong.into(),
                    source_name: Some("wrong.kdl".into()),
                },
                actor: actor.clone(),
                incarnation: Some("test".into()),
                idempotency_key: "reject-wrong-output".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{output}");
        assert_eq!(output["code"], "wrong-mission-output");
        let (status, output) = json_request(
            app,
            &format!("/v1/work/mission/{}", urlencoding::encode(&step.subject)),
            serde_json::to_value(MissionProductionRequest {
                intent: crate::model::IntentInput {
                    kdl: produced.into(),
                    source_name: Some("generated.kdl".into()),
                },
                actor,
                incarnation: Some("test".into()),
                idempotency_key: "publish-produced-work".into(),
            })
            .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{output}");
        assert_eq!(output["mission"], "mission/project/work");
        let revision = output["revision"].as_str().unwrap();
        assert_eq!(revision.len(), 64);
        assert!(
            state
                .store
                .mission_spec("project/work", Some(revision))
                .unwrap()
                .is_some()
        );
        let bound = state
            .store
            .mission_output(&step.subject, 1, &step.definition_hash)
            .unwrap()
            .expect("attempt-bound mission output");
        assert_eq!(bound.revision, revision);
        assert_eq!(bound.claim_id, output["claim_id"]);
    }

    #[tokio::test]
    async fn claims_endpoint_returns_bounded_cursor_pages() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        for (subject, key) in [("host/one", "one"), ("host/two", "two")] {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: subject.into(),
                    kind: "transport.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(key.into()),
                })
                .unwrap();
        }
        let app = router(state);
        let (status, first) = get_request(app.clone(), "/v1/claims?limit=1").await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["claims"].as_array().unwrap().len(), 1);
        let cursor = first["next_cursor"].as_u64().unwrap();
        let (status, second) = get_request(
            app.clone(),
            &format!("/v1/claims?limit=1&after_index={cursor}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert_eq!(second["claims"].as_array().unwrap().len(), 1);
        assert!(second["next_cursor"].is_null());

        let (status, descending) =
            get_request(app, "/v1/claims?limit=1&order=desc&before_index=3").await;
        assert_eq!(status, StatusCode::OK, "{descending}");
        assert_eq!(
            descending["claims"][0]["subject"].as_str(),
            Some("host/two")
        );
    }

    #[tokio::test]
    async fn public_claim_admission_and_idempotency_use_the_exported_schema() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let reconcile_notify = state.notify.clone();
        let app = router(state);

        let (status, schema) = get_request(app.clone(), "/v1/schema").await;
        assert_eq!(status, StatusCode::OK, "{schema}");
        assert_eq!(schema["schema"], st3_schema::SCHEMA_NAME);
        assert_eq!(schema["digest"], st3_schema::registry().digest());

        let forbidden = serde_json::to_value(ClaimInput {
            subject: "host/node".into(),
            kind: "transport.observed".into(),
            actor: None,
            fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("caller-transport".into()),
        })
        .unwrap();
        let (status, body) = json_request(app.clone(), "/v1/claims", forbidden).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["code"], "claim-write-forbidden");

        let resource = ClaimInput {
            subject: "resource/example".into(),
            kind: "resource.observed".into(),
            actor: Some("person/alex".into()),
            fields: BTreeMap::from([
                (
                    "kind".into(),
                    Value::String("custom.example.measurement".into()),
                ),
                ("value".into(), Value::from(1)),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: Some("caller-resource".into()),
        };
        let (status, first) = json_request(
            app.clone(),
            "/v1/claims",
            serde_json::to_value(&resource).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            reconcile_notify.notified(),
        )
        .await
        .expect("a new claim must wake reconciliation");
        let (status, retry) = json_request(
            app.clone(),
            "/v1/claims",
            serde_json::to_value(&resource).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{retry}");
        assert_eq!(retry["id"], first["id"]);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(25),
                reconcile_notify.notified(),
            )
            .await
            .is_err(),
            "an idempotent claim replay must not wake reconciliation"
        );

        let mut changed = resource;
        changed.fields.insert("value".into(), Value::from(2));
        let (status, mismatch) =
            json_request(app, "/v1/claims", serde_json::to_value(changed).unwrap()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{mismatch}");
        assert_eq!(mismatch["code"], "idempotency-mismatch");
    }

    #[tokio::test]
    async fn harness_diagnostic_has_a_dedicated_idempotent_local_operation() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let app = router(state.clone());
        let request = json!({
            "actor": "agent/recovery-owner",
            "code": "driver-failed",
            "reason": "the harness transport exited",
            "severity": "error",
            "status": "active",
            "incarnation_id": "runtime:one",
            "idempotency_key": "harness-diagnostic-test"
        });
        let (status, first) =
            json_request(app.clone(), "/v1/diagnostics/harness", request.clone()).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["kind"], "harness.diagnostic");
        assert_eq!(first["subject"], "agent/recovery-owner");
        let (status, replay) = json_request(app, "/v1/diagnostics/harness", request).await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(replay["id"], first["id"]);
        assert_eq!(
            state
                .store
                .claims_for("agent/recovery-owner", Some("harness.diagnostic"))
                .unwrap()
                .len(),
            1
        );

        let (status, denied) =
            json_request(fabric_router(state), "/v1/diagnostics/harness", json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    }

    #[tokio::test]
    async fn an_invalid_gate_result_does_not_consume_its_capability() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        let subject = "gate-operation/test/result";
        let (capability, _) = store
            .issue_capability("gate-result", subject, None, 60_000)
            .unwrap();
        let app = router(state);
        let request = |idempotency_key: String| {
            serde_json::to_value(GateResultRequest {
                operation_capability: capability.clone(),
                verdict: "pass".into(),
                reason: "the evidence passes".into(),
                evidence: Vec::new(),
                idempotency_key,
            })
            .unwrap()
        };

        let (status, invalid) =
            json_request(app.clone(), "/v1/gate-results", request("x".repeat(513))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
        assert_eq!(invalid["code"], "invalid-idempotency-key");
        assert!(!store.capability(&capability, "gate-result").unwrap().used);

        let (status, accepted) =
            json_request(app, "/v1/gate-results", request("valid-gate-result".into())).await;
        assert_eq!(status, StatusCode::OK, "{accepted}");
        assert_eq!(accepted["body"]["fields"]["verdict"], "pass");
        assert!(store.capability(&capability, "gate-result").unwrap().used);
    }

    #[tokio::test]
    async fn human_review_list_and_decisions_use_exact_current_requests() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let source = r#"
version 2

  agent "worker" { workspace "/tmp"; command "true" }
  mission "review-api" state="ready" {
    goal "Complete mission review-api."
    step "approval" {
      goal "Submit the candidate."
      assigned-to "agent/worker"
      gate "human-review" type="human" { reviewer "person/alex" }
    }
  }

"#;
        let intent = parse_intent(source, "node").unwrap();
        let planned = state
            .store
            .mission(
                &intent,
                crate::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &planned.subject_tokens, "review-api-mission")
            .unwrap();
        let run = state
            .store
            .create_mission_run(&MissionRunRequest {
                mission: "review-api".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "review-api-run".into(),
            })
            .unwrap();
        let step = &run.steps[0];
        assert!(!step.agentless, "{step:?}");
        let claimant = step.assigned_to.clone().expect("the step has an assignee");
        state
            .store
            .set_step_state(&step.subject, "ready", None)
            .unwrap();
        state
            .store
            .work_action(
                &step.subject,
                "claim",
                &crate::model::WorkRequest {
                    actor: Some(claimant.clone()),
                    incarnation: Some("test-incarnation".into()),
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: "review-api-claim".into(),
                },
            )
            .unwrap();
        state
            .store
            .work_action(
                &step.subject,
                "complete",
                &crate::model::WorkRequest {
                    actor: Some(claimant.clone()),
                    incarnation: Some("test-incarnation".into()),
                    summary: Some("Candidate submitted for review".into()),
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: "review-api-submit".into(),
                },
            )
            .unwrap();
        let request_fields = |owner: String, definition: String, operation: &str| {
            BTreeMap::from([
                ("owner".into(), Value::String(owner)),
                ("reviewer".into(), Value::String("person/alex".into())),
                (
                    "question".into(),
                    Value::String("Is the candidate ready?".into()),
                ),
                (
                    "review_targets".into(),
                    Value::Array(vec![
                        Value::String("resource/review/candidate".into()),
                        Value::String("doc/review/report@abc".into()),
                    ]),
                ),
                (
                    "decisions".into(),
                    Value::Array(vec![
                        Value::String("approved".into()),
                        Value::String("rejected".into()),
                    ]),
                ),
                ("operation".into(), Value::String(operation.into())),
                (
                    "mission_revision".into(),
                    Value::String(run.revision.clone()),
                ),
                ("step_definition".into(), Value::String(definition)),
                ("attempt".into(), Value::from(step.attempt)),
            ])
        };
        let step_request = state
            .store
            .append_claim(&ClaimInput {
                subject: "gate-operation/review-api/approval".into(),
                kind: "gate.requested".into(),
                actor: None,
                fields: request_fields(
                    step.subject.clone(),
                    step.definition_hash.clone(),
                    "gate-operation/review-api/approval",
                ),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("step-review-request".into()),
            })
            .unwrap();
        let mission_request = state
            .store
            .append_claim(&ClaimInput {
                subject: "gate-operation/review-api/mission".into(),
                kind: "gate.requested".into(),
                actor: None,
                fields: request_fields(
                    run.subject.clone(),
                    run.revision.clone(),
                    "gate-operation/review-api/mission",
                ),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("mission-review-request".into()),
            })
            .unwrap();
        let store = state.store.clone();
        let app = router(state);
        let (status, listed) = get_request(app.clone(), "/v1/reviews").await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        assert_eq!(listed.as_array().unwrap().len(), 2);
        assert_eq!(listed[0]["request"], step_request.id);
        assert_eq!(listed[0]["owner"], step.subject);
        assert_eq!(listed[0]["step"], "approval");
        assert_eq!(listed[0]["review_targets"].as_array().unwrap().len(), 2);
        assert_eq!(listed[1]["request"], mission_request.id);
        assert_eq!(listed[1]["owner"], run.subject);
        assert!(listed[1].get("step").is_none());

        let (status, selected) =
            get_request(app.clone(), "/v1/reviews?reviewer=person%2Falex").await;
        assert_eq!(status, StatusCode::OK, "{selected}");
        assert_eq!(selected.as_array().unwrap().len(), 2);
        let (status, attention) =
            get_request(app.clone(), "/v1/attention?person=person%2Falex").await;
        assert_eq!(status, StatusCode::OK, "{attention}");
        assert_eq!(attention.as_array().unwrap().len(), 2);
        assert!(
            attention
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["kind"] == "human-gate")
        );
        assert!(
            attention.as_array().unwrap().iter().all(|item| {
                item["actions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|action| action["argv"][1] == "attention" && action["argv"][4] == "--as")
            }),
            "{attention}"
        );

        let (status, filtered) =
            get_request(app.clone(), "/v1/reviews?reviewer=person%2Fsomeone-else").await;
        assert_eq!(status, StatusCode::OK, "{filtered}");
        assert_eq!(filtered, json!([]));

        store
            .append_claim(&ClaimInput {
                subject: step_request.subject.clone(),
                kind: "gate.result".into(),
                actor: Some("person/someone-else".into()),
                fields: BTreeMap::from([
                    ("verdict".into(), Value::String("pass".into())),
                    ("request".into(), Value::String(step_request.id.clone())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("wrong-review-result".into()),
            })
            .unwrap();
        store
            .append_claim(&ClaimInput {
                subject: step_request.subject.clone(),
                kind: "gate.result".into(),
                actor: Some("person/alex".into()),
                fields: BTreeMap::from([("verdict".into(), Value::String("pass".into()))]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("unbound-review-result".into()),
            })
            .unwrap();
        let (_, still_pending) = get_request(app.clone(), "/v1/reviews").await;
        assert_eq!(still_pending.as_array().unwrap().len(), 2);

        // A newer build asks the mission gate again as a new operation. The reviewer sees one
        // review, aged from the first request, and the decision answers the request the gate
        // now waits on.
        let mut again_fields = request_fields(
            run.subject.clone(),
            run.revision.clone(),
            "gate-operation/review-api/mission-again",
        );
        again_fields.insert("mode".into(), Value::String("approve".into()));
        let mission_again = store
            .append_claim(&ClaimInput {
                subject: "gate-operation/review-api/mission-again".into(),
                kind: "gate.requested".into(),
                actor: None,
                fields: again_fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("mission-review-again".into()),
            })
            .unwrap();
        let (_, asked_again) = get_request(app.clone(), "/v1/reviews").await;
        assert_eq!(asked_again.as_array().unwrap().len(), 2, "{asked_again}");
        assert_eq!(asked_again[1]["request"], mission_again.id);
        assert_eq!(
            asked_again[1]["requested_at_unix_ms"],
            json!(mission_request.accepted_at_unix_ms)
        );

        let body = |actor: &str| {
            serde_json::to_value(ReviewRequest {
                decision: "approved".into(),
                reason: None,
                actor: Some(actor.into()),
                expected_subject: None,
            })
            .unwrap()
        };
        let (status, rejected) = json_request(
            app.clone(),
            &format!("/v1/reviews/{}", run.subject),
            body("person/someone-else"),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{rejected}");
        assert_eq!(rejected["code"], "wrong-reviewer");

        let (status, accepted_mission) = json_request(
            app.clone(),
            &format!("/v1/reviews/{}", run.subject),
            body("person/alex"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{accepted_mission}");
        assert_eq!(
            accepted_mission["body"]["fields"]["request"],
            mission_again.id
        );
        assert_eq!(accepted_mission["subject"], mission_again.subject);
        assert_eq!(accepted_mission["body"]["fields"]["verdict"], "pass");

        let missing_reason = serde_json::to_value(ReviewRequest {
            decision: "rejected".into(),
            reason: None,
            actor: Some("person/alex".into()),
            expected_subject: None,
        })
        .unwrap();
        let (status, invalid) = json_request(
            app.clone(),
            &format!("/v1/reviews/{}", step.subject),
            missing_reason,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
        assert_eq!(invalid["code"], "missing-review-reason");

        let reject = serde_json::to_value(ReviewRequest {
            decision: "rejected".into(),
            reason: Some("the evidence is incomplete".into()),
            actor: Some("person/alex".into()),
            expected_subject: None,
        })
        .unwrap();
        let (status, accepted_step) = json_request(
            app.clone(),
            &format!("/v1/reviews/{}", step.subject),
            reject,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{accepted_step}");
        assert_eq!(accepted_step["body"]["fields"]["request"], step_request.id);
        assert_eq!(accepted_step["body"]["fields"]["verdict"], "fail");
        assert_eq!(accepted_step["body"]["evidence"][0], step_request.id);
        let messages = store.messages(Some(&claimant), false).unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains("the evidence is incomplete"));

        store
            .set_mission_run_state(&run.id, "failed", "terminal", None)
            .unwrap();
        let feedback_run = store
            .create_mission_run(&MissionRunRequest {
                mission: "review-api".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/test".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "review-api-feedback-run".into(),
            })
            .unwrap();
        let feedback_step = &feedback_run.steps[0];
        let feedback_claimant = feedback_step.assigned_to.clone().unwrap();
        store
            .set_step_state(&feedback_step.subject, "ready", None)
            .unwrap();
        for action in ["claim", "complete"] {
            store
                .work_action(
                    &feedback_step.subject,
                    action,
                    &crate::model::WorkRequest {
                        actor: Some(feedback_claimant.clone()),
                        incarnation: Some("test-incarnation".into()),
                        summary: Some("Candidate submitted".into()),
                        reason: None,
                        evidence: Vec::new(),
                        idempotency_key: format!("feedback-api-{action}"),
                    },
                )
                .unwrap();
        }
        let mut feedback_fields = request_fields(
            feedback_step.subject.clone(),
            feedback_step.definition_hash.clone(),
            "gate-operation/review-api/feedback",
        );
        feedback_fields.insert("mode".into(), Value::String("feedback".into()));
        feedback_fields.insert("decisions".into(), json!(["approved", "changes-requested"]));
        let feedback_request = store
            .append_claim(&ClaimInput {
                subject: "gate-operation/review-api/feedback".into(),
                kind: "gate.requested".into(),
                actor: None,
                fields: feedback_fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("feedback-api-request".into()),
            })
            .unwrap();
        let (_, attention) = get_request(app.clone(), "/v1/attention?person=person%2Falex").await;
        assert_eq!(attention[0]["review_mode"], "feedback");
        assert!(
            attention[0]["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|action| action["label"] == "request changes")
        );
        let feedback_path = format!("/v1/reviews/{}", feedback_step.subject);
        let (status, invalid) = json_request(
            app.clone(),
            &feedback_path,
            json!({
                "decision": "rejected", "reason": "More detail", "actor": "person/alex"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
        assert_eq!(invalid["code"], "review-decision-not-offered");
        let (status, accepted) = json_request(
            app.clone(),
            &feedback_path,
            json!({
                "decision": "changes-requested", "reason": "Add a source.", "actor": "person/alex"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{accepted}");
        assert_eq!(accepted["body"]["fields"]["verdict"], "feedback");
        assert_eq!(accepted["body"]["fields"]["request"], feedback_request.id);

        let (_, empty) = get_request(app, "/v1/reviews?reviewer=person%2Falex").await;
        assert_eq!(empty, json!([]));
    }

    #[tokio::test]
    async fn legacy_attention_mutations_report_migration_without_writes() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        let app = router(state);
        for (path, body) in [
            (
                "/v1/attention",
                json!({"reviewer":"person/avery","title":"Decide","reason":"Choose a date","severity":"warning","targets":[],"actor":"agent/alder.asker","idempotency_key":"legacy-ask"}),
            ),
            (
                "/v1/attention/resolve/attention%2Flegacy",
                json!({"outcome":"resolved","actor":"person/avery","idempotency_key":"legacy-done"}),
            ),
            (
                "/v1/attention/withdraw/attention%2Flegacy",
                json!({"reason":"Ended","actor":"agent/alder.asker","idempotency_key":"legacy-cancel"}),
            ),
        ] {
            let (status, error) = json_request(app.clone(), path, body).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
            assert_eq!(error["code"], "attention-migrated");
        }
        assert!(store.attention_requests(None, true).unwrap().is_empty());
        assert!(store.messages(None, true).unwrap().is_empty());
    }

    #[tokio::test]
    async fn client_now_never_lists_a_fault() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let store = state.store.clone();
        store
            .record_operational_failure(
                "daemon/high",
                &AttentionRequest {
                    reviewer: "person/alex".into(),
                    title: "Fault error".into(),
                    reason: "The subscription needs a correction.".into(),
                    severity: "error".into(),
                    targets: vec!["daemon/high".into()],
                    actor: "daemon/runtime".into(),
                    idempotency_key: "daemon/high".into(),
                },
            )
            .unwrap();
        assert_eq!(store.fault_snapshot(client_now_ms()).unwrap().len(), 1);
        let app = router(state);
        for path in ["/v1/client/now?person=person%2Falex", "/v1/client/now"] {
            let (status, page) = get_request(app.clone(), path).await;
            assert_eq!(status, StatusCode::OK, "{page}");
            assert_eq!(page["items"], json!([]), "{path}");
        }
    }

    #[test]
    fn legacy_attention_from_live_and_retired_requesters_is_not_current() {
        let store = Store::open_memory("node").unwrap();
        let apply = |intent: &crate::model::NormalizedIntent, key: &str| {
            let planned = store
                .mission(
                    intent,
                    crate::model::IntentInput {
                        kdl: key.into(),
                        source_name: None,
                    },
                )
                .unwrap();
            store.apply(intent, &planned.subject_tokens, key).unwrap();
        };
        let parse = |source: &str| crate::graph::parse_intent(source, "node").unwrap();
        apply(
            &parse(
                r#"version 2
agent "stopped" { workspace "/tmp"; command "true" }
agent "live" { workspace "/tmp"; command "true" }
mission "standing" state="ready" {
  goal "Keep a seat."
  step "hold" { agentless }
}
"#,
            ),
            "requesters",
        );
        let run = store
            .create_mission_run(&crate::model::MissionRunRequest {
                mission: "standing".into(),
                revision: None,
                workspace: "/tmp".into(),
                requester: Some("person/alex".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "standing-run".into(),
            })
            .unwrap();
        // The run owns this seat, as a standing mission owns its agents.
        let mut owned = parse(
            r#"version 2
agent "seat" { workspace "/tmp"; command "true" }
"#,
        );
        let desired = owned.subjects.get_mut("agent/node.seat").unwrap();
        desired.owner_run = Some(run.subject.clone());
        desired.owner_generation = Some(run.generation.clone());
        apply(&owned, "owned-seat");
        let seat = "agent/node.seat".to_owned();
        for (subject, actor) in [
            ("attention/from-stopped", "agent/node.stopped"),
            ("attention/from-live", "agent/node.live"),
            ("attention/from-seat", seat.as_str()),
        ] {
            store
                .request_attention(
                    subject,
                    &AttentionRequest {
                        reviewer: "person/alex".into(),
                        title: format!("Fault from {actor}"),
                        reason: "a person must decide".into(),
                        severity: "warning".into(),
                        targets: Vec::new(),
                        actor: actor.into(),
                        idempotency_key: format!("{subject}:requested"),
                    },
                )
                .unwrap();
        }
        let reasons = |store: &Store| {
            client_attention_resources(store, Some("person/alex"), false)
                .unwrap()
                .into_iter()
                .map(|resource| {
                    (
                        resource["source_id"].as_str().unwrap().to_owned(),
                        resource["operational"]["reasons"].clone(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        assert!(
            reasons(&store)
                .values()
                .all(|reasons| reasons == &json!([]))
        );

        apply(
            &parse("version 2\nstop \"agent/node.stopped\"\n"),
            "stop-requester",
        );
        store
            .set_mission_run_state(&run.id, "cancelled", "terminal", Some("moved to a seat"))
            .unwrap();

        assert!(reasons(&store).is_empty(), "legacy rows are audit data");
    }

    #[test]
    fn source_failure_priorities_order_faults_and_never_reach_attention() {
        let store = Store::open_memory("alder").unwrap();
        for severity in ["warning", "error"] {
            store
                .record_operational_failure(
                    severity,
                    &AttentionRequest {
                        reviewer: "person/avery".into(),
                        title: format!("{severity} fault"),
                        reason: "The disk needs attention.".into(),
                        severity: severity.into(),
                        targets: vec![format!("daemon/{severity}")],
                        actor: "daemon/runtime".into(),
                        idempotency_key: severity.into(),
                    },
                )
                .unwrap();
        }
        let faults = store.fault_snapshot(client_now_ms()).unwrap();
        assert_eq!(faults[0].item.priority, "high");
        assert_eq!(faults[1].item.priority, "normal");
        for history in [false, true] {
            assert!(
                client_attention_resources(&store, Some("person/avery"), history)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn person_work_routes_share_snapshot_and_reject_stale_or_wrong_responses() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let intent = crate::graph::parse_internal_intent(
            "version 2\nagent \"asker\" { workspace \"/tmp\"; command \"true\" }",
            state.store.origin(),
        )
        .unwrap();
        state
            .store
            .apply_internal(&intent, "api-person-asker")
            .unwrap();
        let actor = format!("agent/{}.asker", state.store.origin());
        let app = router(state.clone());
        let body = json!({"person":"person/avery","title":"Choose a date","reason":"Reply with a date","actor":actor,"new_run":"release-date","idempotency_key":"api-ask"});
        let (status, ask) = json_request(app.clone(), "/v1/work/ask", body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{ask}");
        let (_, duplicate) = json_request(app.clone(), "/v1/work/ask", body).await;
        assert_eq!(ask["subject"], duplicate["subject"]);
        let (_, attention) = get_request(app.clone(), "/v1/attention?person=person%2Favery").await;
        assert_eq!(attention[0]["subject"], ask["subject"]);
        let (_, now) = get_request(app.clone(), "/v1/client/now?person=person%2Favery").await;
        assert_eq!(now["items"][0]["source_id"], ask["subject"]);
        assert_eq!(now["items"][0]["actions"], json!(["work.done"]));
        let episode = attention[0]["episode"].clone();
        let mut done = json!({"subject":ask["subject"],"actor":"person/robin","summary":"Friday","episode":episode,"evidence":[],"idempotency_key":"api-done"});
        let (_, refused) = json_request(app.clone(), "/v1/work/done", done.clone()).await;
        assert_eq!(refused["code"], "forbidden");
        done["actor"] = json!("person/avery");
        done["episode"] = json!("old");
        let (_, stale) = json_request(app.clone(), "/v1/work/done", done.clone()).await;
        assert_eq!(stale["code"], "stale-fence");
        done["episode"] = episode;
        let (status, completed) = json_request(app.clone(), "/v1/work/done", done.clone()).await;
        assert_eq!(status, StatusCode::OK, "{completed}");
        assert_eq!(completed["status"], "completed");
        let (_, again) = json_request(app.clone(), "/v1/work/done", done).await;
        assert_eq!(again["status"], "completed");
        let (_, empty) = get_request(app, "/v1/client/now?person=person%2Favery").await;
        assert_eq!(empty["items"], json!([]));
        // No step waits on a new-run ask; its requester hears the answer once, by message.
        let told = state.store.messages(Some(&actor), true).unwrap();
        assert_eq!(told.len(), 1);
        assert!(told[0].content.contains("Friday"));
    }

    #[tokio::test]
    async fn message_pages_are_bounded_and_stable_across_new_writes() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        for index in 0..205 {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: format!("message/page-{index:03}"),
                    kind: "message.sent".into(),
                    actor: Some("agent/sender".into()),
                    fields: BTreeMap::from([
                        ("from".into(), Value::String("agent/sender".into())),
                        ("to".into(), Value::String("agent/receiver".into())),
                        ("content".into(), Value::String(format!("body {index}"))),
                        ("status".into(), Value::String("sent".into())),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
        }
        let app = router(state.clone());
        let mut cursor: Option<String> = None;
        let mut seen = Vec::new();
        loop {
            let mut path = "/v1/messages/page?to=agent%2Freceiver&limit=100".to_owned();
            if let Some(cursor) = &cursor {
                path.push_str(&format!("&cursor={}", urlencoding::encode(cursor)));
            }
            let (status, page) = get_request(app.clone(), &path).await;
            assert_eq!(status, StatusCode::OK, "{page}");
            let items = page["items"].as_array().unwrap();
            assert!(items.len() <= 100);
            seen.extend(
                items
                    .iter()
                    .map(|item| item["subject"].as_str().unwrap().to_owned()),
            );
            cursor = page["next_cursor"].as_str().map(str::to_owned);
            if seen.len() == 100 {
                state
                    .store
                    .append_claim(&ClaimInput {
                        subject: "message/new-after-page".into(),
                        kind: "message.sent".into(),
                        actor: Some("agent/sender".into()),
                        fields: BTreeMap::from([
                            ("from".into(), Value::String("agent/sender".into())),
                            ("to".into(), Value::String("agent/receiver".into())),
                            ("content".into(), Value::String("late".into())),
                            ("status".into(), Value::String("sent".into())),
                        ]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: None,
                    })
                    .unwrap();
            }
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(seen.len(), 205);
        assert_eq!(seen.first().unwrap(), "message/page-000");
        assert_eq!(seen.last().unwrap(), "message/page-204");
        let (status, exact) = get_request(app, "/v1/messages/read/message%2Fpage-204").await;
        assert_eq!(status, StatusCode::OK, "{exact}");
        assert_eq!(exact["content"], "body 204");
    }
    #[tokio::test]
    async fn harness_event_endpoint_requires_this_native_seats_local_peer_and_runtime() {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let subject = "agent/example/event-api";
        state
            .store
            .append_claim(&ClaimInput {
                subject: subject.into(),
                kind: "runtime.observed".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("running")),
                    ("incarnation_id".into(), json!("runtime-a")),
                ]),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let request = json!({"runtime_incarnation":"runtime-a", "sequence":1,
            "claim":{"subject":subject,"kind":"harness.observed","actor":subject,
                "fields":{"state":"idle","driver":"claude","incarnation_id":"runtime-a"},
                "evidence":[],"idempotency_key":"fixture-event"}});
        let app = router(state);
        let (status, _) = json_request(app.clone(), "/v1/harness-events", request.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let peer = NativeDeliveryPeer {
            agent: "agent/example/foreign".into(),
            transport: "claude-channel",
            pid: 7,
            archives_inbox: true,
        };
        let (status, _) = json_request(
            app.clone().layer(Extension(peer)),
            "/v1/harness-events",
            request.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let peer = NativeDeliveryPeer {
            agent: subject.into(),
            transport: "claude-channel",
            pid: 7,
            archives_inbox: true,
        };
        let app = app.layer(Extension(peer));
        let (status, first) =
            json_request(app.clone(), "/v1/harness-events", request.clone()).await;
        assert_eq!(status, StatusCode::OK, "{first}");
        let (status, replay) =
            json_request(app.clone(), "/v1/harness-events", request.clone()).await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(first["body"]["fields"], replay["body"]["fields"]);
        let mut usage = request.clone();
        usage["sequence"] = json!(2);
        usage["claim"]["kind"] = json!("harness.timeline");
        usage["claim"]["fields"] = json!({
            "operation":"append", "entry_id":"response-a", "source_id":"source/response-a",
            "sequence":1, "revision":1, "role":"system", "entry_type":"usage", "final":true,
            "driver":"claude", "incarnation_id":"runtime-a", "observed_at_unix_ms":1,
            "body":{"semantics":"response", "driver":"claude", "model":"fixture-model",
                "input_tokens":10, "output_tokens":5, "cached_tokens":2, "total_tokens":17}
        });
        usage["claim"]["fields"]["observed_at_unix_ms"] = json!(client_now_ms() as u64);
        for _ in 0..2 {
            let (status, body) =
                json_request(app.clone(), "/v1/harness-events", usage.clone()).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let (status, rollup) = get_request(
            app.clone(),
            &format!("/v1/usage?since_ms=0&until_ms={}", client_now_ms() + 60_000),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{rollup}");
        assert_eq!(
            rollup["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["total_tokens"].as_u64().unwrap_or(0))
                .sum::<u64>(),
            17,
            "{rollup}"
        );
        let mut stale = request;
        stale["runtime_incarnation"] = json!("retired-runtime");
        let (status, body) = json_request(app, "/v1/harness-events", stale).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
}
