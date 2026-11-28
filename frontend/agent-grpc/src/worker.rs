//! Each session lives on its own thread: `Emu` is not `Send`, and sessions step concurrently.
//!
//! While a step is stopped at a hook hit with an `Events` subscriber, the session's hit handler
//! keeps serving jobs from the same queue until a `Continue` job arrives, so clients can inspect
//! and change the stopped emulator.

use crate::{hit_to_pb, pb};
use dust_agent::{Session, SessionOptions};
use std::{
    path::PathBuf,
    rc::Rc,
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::Duration,
};
use tokio::sync::{broadcast, oneshot};

type Call = Box<dyn FnOnce(&mut Session) + Send>;

enum Job {
    Call(Call),
    /// Resumes after a hit; answers whether the session was stopped at one.
    Continue(oneshot::Sender<bool>),
}

/// How often a stopped session checks that its `Events` subscribers are still there.
const SUBSCRIBER_POLL: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub enum CallError {
    /// The worker thread is gone (the session was destroyed or its thread panicked).
    Stopped,
}

/// Handle to a session's worker thread; dropping every clone stops the thread once its current
/// job finishes.
#[derive(Clone)]
pub struct Worker {
    jobs: mpsc::Sender<Job>,
    events: broadcast::Sender<pb::Event>,
}

/// Serves jobs while stopped at a hit, until `Continue`, until nobody listens for events any more,
/// or until the session is gone.
fn wait_for_continue(
    session: &mut Session,
    jobs: &mpsc::Receiver<Job>,
    events: &broadcast::Sender<pb::Event>,
) {
    loop {
        match jobs.recv_timeout(SUBSCRIBER_POLL) {
            Ok(Job::Call(call)) => call(session),
            Ok(Job::Continue(reply)) => {
                let _ = reply.send(true);
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                if events.receiver_count() == 0 {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

impl Worker {
    /// Creates the session on a new thread and returns once it is built.
    pub async fn spawn(
        name: String,
        rom: PathBuf,
        opts: SessionOptions,
    ) -> Result<Self, dust_agent::Error> {
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let (events_tx, _) = broadcast::channel(16);
        let (ready_tx, ready_rx) = oneshot::channel();
        let events = events_tx.clone();
        thread::Builder::new()
            .name(format!("dust session {name}"))
            .spawn(move || {
                let mut session = match Session::new(rom, &opts) {
                    Ok(session) => {
                        let _ = ready_tx.send(Ok(()));
                        session
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err));
                        return;
                    }
                };
                let jobs_rx = Rc::new(jobs_rx);
                let handler_jobs = Rc::clone(&jobs_rx);
                session.set_hit_handler(Some(Box::new(move |session, hit| {
                    if events.receiver_count() == 0 {
                        return;
                    }
                    let event = pb::Event {
                        kind: Some(pb::event::Kind::HookHit(hit_to_pb(hit))),
                    };
                    if events.send(event).is_ok() {
                        wait_for_continue(session, &handler_jobs, &events);
                    }
                })));
                while let Ok(job) = jobs_rx.recv() {
                    match job {
                        Job::Call(call) => call(&mut session),
                        Job::Continue(reply) => {
                            let _ = reply.send(false);
                        }
                    }
                }
            })
            .expect("couldn't spawn session thread");
        match ready_rx.await {
            Ok(Ok(())) => Ok(Worker {
                jobs: jobs_tx,
                events: events_tx,
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => panic!("session thread exited during creation"),
        }
    }

    /// Runs `f` on the session's thread and returns its result.
    pub async fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Session) -> R + Send + 'static,
    ) -> Result<R, CallError> {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Job::Call(Box::new(move |session| {
                let _ = tx.send(f(session));
            })))
            .map_err(|_| CallError::Stopped)?;
        rx.await.map_err(|_| CallError::Stopped)
    }

    /// Resumes the session if it is stopped at a hit; returns whether it was.
    pub async fn continue_after_hit(&self) -> Result<bool, CallError> {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Job::Continue(tx))
            .map_err(|_| CallError::Stopped)?;
        rx.await.map_err(|_| CallError::Stopped)
    }

    /// Releases a session stopped at a hit without waiting for the answer (used on destroy).
    pub fn release_hit(&self) {
        let (tx, _) = oneshot::channel();
        let _ = self.jobs.send(Job::Continue(tx));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<pb::Event> {
        self.events.subscribe()
    }
}
