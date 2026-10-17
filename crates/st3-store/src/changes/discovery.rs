//! One request-driven Git discovery owner per configured repository. Cancellation
//! of a client does not cancel the owner task or permit a second concurrent read.
use super::transport::{Advertisement, Repository};
use anyhow::{Context, Result};
use serde::{Serialize, Serializer};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, watch};

const INTERVAL: Duration = Duration::from_secs(10);
const WINDOW: Duration = Duration::from_secs(60);
const READS_PER_WINDOW: usize = 6;

pub trait Transport: Send + Sync + 'static {
    fn discover(&self) -> Result<Advertisement>;
    fn sync(&self) -> Result<()>;
}

pub struct GitTransport {
    pub repository: Repository,
    pub remote: String,
}

impl Transport for GitTransport {
    fn discover(&self) -> Result<Advertisement> {
        self.repository.discover(&self.remote)
    }

    fn sync(&self) -> Result<()> {
        self.repository.sync(&self.remote)
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Observation {
    /// Last successful inventory, retained after transport failure.
    #[serde(serialize_with = "serialize_advertisement")]
    advertisement: Option<Arc<Advertisement>>,
    #[serde(skip)]
    ids: Arc<BTreeMap<String, Option<String>>>,
    pub attempted_at: Option<String>,
    pub operation_ms: Option<u64>,
    pub error: Option<String>,
    pub offline: bool,
    pub reused: bool,
}

impl Observation {
    /// Validate and reduce an inventory once, before sharing it with requests.
    pub fn from_advertisement(advertisement: Advertisement) -> Result<Self> {
        let ids = super::view::advertised_ids(&advertisement)?;
        Ok(Self {
            advertisement: Some(Arc::new(advertisement)),
            ids: Arc::new(ids),
            ..Self::default()
        })
    }

    pub fn advertisement(&self) -> Option<&Advertisement> {
        self.advertisement.as_deref()
    }

    pub fn advertised_ids(&self) -> &BTreeMap<String, Option<String>> {
        &self.ids
    }
}

fn serialize_advertisement<S: Serializer>(
    advertisement: &Option<Arc<Advertisement>>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    advertisement.as_deref().serialize(serializer)
}

#[derive(Default)]
struct Budget {
    starts: VecDeque<Instant>,
}

impl Budget {
    fn available_at(&mut self, now: Instant) -> Instant {
        while self
            .starts
            .front()
            .is_some_and(|start| now.duration_since(*start) >= WINDOW)
        {
            self.starts.pop_front();
        }
        let interval = self.starts.back().map_or(now, |start| *start + INTERVAL);
        let window = if self.starts.len() >= READS_PER_WINDOW {
            self.starts[0] + WINDOW
        } else {
            now
        };
        interval.max(window).max(now)
    }

    fn admit(&mut self, now: Instant) -> bool {
        if self.available_at(now) != now {
            return false;
        }
        self.starts.push_back(now);
        true
    }
}

struct State {
    budget: Budget,
    observation: Observation,
    discovery: Option<watch::Receiver<Option<Observation>>>,
    syncing: bool,
    counters: Counters,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Counters {
    pub discoveries_started: u64,
    pub fetches_started: u64,
    pub joined_requests: u64,
    pub reused_observations: u64,
    pub offline_requests: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Diagnostics {
    pub counters: Counters,
    pub remote_read_operations_in_last_minute: usize,
    pub next_read_after_ms: u64,
    pub in_flight: bool,
}

/// The daemon registry constructs exactly one Owner per repository and is the only
/// place authorized to schedule transport reads. Fetch and discovery share a budget.
#[derive(Clone)]
pub struct Owner {
    transport: Arc<dyn Transport>,
    state: Arc<Mutex<State>>,
}

impl Owner {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            state: Arc::new(Mutex::new(State {
                budget: Budget::default(),
                observation: Observation::default(),
                discovery: None,
                syncing: false,
                counters: Counters::default(),
            })),
        }
    }

    /// Offline performs no transport call. Online joins an in-flight discovery or
    /// returns the last observation with its original timestamp during the budget gap.
    pub async fn observe(&self, offline: bool) -> Result<Observation> {
        let mut state = self.state.lock().await;
        if offline {
            state.counters.offline_requests = state.counters.offline_requests.saturating_add(1);
            let mut observation = state.observation.clone();
            observation.offline = true;
            observation.reused = true;
            return Ok(observation);
        }
        let mut receiver = if let Some(receiver) = state.discovery.clone() {
            state.counters.joined_requests = state.counters.joined_requests.saturating_add(1);
            receiver
        } else if state.syncing || !state.budget.admit(Instant::now()) {
            state.counters.reused_observations =
                state.counters.reused_observations.saturating_add(1);
            let mut observation = state.observation.clone();
            observation.reused = true;
            return Ok(observation);
        } else {
            state.counters.discoveries_started =
                state.counters.discoveries_started.saturating_add(1);
            let (sender, receiver) = watch::channel(None);
            state.discovery = Some(receiver.clone());
            let owner = self.clone();
            tokio::spawn(async move {
                let attempted_at = chrono::Utc::now().to_rfc3339();
                let started = Instant::now();
                let transport = owner.transport.clone();
                let result = tokio::task::spawn_blocking(move || {
                    transport
                        .discover()
                        .and_then(Observation::from_advertisement)
                })
                .await;
                let result = match result {
                    Ok(result) => result,
                    Err(error) => Err(anyhow::Error::from(error)),
                };
                let mut state = owner.state.lock().await;
                let mut observation = match result {
                    Ok(observation) => observation,
                    Err(error) => {
                        let mut observation = state.observation.clone();
                        observation.error = Some(error.to_string());
                        observation
                    }
                };
                observation.attempted_at = Some(attempted_at);
                observation.operation_ms =
                    Some(started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
                observation.offline = false;
                observation.reused = false;
                state.observation = observation.clone();
                state.discovery = None;
                let _ = sender.send(Some(observation));
            });
            receiver
        };
        drop(state);
        loop {
            if let Some(observation) = receiver.borrow().clone() {
                return Ok(observation);
            }
            receiver
                .changed()
                .await
                .context("Git discovery owner stopped")?;
        }
    }

    /// One explicit fetch. A denied fetch has a visible next-admission delay; it never
    /// bypasses a discovery read or turns into an independent seat polling loop.
    pub async fn sync(&self) -> Result<SyncAdmission> {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        if state.syncing || state.discovery.is_some() {
            return Ok(SyncAdmission::InFlight);
        }
        let available = state.budget.available_at(now);
        if !state.budget.admit(now) {
            return Ok(SyncAdmission::Deferred {
                retry_after_ms: available
                    .duration_since(now)
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64,
            });
        }
        state.syncing = true;
        state.counters.fetches_started = state.counters.fetches_started.saturating_add(1);
        let (sender, mut receiver) = watch::channel(None);
        let owner = self.clone();
        tokio::spawn(async move {
            let transport = owner.transport.clone();
            let result = tokio::task::spawn_blocking(move || transport.sync()).await;
            owner.state.lock().await.syncing = false;
            let result = match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let _ = sender.send(Some(result));
        });
        drop(state);
        loop {
            if let Some(result) = receiver.borrow().clone() {
                result.map_err(anyhow::Error::msg)?;
                return Ok(SyncAdmission::Completed);
            }
            receiver.changed().await.context("Git sync owner stopped")?;
        }
    }

    /// Fixed-size diagnostic reduction; reading it does not perform Git I/O.
    pub async fn diagnostics(&self) -> Diagnostics {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        let available = state.budget.available_at(now);
        Diagnostics {
            counters: state.counters.clone(),
            remote_read_operations_in_last_minute: state.budget.starts.len(),
            next_read_after_ms: available
                .duration_since(now)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            in_flight: state.syncing || state.discovery.is_some(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SyncAdmission {
    Completed,
    InFlight,
    Deferred { retry_after_ms: u64 },
}

#[cfg(test)]
mod tests;
