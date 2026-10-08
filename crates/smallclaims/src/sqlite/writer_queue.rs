//! One writer scheduler with opt-in background loans. Existing foreground traffic is FIFO.
use super::*;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, TryRecvError};

/// Maximum foreground loan/batch turns while a background loan remains pending.
/// This bounds dispatch overtaking, not elapsed time or SQL work inside a holder.
pub const FOREGROUND_TURNS: usize = 8;

pub(super) enum LoanClass {
    Foreground,
    Background,
    Fence,
}

#[derive(Default)]
struct Pending {
    last: u64,
    closed: bool,
    jobs: VecDeque<(u64, WriterJob)>,
}

/// Internal queue carried by a notification in the original admission channel.
#[doc(hidden)]
#[derive(Default)]
pub struct BackgroundQueue(Mutex<Pending>);

impl BackgroundQueue {
    /// Only transition from empty needs a wake; no per-job scan or notification backlog.
    pub(super) fn push(&self, job: WriterJob) -> bool {
        let mut pending = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(!pending.closed, "the background writer queue is closed");
        let wake = pending.jobs.is_empty();
        pending.last = pending
            .last
            .checked_add(1)
            .expect("writer admission exhausted");
        let sequence = pending.last;
        pending.jobs.push_back((sequence, job));
        wake
    }

    pub(super) fn watermark(&self) -> u64 {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).last
    }

    fn pop(&self, through: u64) -> Option<WriterJob> {
        let mut pending = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if pending
            .jobs
            .front()
            .is_some_and(|(sequence, _)| *sequence <= through)
        {
            pending.jobs.pop_front().map(|(_, job)| job)
        } else {
            None
        }
    }

    fn has_pending(&self) -> bool {
        !self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .jobs
            .is_empty()
    }

    pub(super) fn close(&self) {
        let jobs = {
            let mut pending = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            pending.closed = true;
            std::mem::take(&mut pending.jobs)
        };
        // Release abandoned borrowers without retaining callback/channel destructors in a lock.
        drop(jobs);
    }
}

#[derive(Default)]
pub(super) struct BackgroundAdmission {
    pub queue: std::sync::OnceLock<Arc<BackgroundQueue>>,
    pub closed: AtomicBool,
}

pub(super) struct Admission {
    lifetime: Arc<BackgroundAdmission>,
    pub queue: Receiver<WriterJob>,
    next: Option<WriterJob>,
    background: Option<Arc<BackgroundQueue>>,
    foreground_turns: usize,
    #[cfg(test)]
    inspected: usize,
}

impl Admission {
    pub fn new(queue: Receiver<WriterJob>, lifetime: Arc<BackgroundAdmission>) -> Self {
        Self {
            lifetime,
            queue,
            next: None,
            background: None,
            foreground_turns: 0,
            #[cfg(test)]
            inspected: 0,
        }
    }

    pub fn put_back(&mut self, job: Option<WriterJob>) {
        debug_assert!(self.next.is_none());
        self.next = job;
    }

    pub fn next(&mut self) -> Option<WriterJob> {
        loop {
            let job = match self.next.take() {
                Some(job) => job,
                None if self.background.is_none() => self.queue.recv().ok()?,
                None => match self.queue.try_recv() {
                    Ok(job) => job,
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                        if let Some(job) = self.background.as_ref()?.pop(u64::MAX) {
                            self.foreground_turns = 0;
                            return Some(job);
                        }
                        self.queue.recv().ok()?
                    }
                },
            };
            #[cfg(test)]
            {
                self.inspected += 1;
            }
            match job {
                WriterJob::BackgroundReady(queue) => {
                    debug_assert!(
                        self.background
                            .as_ref()
                            .is_none_or(|old| Arc::ptr_eq(old, &queue))
                    );
                    self.background = Some(queue);
                }
                job => {
                    if let Some(background) = &self.background {
                        let through = match &job {
                            WriterJob::FenceLend { through, .. } => Some(*through),
                            _ if self.foreground_turns >= FOREGROUND_TURNS => Some(u64::MAX),
                            _ => None,
                        };
                        if let Some(through) = through
                            && let Some(loan) = background.pop(through)
                        {
                            self.next = Some(job);
                            self.foreground_turns = 0;
                            return Some(loan);
                        }
                        if background.has_pending() {
                            self.foreground_turns += 1;
                        } else {
                            self.foreground_turns = 0;
                        }
                    }
                    return Some(job);
                }
            }
        }
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        // Includes worker failure while it holds a loan: pending background receivers must
        // disconnect just like the original foreground channel, rather than wait forever.
        self.lifetime.closed.store(true, Ordering::Release);
        if let Some(background) = self.lifetime.queue.get() {
            background.close();
        }
    }
}

#[cfg(test)]
mod tests;
