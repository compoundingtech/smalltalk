//! What the sync worker asks of the store it keeps in step: export and receive exchanges, answer
//! heals, serve and adopt checkpoints, publish endpoints, redeem invites, and read membership.

use std::borrow::Borrow;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use crate::claim::ReplicaEnvelopeId;
use crate::fleet::FleetView;
use crate::fleet::handshake::JoinRequest;
use crate::replication::{
    ReplicationExchange, ReplicationExportResponse, ReplicationHealAnswer, ReplicationHealQuery,
    ReplicationHealStep, ReplicationInventory, ReplicationReceiveResponse,
};
use crate::store::{
    CheckpointAction, CheckpointManifest, CheckpointManifestNeed, CheckpointManifestPage,
    CheckpointManifestRequest, FleetRedemption, Store,
};

/// The store the worker syncs, as the worker reaches it. Every call may cross a process
/// boundary, so each takes and returns plain values.
pub trait Backend: Clone + Send + Sync + 'static {
    /// Wait until the store answers. The default answers at once.
    fn ready(&self) -> impl Future<Output = ()> + Send {
        async {}
    }

    /// This node's exchange for a peer that holds `inventory`, or only its summary.
    fn export(
        &self,
        fleet_id: &str,
        inventory: &ReplicationInventory,
        summary_only: bool,
        signature_requests: &[ReplicaEnvelopeId],
    ) -> impl Future<Output = Result<ReplicationExportResponse>> + Send;

    /// Export for a peer using projection digests. Implementations that have not opted in
    /// still send the exact legacy digest, preserving the old exchange contract.
    fn export_modern(
        &self,
        fleet_id: &str,
        inventory: &ReplicationInventory,
        summary_only: bool,
        signature_requests: &[ReplicaEnvelopeId],
    ) -> impl Future<Output = Result<ReplicationExportResponse>> + Send {
        self.export(fleet_id, inventory, summary_only, signature_requests)
    }

    /// Store a peer's exchange, then admit and project what it carried. `round_trip` is how
    /// long this worker's request that returned it took, when the exchange is a response.
    fn receive(
        &self,
        peer: &str,
        fleet_id: &str,
        exchange: &ReplicationExchange,
        round_trip: Option<Duration>,
    ) -> impl Future<Output = Result<ReplicationReceiveResponse>> + Send;

    /// Answer a peer's heal question from this node's claims.
    fn heal_answer(
        &self,
        peer: &str,
        fleet_id: &str,
        query: &ReplicationHealQuery,
    ) -> impl Future<Output = Result<ReplicationHealAnswer>> + Send;

    /// Compare a peer's heal answer with this node's claims and learn what to ask next.
    fn heal_next(
        &self,
        peer: &str,
        answer: ReplicationHealAnswer,
    ) -> impl Future<Output = Result<ReplicationHealStep>> + Send;

    fn checkpoint_manifest(
        &self,
        request: &CheckpointManifestRequest,
    ) -> impl Future<Output = Result<CheckpointManifestPage>> + Send;

    fn checkpoint_need(
        &self,
    ) -> impl Future<Output = Result<Option<CheckpointManifestNeed>>> + Send;

    fn adopt_checkpoint(
        &self,
        manifest: &CheckpointManifest,
    ) -> impl Future<Output = Result<Vec<CheckpointAction>>> + Send;

    fn publish_endpoints(
        &self,
        mode: &str,
        endpoints: &[Value],
    ) -> impl Future<Output = Result<()>> + Send;

    /// Redeem a join request against this node's open invites, answering as
    /// [`redemption_answer`] does.
    fn redeem(&self, request: &JoinRequest) -> impl Future<Output = Result<Value>> + Send;

    fn fleet_view(&self) -> impl Future<Output = Result<FleetView>> + Send;

    fn record_failure(
        &self,
        peer: &str,
        status: &str,
        error: &str,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Publish host-local worker progress without adding replicated claims.
    fn record_worker(
        &self,
        _peer: &str,
        _worker: crate::replication::ReplicationWorkerStatus,
    ) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }

    /// The store took in something new from a peer. A backend whose store has readers
    /// elsewhere tells them here; the default does nothing.
    fn changed(&self) -> impl Future<Output = ()> + Send {
        async {}
    }
}

/// A store this process holds itself: a [`Store`], or a runtime's store built on one.
pub struct Local<S>(pub Arc<S>);

impl<S> Clone for Local<S> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<S: Borrow<Store>> Local<S> {
    fn store(&self) -> &Store {
        (*self.0).borrow()
    }
}

/// A redemption as the join route reads it.
pub fn redemption_answer(store: &Store, redemption: FleetRedemption) -> Result<Value> {
    Ok(match redemption {
        FleetRedemption::Closed => serde_json::json!({"status": "closed"}),
        FleetRedemption::Refused(reason) => {
            serde_json::json!({"status": "refused", "reason": reason})
        }
        FleetRedemption::Admitted {
            token,
            writer_floor,
            admitted_claim,
            ..
        } => serde_json::json!({
            "status": "admitted",
            "token": hex::encode(token),
            "writer_floor": writer_floor,
            "admitted_claim": admitted_claim,
            "anchor_key": store.fleet_anchor()?,
            "fleet_id": store.bound_fleet()?,
            "fabric_protocol": serde_json::Value::Null,
        }),
    })
}

impl<S> Backend for Local<S>
where
    S: Borrow<Store> + Send + Sync + 'static,
{
    async fn export_modern(
        &self,
        fleet_id: &str,
        inventory: &ReplicationInventory,
        summary_only: bool,
        signature_requests: &[ReplicaEnvelopeId],
    ) -> Result<ReplicationExportResponse> {
        let backend = self.clone();
        let fleet_id = fleet_id.to_owned();
        let inventory = inventory.clone();
        let signature_requests = signature_requests.to_vec();
        tokio::task::spawn_blocking(move || {
            let store = backend.store();
            let exchange = if summary_only {
                store.export_replication_summary_modern(&fleet_id)?
            } else {
                store.export_replication_exchange_answering_modern(
                    &fleet_id,
                    &inventory,
                    &signature_requests,
                )?
            };
            Ok(ReplicationExportResponse {
                exchange,
                store_index: store.index()?,
            })
        })
        .await?
    }

    async fn export(
        &self,
        fleet_id: &str,
        inventory: &ReplicationInventory,
        summary_only: bool,
        signature_requests: &[ReplicaEnvelopeId],
    ) -> Result<ReplicationExportResponse> {
        let backend = self.clone();
        let fleet_id = fleet_id.to_owned();
        let inventory = inventory.clone();
        let signature_requests = signature_requests.to_vec();
        tokio::task::spawn_blocking(move || {
            let store = backend.store();
            let exchange = if summary_only {
                store.export_replication_summary(&fleet_id)?
            } else {
                store.export_replication_exchange_answering(
                    &fleet_id,
                    &inventory,
                    &signature_requests,
                )?
            };
            Ok(ReplicationExportResponse {
                exchange,
                store_index: store.index()?,
            })
        })
        .await?
    }

    async fn receive(
        &self,
        peer: &str,
        fleet_id: &str,
        exchange: &ReplicationExchange,
        round_trip: Option<Duration>,
    ) -> Result<ReplicationReceiveResponse> {
        let backend = self.clone();
        let peer = peer.to_owned();
        let fleet_id = fleet_id.to_owned();
        let exchange = exchange.clone();
        tokio::task::spawn_blocking(move || {
            let store = backend.store();
            if let Some(round_trip) = round_trip {
                store.record_replication_round_trip(round_trip);
            }
            let receipt = store
                .receive_replication_exchange_asking(
                    &peer,
                    &fleet_id,
                    &exchange,
                    round_trip.is_some(),
                )
                .map_err(anyhow::Error::msg)?;
            store.record_transport_observation(&peer, "up", None, None)?;
            let admission = store.validate_replication_backlog()?;
            let repairs = store.apply_replication_repairs()?;
            let projected = store.project_replication_backlog()?;
            Ok(ReplicationReceiveResponse {
                receipt,
                changed: projected && (admission.changed || repairs != 0),
                store_index: store.index()?,
            })
        })
        .await?
    }

    async fn heal_answer(
        &self,
        peer: &str,
        _fleet_id: &str,
        query: &ReplicationHealQuery,
    ) -> Result<ReplicationHealAnswer> {
        self.store().heal_answer(peer, query)
    }

    async fn heal_next(
        &self,
        peer: &str,
        answer: ReplicationHealAnswer,
    ) -> Result<ReplicationHealStep> {
        self.store().heal_next(peer, answer)
    }

    async fn checkpoint_manifest(
        &self,
        request: &CheckpointManifestRequest,
    ) -> Result<CheckpointManifestPage> {
        self.store().checkpoint_manifest_page(request)
    }

    async fn checkpoint_need(&self) -> Result<Option<CheckpointManifestNeed>> {
        self.store().checkpoint_manifest_need()
    }

    async fn adopt_checkpoint(
        &self,
        manifest: &CheckpointManifest,
    ) -> Result<Vec<CheckpointAction>> {
        self.store()
            .adopt_checkpoint(manifest)
            .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))
    }

    async fn publish_endpoints(&self, mode: &str, endpoints: &[Value]) -> Result<()> {
        self.store()
            .publish_fleet_endpoints(mode, endpoints, env!("CARGO_PKG_VERSION"))
            .map(|_| ())
    }

    async fn redeem(&self, request: &JoinRequest) -> Result<Value> {
        redemption_answer(self.store(), self.store().redeem_fleet_invite(request)?)
    }

    async fn fleet_view(&self) -> Result<FleetView> {
        self.store().fleet_view()
    }

    async fn record_worker(
        &self,
        peer: &str,
        worker: crate::replication::ReplicationWorkerStatus,
    ) -> Result<()> {
        self.store().record_replication_worker(peer, worker);
        Ok(())
    }

    async fn record_failure(&self, peer: &str, status: &str, error: &str) -> Result<()> {
        if self.store().record_peer_failure(peer, status, error)? {
            self.store()
                .record_transport_observation(peer, status, Some(error), None)?;
        }
        Ok(())
    }
}
