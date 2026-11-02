//! FIFO queue for work a delivered take defers past its lifecycle guard.
//!
//! Each job owns what it writes or removes (metrics, temporary audio) and runs
//! on one background thread in submission order. Storage privacy deletions
//! wait for every job queued or reserved before them, so deferred writes never
//! land after a deletion the user asked for. A worker thread exists only while jobs are
//! queued.
use std::{
    collections::VecDeque,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

type Job = Box<dyn FnOnce() + Send + 'static>;

#[derive(Default)]
struct QueueState {
    jobs: VecDeque<Job>,
    /// Jobs ever queued and finished; a drain waits for the count it saw.
    queued: u64,
    finished: u64,
    /// Set while a worker owns the queue; `worker` names its thread.
    running: bool,
    worker: Option<ThreadId>,
}

#[derive(Default)]
pub(crate) struct CompletionQueue {
    state: Mutex<QueueState>,
    progress: Condvar,
}

impl CompletionQueue {
    fn lock(&self) -> MutexGuard<'_, QueueState> {
        // Jobs run outside the lock and panics are caught, so a poisoned
        // lock still holds consistent counters.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts a job that its owner queues later, so a drain that starts in
    /// between already waits for it. The reservation is released when its
    /// job is queued, or when it is dropped unused.
    pub(crate) fn reserve(self: &Arc<Self>) -> Reservation {
        self.lock().queued += 1;
        Reservation {
            queue: Some(Arc::clone(self)),
        }
    }

    /// Adds an already counted job and starts a worker if none runs.
    fn enqueue(self: &Arc<Self>, job: Job) {
        let mut state = self.lock();
        state.jobs.push_back(job);
        if state.running {
            return;
        }
        state.running = true;
        drop(state);
        let queue = Arc::clone(self);
        let spawned = thread::Builder::new()
            .name("completion-queue".into())
            .spawn(move || queue.run());
        if spawned.is_err() {
            // Without a thread the work still runs, in order, on the caller.
            self.run();
        }
    }

    fn run(&self) {
        let mut state = self.lock();
        state.worker = Some(thread::current().id());
        loop {
            let Some(job) = state.jobs.pop_front() else {
                state.running = false;
                state.worker = None;
                return;
            };
            drop(state);
            // A failing job must not stall later jobs or a waiting deletion.
            let _ = catch_unwind(AssertUnwindSafe(job));
            state = self.lock();
            state.finished += 1;
            self.progress.notify_all();
        }
    }

    /// Waits until every job queued before this call has finished, or until
    /// `timeout` passes. Returns whether they all finished. Called from a job,
    /// it returns at once: the jobs queued before that job already ran.
    pub(crate) fn drain(&self, timeout: Option<Duration>) -> bool {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut state = self.lock();
        if state.worker == Some(thread::current().id()) {
            return true;
        }
        let target = state.queued;
        while state.finished < target {
            state = match deadline {
                None => self
                    .progress
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner),
                Some(deadline) => {
                    let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                        return false;
                    };
                    self.progress
                        .wait_timeout(state, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
        true
    }
}

/// A counted place in the queue for a job its owner has yet to build.
pub(crate) struct Reservation {
    queue: Option<Arc<CompletionQueue>>,
}

impl Reservation {
    /// Queues the reserved job after every job queued before it.
    pub(crate) fn push(mut self, job: Job) {
        if let Some(queue) = self.queue.take() {
            queue.enqueue(job);
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // An unused reservation counts as finished, so no drain waits for it.
        if let Some(queue) = self.queue.take() {
            let mut state = queue.lock();
            state.finished += 1;
            queue.progress.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn jobs_run_in_submission_order_off_the_caller() {
        let queue = Arc::new(CompletionQueue::default());
        let caller = thread::current().id();
        let (sender, receiver) = mpsc::channel();
        for index in 0..50 {
            let sender = sender.clone();
            queue.reserve().push(Box::new(move || {
                sender.send((index, thread::current().id())).unwrap();
            }));
        }
        assert!(queue.drain(None));
        let ran: Vec<_> = receiver.try_iter().collect();
        assert_eq!(
            ran.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            (0..50).collect::<Vec<_>>()
        );
        assert!(ran.iter().all(|(_, thread)| *thread != caller));
    }

    #[test]
    fn a_bounded_drain_reports_unfinished_work_and_a_panic_does_not_stall_the_queue() {
        let queue = Arc::new(CompletionQueue::default());
        let (release, gate) = mpsc::channel::<()>();
        queue.reserve().push(Box::new(move || {
            let _ = gate.recv();
        }));
        queue.reserve().push(Box::new(|| panic!("job failure")));
        let (sender, receiver) = mpsc::channel();
        queue
            .reserve()
            .push(Box::new(move || sender.send(()).unwrap()));
        assert!(!queue.drain(Some(Duration::from_millis(20))));
        release.send(()).unwrap();
        assert!(queue.drain(Some(Duration::from_secs(10))));
        assert!(receiver.try_recv().is_ok());
    }

    #[test]
    fn a_drain_waits_for_a_reservation_until_its_job_ran_or_it_was_dropped() {
        let queue = Arc::new(CompletionQueue::default());
        let reservation = queue.reserve();
        // Work counted before a deletion but not yet queued still blocks it.
        assert!(!queue.drain(Some(Duration::from_millis(20))));
        let (sender, receiver) = mpsc::channel();
        reservation.push(Box::new(move || sender.send(()).unwrap()));
        assert!(queue.drain(Some(Duration::from_secs(10))));
        assert!(receiver.try_recv().is_ok());

        drop(queue.reserve());
        assert!(queue.drain(Some(Duration::from_millis(20))));
    }

    #[test]
    fn a_job_that_drains_does_not_wait_for_itself() {
        let queue = Arc::new(CompletionQueue::default());
        let inner = Arc::clone(&queue);
        let (sender, receiver) = mpsc::channel();
        queue.reserve().push(Box::new(move || {
            sender.send(inner.drain(None)).unwrap();
        }));
        assert_eq!(receiver.recv_timeout(Duration::from_secs(10)), Ok(true));
    }
}
