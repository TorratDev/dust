//! Each session lives on its own thread: `Emu` is not `Send`, and sessions step concurrently.
//! Hosted sessions live on a thread their host runs (the desktop GUI's emulation thread), which
//! polls for jobs with [`HostedJobs::run_pending`].

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
    hosted: bool,
}

/// The host's end of a hosted session's job channel.
pub struct HostedJobs {
    jobs: mpsc::Receiver<Job>,
}

impl HostedJobs {
    /// Runs the queued jobs on `session`, stopping early after one requests a reset (see
    /// [`Session::reset_requested`]) so the host can rebuild the emulator before the next job.
    /// Returns whether any job ran.
    pub fn run_pending(&self, session: &mut Session) -> bool {
        let mut ran = false;
        while !session.reset_requested() {
            let Ok(job) = self.jobs.try_recv() else {
                break;
            };
            job(session);
            ran = true;
        }
        ran
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
            Ok(Ok(())) => Ok(Worker {
                jobs: jobs_tx,
                hosted: false,
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => panic!("session thread exited during creation"),
        }
    }

    /// A worker for a session that lives on its host's thread; the host runs the jobs.
    pub fn hosted() -> (Self, HostedJobs) {
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        (
            Worker {
                jobs: jobs_tx,
                hosted: true,
            },
            HostedJobs { jobs: jobs_rx },
        )
    }

    pub fn is_hosted(&self) -> bool {
        self.hosted
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
