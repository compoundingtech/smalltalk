//! What the sync worker asks of the store it keeps in step: export and receive exchanges, serve
//! and adopt checkpoints, publish endpoints, redeem invites, and read membership.

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
    ReplicationExchange, ReplicationExportResponse, ReplicationInventory,
    ReplicationReceiveResponse,
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

    /// The recent-inbound guard still requests the complete, unchanged summary.
    /// A process boundary may mark this local request for count/wall accounting.
    fn guard_summary(
        &self,
        fleet_id: &str,
    ) -> impl Future<Output = Result<ReplicationExportResponse>> + Send {
        async move {
            self.export(fleet_id, &ReplicationInventory::default(), true, &[]).await
        }
    }

    /// Store a peer's exchange, then admit and project what it carried. `round_trip` is how
    /// long this worker's request that returned it took, when the exchange is a response.
    fn receive(
        &self,
        peer: &str,
        fleet_id: &str,
        exchange: &ReplicationExchange,
        round_trip: Option<Duration>,
        carrier: crate::replication::ReplicationCarrier,
    ) -> impl Future<Output = Result<ReplicationReceiveResponse>> + Send;

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
        carrier: Option<crate::replication::ReplicationCarrier>,
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
        tokio::task::spawn_blocking(move || crate::task_cpu::blocking("replication.export", || {
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
        }))
        .await?
    }

    async fn receive(
        &self,
        peer: &str,
        fleet_id: &str,
        exchange: &ReplicationExchange,
        round_trip: Option<Duration>,
        carrier: crate::replication::ReplicationCarrier,
    ) -> Result<ReplicationReceiveResponse> {
        let backend = self.clone();
        let peer = peer.to_owned();
        let fleet_id = fleet_id.to_owned();
        let exchange = exchange.clone();
        self.store().connection.spawn_blocking(move || crate::task_cpu::blocking("replication.receive", || {
            let store = backend.store();
            if let Some(round_trip) = round_trip {
                store.record_replication_round_trip(round_trip);
            }
            let receipt = store
                .receive_replication_exchange_via(
                    &peer,
                    &fleet_id,
                    &exchange,
                    round_trip.is_some(),
                    carrier,
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
        }))
        .await?
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

    async fn record_failure(
        &self,
        peer: &str,
        status: &str,
        error: &str,
        carrier: Option<crate::replication::ReplicationCarrier>,
    ) -> Result<()> {
        if self
            .store()
            .record_peer_failure_via(peer, status, error, carrier)?
        {
            self.store()
                .record_transport_observation(peer, status, Some(error), None)?;
        }
        Ok(())
    }
}
