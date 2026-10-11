//! Collection-time observations of the daemon's pending work FIFOs.
//! Sends and timestamp insertion share a lock, so concurrent producers cannot
//! disagree with channel order. Receivers remove timestamps before executing work.
use opentelemetry::{KeyValue, metrics::Meter};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use std::time::Instant;

#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Kind {
    Writer,
    Conversation,
    TerminalEmulation,
}
const KINDS: [Kind; 3] = [Kind::Writer, Kind::Conversation, Kind::TerminalEmulation];
impl Kind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Writer => "writer",
            Self::Conversation => "conversation",
            Self::TerminalEmulation => "terminal_emulation",
        }
    }
}
static ENABLED: AtomicBool = AtomicBool::new(false);
static QUEUES: Mutex<[Vec<Weak<Queue>>; 3]> = Mutex::new([Vec::new(), Vec::new(), Vec::new()]);
static ATTRIBUTES: LazyLock<[[KeyValue; 1]; 3]> =
    LazyLock::new(|| KINDS.map(|kind| [KeyValue::new("queue", kind.as_str())]));
static DISABLED: LazyLock<Arc<Queue>> = LazyLock::new(|| {
    Arc::new(Queue {
        enabled: false,
        pending: Mutex::new(VecDeque::new()),
    })
});

pub struct Queue {
    enabled: bool,
    pending: Mutex<VecDeque<Instant>>,
}
impl Queue {
    #[cfg(test)]
    pub(crate) fn enabled_for_test() -> Arc<Self> {
        Arc::new(Self { enabled: true, pending: Mutex::new(VecDeque::new()) })
    }
    #[cfg(test)]
    pub(crate) fn depth_for_test(&self) -> usize {
        self.pending.lock().len()
    }
    pub fn new(kind: Kind) -> Arc<Self> {
        if !ENABLED.load(Ordering::Relaxed) {
            return DISABLED.clone();
        }
        let queue = Arc::new(Self {
            enabled: true,
            pending: Mutex::new(VecDeque::new()),
        });
        let mut queues = QUEUES.lock();
        let instances = &mut queues[kind as usize];
        instances.retain(|queue| queue.strong_count() > 0);
        instances.push(Arc::downgrade(&queue));
        queue
    }
    pub fn send<T, E>(&self, send: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
        if !self.enabled {
            return send();
        }
        self.send_at(Instant::now(), send)
    }
    /// Reuse a work item's existing enqueue clock instead of reading it twice.
    pub fn send_at<T, E>(
        &self,
        enqueued: Instant,
        send: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        if !self.enabled {
            return send();
        }
        let mut pending = self.pending.lock();
        let result = send();
        if result.is_ok() {
            pending.push_back(enqueued);
        }
        result
    }
    pub fn dequeued(&self) {
        if !self.enabled {
            return;
        }
        self.pending.lock().pop_front();
    }
    /// Writer admission can dispatch foreground work ahead of background loans.
    /// Remove the dispatched item's clock, not the oldest still-pending loan.
    pub fn dequeued_at(&self, enqueued: Instant) {
        if !self.enabled {
            return;
        }
        let mut pending = self.pending.lock();
        if let Some(index) = pending.iter().position(|clock| *clock == enqueued) {
            pending.remove(index);
        }
    }
    /// Own the receiving end so discarded work stops contributing to backlog.
    pub fn receiver<T>(self: &Arc<Self>, receiver: T) -> Receiver<T> {
        Receiver { inner: Some(receiver), queue: self.clone() }
    }
}

pub struct Receiver<T> {
    inner: Option<T>,
    queue: Arc<Queue>,
}
impl<T> std::ops::Deref for Receiver<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.inner.as_ref().expect("receiver is present until drop")
    }
}
impl<T> std::ops::DerefMut for Receiver<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.inner.as_mut().expect("receiver is present until drop")
    }
}
impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // Close first: after this, even concurrent sends cannot add pending work.
        drop(self.inner.take());
        if self.queue.enabled {
            self.queue.pending.lock().clear();
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Observation {
    depth: u64,
    oldest_age: f64,
}
fn observations() -> [Observation; 3] {
    let mut observations = [Observation::default(); 3];
    let mut queues = QUEUES.lock();
    for (instances, observation) in queues.iter_mut().zip(&mut observations) {
        instances.retain(|instance| {
            let Some(queue) = instance.upgrade() else {
                return false;
            };
            let pending = queue.pending.lock();
            observation.depth += pending.len() as u64;
            if let Some(oldest) = pending.front() {
                observation.oldest_age = observation.oldest_age.max(oldest.elapsed().as_secs_f64());
            }
            true
        });
    }
    observations
}

/// Called once after the daemon installs its meter provider, before it opens stores.
pub fn init(meter: &Meter) {
    LazyLock::force(&ATTRIBUTES);
    meter
        .u64_observable_gauge("st.fifo.depth")
        .with_unit("{item}")
        .with_callback(|observer| {
            for (observation, attributes) in observations().iter().zip(ATTRIBUTES.iter()) {
                observer.observe(observation.depth, attributes);
            }
        })
        .build();
    meter
        .f64_observable_gauge("st.fifo.oldest_age")
        .with_unit("s")
        .with_callback(|observer| {
            for (observation, attributes) in observations().iter().zip(ATTRIBUTES.iter()) {
                observer.observe(observation.oldest_age, attributes);
            }
        })
        .build();
    ENABLED.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn otel_fifo_tracks_only_successful_pending_sends() {
        let queue = Queue {
            enabled: true,
            pending: Mutex::new(VecDeque::new()),
        };
        let first = Instant::now();
        queue.send_at(first, || Ok::<_, ()>(())).unwrap();
        assert!(queue.send(|| Err::<(), _>(())).is_err());
        let pending = queue.pending.lock();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending.front(), Some(&first));
        drop(pending);
        queue.dequeued();
        assert!(queue.pending.lock().is_empty());
    }
    #[test]
    fn otel_fifo_overtaking_keeps_oldest_pending_clock() {
        let queue = Queue::enabled_for_test();
        let background = Instant::now();
        let foreground = background + std::time::Duration::from_millis(1);
        queue.send_at(background, || Ok::<_, ()>(())).unwrap();
        queue.send_at(foreground, || Ok::<_, ()>(())).unwrap();
        queue.dequeued_at(foreground);
        assert_eq!(queue.pending.lock().front(), Some(&background));
        assert_eq!(queue.depth_for_test(), 1);
        queue.dequeued_at(background);
        assert_eq!(queue.depth_for_test(), 0);
    }
    #[test]
    fn otel_fifo_drop_receiver_discards_backlog() {
        let queue = Arc::new(Queue {
            enabled: true,
            pending: Mutex::new(VecDeque::new()),
        });
        let (sender, receiver) = std::sync::mpsc::channel();
        let receiver = queue.receiver(receiver);
        for item in 0..3 {
            queue.send(|| sender.send(item)).unwrap();
        }
        assert_eq!(queue.pending.lock().len(), 3);
        drop(receiver);
        assert_eq!(queue.pending.lock().len(), 0);
        assert!(queue.send(|| sender.send(4)).is_err());
        assert!(queue.pending.lock().front().is_none());
    }
}
