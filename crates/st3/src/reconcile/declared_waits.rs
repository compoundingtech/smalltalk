//! A held step's declared wait (`st work wait --on TARGET`) is watched as part of its run's
//! evaluation. When the target moves, st records `work.wait-ended` on the step, which starts its
//! execution clock again, and wakes the holder unless the target already did:
//!
//! - a pull request wakes the seat through the `gh watch` the wait set. The wait ends when that
//!   watch ends: the pull request merged or closed (the watch's last wake told the seat), the
//!   watch reached its deadline, or the seat unwatched it.
//! - a step: when it completes, fails or is cancelled, st wakes the seat.
//! - a time: when it comes, st wakes the seat. Until then the run's evaluation is due at that
//!   time, so nothing polls.
//!
//! Each check reads the step's newest claims and one target: a watch, a step or the clock.
use super::*;
use crate::github_watch::ThreadRef;
use crate::store::{WaitWake, describe_wait};

impl<R: RuntimeControl> Reconciler<R> {
    /// End `view`'s open declared wait if its target moved. Returns whether it ended.
    pub(super) fn watch_declared_wait(&self, view: &StepRunView) -> Result<bool> {
        let Some(holder) = view.claimant.as_deref() else {
            return Ok(false);
        };
        let Some(wait) = self.store.declared_wait(&view.subject, view.attempt)? else {
            return Ok(false);
        };
        let now = now_ms();
        let (reason, woke) = match wait.kind.as_str() {
            "pull-request" => {
                let Ok(thread) = ThreadRef::parse(&wait.subject) else {
                    return Ok(false);
                };
                let watch = thread.watch(holder);
                smallclaims::touched::note_read(|| watch.clone());
                if self.store.live_watch(&watch)?.is_some() {
                    return Ok(false);
                }
                let ended = self
                    .store
                    .last_watch_end(&watch)?
                    .unwrap_or_else(|| "it is gone".to_owned());
                (format!("the watch on {} ended: {ended}", wait.subject), true)
            }
            "step" => {
                smallclaims::touched::note_read(|| wait.subject.clone());
                match self.store.step_run(&wait.subject)? {
                    Some(target)
                        if !matches!(
                            target.status.as_str(),
                            "completed" | "failed" | "cancelled"
                        ) =>
                    {
                        return Ok(false);
                    }
                    Some(target) => (format!("{} {}", wait.subject, target.status), false),
                    None => (format!("{} no longer exists", wait.subject), false),
                }
            }
            "time" => {
                let until = wait.until_unix_ms.unwrap_or_default();
                if until > now {
                    self.arm_deadline_at(
                        &format!("declared-wait:{}", view.subject),
                        until,
                        crate::reconcile_telemetry::WakeCause::TimerStepTimeout,
                    );
                    return Ok(false);
                }
                (format!("{} came", wait.subject), false)
            }
            _ => return Ok(false),
        };
        let wake = (!woke).then(|| {
            let title = view.title.as_deref().unwrap_or(&view.step);
            WaitWake {
                agent: holder.to_owned(),
                title: format!("Wait over: {title}"),
                content: format!(
                    "The wait you declared on {} is over: {reason}.\n\nStep: {}\nTitle: {title}\n\n\
                     Continue the work, declare what it waits on next with `st work wait {} --on TARGET --as {holder}`, \
                     or release the step with `st work release {} --as {holder}`.",
                    describe_wait(&wait),
                    view.subject,
                    view.subject,
                    view.subject,
                ),
            }
        });
        let ended = self.store.end_declared_wait(
            &view.subject,
            view.attempt,
            &wait.claim,
            &reason,
            wake.as_ref(),
        )?;
        if ended {
            self.signal_changed();
        }
        Ok(ended)
    }
}
