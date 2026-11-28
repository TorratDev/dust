//! Each session lives on its own thread: `Emu` is not `Send`, and sessions step concurrently.
//!
//! On a breakpoint or watchpoint hit the thread sends the hit on the session's event channel and,
//! while emulation stays stopped mid-frame, keeps running the jobs it receives (memory reads,
//! hook changes, ...) until one of them requests `Continue`.

use crate::pb;
use dust_agent::{Session, SessionOptions};
use std::{future::Future, ops::ControlFlow, path::PathBuf, rc::Rc, sync::mpsc, thread};
use tokio::sync::{broadcast, oneshot};

type Job = Box<dyn FnOnce(&mut Session) + Send>;

/// Events buffered per subscriber before the slowest one starts missing them.
const EVENT_CAPACITY: usize = 1024;

#[derive(Debug)]
pub enum CallError {
    /// The worker thread is gone (the session was destroyed or its thread panicked).
    Stopped,
}

/// Handle to a session's worker thread; dropping every clone stops the thread once its current
/// job finishes (or, at a hook hit, makes the running step fail).
#[derive(Clone)]
pub struct Worker {
    jobs: mpsc::Sender<Job>,
    events: broadcast::Sender<pb::Event>,
}

impl Worker {
    /// Creates the session on a new thread and returns once it is built.
    pub async fn spawn(
        name: String,
        rom: PathBuf,
        opts: SessionOptions,
    ) -> Result<Self, dust_agent::Error> {
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let (events_tx, _) = broadcast::channel(EVENT_CAPACITY);
        let (ready_tx, ready_rx) = oneshot::channel();
        let thread_events = events_tx.clone();
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
                session.subscribe_logs(Box::new({
                    let events = thread_events.clone();
                    move |entry| {
                        let _ = events.send(crate::log_event(entry));
                        true
                    }
                }));
                let jobs_rx = Rc::new(jobs_rx);
                session.set_hit_handler(Some(Box::new({
                    let jobs_rx = Rc::clone(&jobs_rx);
                    move |session, hit| {
                        // No subscriber is fine: `Events` replays the current hit on subscribing.
                        let _ = thread_events.send(crate::hit_event(hit));
                        while !session.take_continue() {
                            match jobs_rx.recv() {
                                Ok(job) => job(session),
                                Err(_) => return ControlFlow::Break(()),
                            }
                        }
                        ControlFlow::Continue(())
                    }
                })));
                while let Ok(job) = jobs_rx.recv() {
                    job(&mut session);
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

    /// Runs `f` on the session's thread and returns its result. The job is queued right away, and
    /// the returned future doesn't keep the worker alive.
    pub fn call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Session) -> R + Send + 'static,
    ) -> impl Future<Output = Result<R, CallError>> + Send + 'static {
        let (tx, rx) = oneshot::channel();
        let sent = self.jobs.send(Box::new(move |session| {
            let _ = tx.send(f(session));
        }));
        async move {
            sent.map_err(|_| CallError::Stopped)?;
            rx.await.map_err(|_| CallError::Stopped)
        }
    }

    /// Subscribes to hook hits and log entries, returning the hit the session is stopped at, if any. Both happen
    /// on the session's thread, which sends the hits, so no hit is missed or seen twice.
    pub fn subscribe(
        &self,
    ) -> impl Future<Output = Result<(broadcast::Receiver<pb::Event>, Option<pb::Event>), CallError>>
           + Send
           + 'static {
        let events = self.events.clone();
        self.call(move |session| {
            (
                events.subscribe(),
                session.current_hit().map(crate::hit_event),
            )
        })
    }
}
