//! Each session lives on its own thread: `Emu` is not `Send`, and sessions step concurrently.

use dust_agent::{Session, SessionOptions};
use std::{path::PathBuf, sync::mpsc, thread};
use tokio::sync::oneshot;

type Job = Box<dyn FnOnce(&mut Session) + Send>;

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
}

impl Worker {
    /// Creates the session on a new thread and returns once it is built.
    pub async fn spawn(
        name: String,
        rom: PathBuf,
        opts: SessionOptions,
    ) -> Result<Self, dust_agent::Error> {
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = oneshot::channel();
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
                while let Ok(job) = jobs_rx.recv() {
                    job(&mut session);
                }
            })
            .expect("couldn't spawn session thread");
        match ready_rx.await {
            Ok(Ok(())) => Ok(Worker { jobs: jobs_tx }),
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
            .send(Box::new(move |session| {
                let _ = tx.send(f(session));
            }))
            .map_err(|_| CallError::Stopped)?;
        rx.await.map_err(|_| CallError::Stopped)
    }
}
