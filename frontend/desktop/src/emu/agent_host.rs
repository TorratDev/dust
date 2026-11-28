//! The emulator as the emulation thread holds it ([`Host`]) and, with the `agent-server` feature,
//! the dust-agent gRPC server the GUI can host ([`Server`]).
//!
//! With the server on, the GUI's emulator is exposed as the session [`SESSION_ID`]: agent calls
//! are queued by the server thread and run on the emulation thread once per loop, like the GDB
//! server's packets, and agent input is merged with the keyboard's according to the session's
//! [`Control`] mode.

use crate::input;
use dust_core::{
    cpu::interpreter::Interpreter,
    emu::{Emu, RunOutput},
};
use std::ops::{Deref, DerefMut};

#[cfg(feature = "agent-server")]
pub use server::*;

#[cfg(feature = "agent-server")]
mod server {
    pub use dust_agent::Control;
    use dust_agent_grpc::{AgentService, HostedJobs, Registry};
    use std::{io, net::SocketAddr, sync::mpsc, thread};

    /// Id of the GUI's own session.
    pub const SESSION_ID: &str = "gui";

    /// What the emulation thread needs to serve the GUI session.
    pub struct Link {
        pub jobs: HostedJobs,
        pub control: Control,
        pub game_title: String,
    }

    /// The gRPC server, running on its own thread for the rest of the process.
    pub struct Server {
        registry: Registry,
        addr: SocketAddr,
    }

    impl Server {
        /// Binds `addr` (port 0 picks a free port) and starts serving.
        pub fn start(addr: SocketAddr) -> io::Result<Self> {
            let (ready_tx, ready_rx) = mpsc::channel();
            thread::Builder::new()
                .name("agent server".to_owned())
                .spawn(move || {
                    let runtime = match tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(err) => {
                            let _ = ready_tx.send(Err(err));
                            return;
                        }
                    };
                    runtime.block_on(async move {
                        let (listener, local_addr) = match dust_agent_grpc::bind(addr).await {
                            Ok(bound) => bound,
                            Err(err) => {
                                let _ = ready_tx.send(Err(err));
                                return;
                            }
                        };
                        let service = AgentService::new();
                        let _ = ready_tx.send(Ok((service.registry(), local_addr)));
                        if let Err(err) = dust_agent_grpc::serve_service(listener, service).await {
                            eprintln!("Agent server failed: {err}");
                        }
                    });
                })?;
            let (registry, addr) = ready_rx
                .recv()
                .map_err(|_| io::Error::other("the agent server thread exited"))??;
            Ok(Server { registry, addr })
        }

        pub fn addr(&self) -> SocketAddr {
            self.addr
        }

        /// Exposes a newly started emulator as [`SESSION_ID`], replacing the previous one.
        pub fn link(&self, control: Control, game_title: String) -> Link {
            Link {
                jobs: self.registry.host(SESSION_ID),
                control,
                game_title,
            }
        }

        /// Called when the emulator stops: calls with [`SESSION_ID`] then fail with `NOT_FOUND`.
        pub fn unlink(&self) {
            self.registry.remove(SESSION_ID);
        }
    }

    pub fn parse_control(value: &str) -> Option<Control> {
        match value {
            "agent" => Some(Control::Agent),
            "human" => Some(Control::Human),
            "shared" => Some(Control::Shared),
            _ => None,
        }
    }
}

/// The emulator, dereferencing to [`Emu`]. With the `agent-server` feature it lives in a
/// [`dust_agent::Session`], which applies the merged human and agent input before every frame.
pub struct Host {
    #[cfg(feature = "agent-server")]
    session: dust_agent::Session,
    #[cfg(feature = "agent-server")]
    jobs: Option<dust_agent_grpc::HostedJobs>,
    #[cfg(feature = "agent-server")]
    human_keys: dust_core::emu::input::Keys,
    #[cfg(feature = "agent-server")]
    human_touch: Option<[u16; 2]>,

    #[cfg(not(feature = "agent-server"))]
    emu: Emu<Interpreter>,
}

impl Host {
    #[cfg(not(feature = "agent-server"))]
    pub fn new(emu: Emu<Interpreter>) -> Self {
        Host { emu }
    }

    #[cfg(feature = "agent-server")]
    pub fn new(emu: Emu<Interpreter>, link: Option<Link>) -> Self {
        let (jobs, control, game_title) = match link {
            Some(link) => (Some(link.jobs), link.control, link.game_title),
            // Without a server, only the keyboard drives the emulator, as without the feature.
            None => (None, Control::Human, String::new()),
        };
        let mut session = dust_agent::Session::hosted(emu, game_title);
        session.set_control(control);
        Host {
            session,
            jobs,
            human_keys: dust_core::emu::input::Keys::empty(),
            human_touch: None,
        }
    }

    /// Applies keyboard/mouse changes from the UI.
    pub fn update_input(&mut self, changes: input::Changes) {
        #[cfg(feature = "agent-server")]
        {
            self.human_keys.insert(changes.pressed);
            self.human_keys.remove(changes.released);
            if let Some(touch_pos) = changes.touch_pos {
                self.human_touch = touch_pos;
            }
            self.session
                .set_human_input(self.human_keys, self.human_touch);
        }
        #[cfg(not(feature = "agent-server"))]
        {
            self.emu.press_keys(changes.pressed);
            self.emu.release_keys(changes.released);
            if let Some(new_touch_pos) = changes.touch_pos {
                if let Some(touch_pos) = new_touch_pos {
                    self.emu.set_touch_pos(touch_pos);
                } else {
                    self.emu.end_touch();
                }
            }
        }
    }

    /// Runs the queued agent calls; returns whether one requested a reset, which the caller must
    /// perform (with [`Host::rebuild`]) before polling again.
    pub fn poll_agent(&mut self) -> bool {
        #[cfg(feature = "agent-server")]
        {
            if let Some(jobs) = &self.jobs {
                jobs.run_pending(&mut self.session);
            }
            self.session.reset_requested()
        }
        #[cfg(not(feature = "agent-server"))]
        false
    }

    /// Whether an agent paused the emulator.
    pub fn agent_paused(&self) -> bool {
        #[cfg(feature = "agent-server")]
        return self.session.is_paused();
        #[cfg(not(feature = "agent-server"))]
        false
    }

    #[cfg(feature = "agent-server")]
    pub fn set_control(&mut self, control: Control) {
        self.session.set_control(control);
    }

    /// Runs one frame with `run` (applying the merged input first).
    pub fn run_frame(&mut self, run: impl FnOnce(&mut Emu<Interpreter>) -> RunOutput) -> RunOutput {
        #[cfg(feature = "agent-server")]
        return self.session.run_host_frame(run);
        #[cfg(not(feature = "agent-server"))]
        run(&mut self.emu)
    }

    /// Replaces the emulator with the one `rebuild` makes from it (a reset).
    pub fn rebuild(
        self,
        rebuild: impl FnOnce(Emu<Interpreter>) -> Option<Emu<Interpreter>>,
    ) -> Option<Self> {
        #[cfg(feature = "agent-server")]
        {
            let Host {
                session,
                jobs,
                human_keys,
                human_touch,
            } = self;
            Some(Host {
                session: session.rebuild(rebuild)?,
                jobs,
                human_keys,
                human_touch,
            })
        }
        #[cfg(not(feature = "agent-server"))]
        rebuild(self.emu).map(|emu| Host { emu })
    }
}

impl Deref for Host {
    type Target = Emu<Interpreter>;

    fn deref(&self) -> &Self::Target {
        #[cfg(feature = "agent-server")]
        return self.session.emu_ref();
        #[cfg(not(feature = "agent-server"))]
        &self.emu
    }
}

impl DerefMut for Host {
    fn deref_mut(&mut self) -> &mut Self::Target {
        #[cfg(feature = "agent-server")]
        return self.session.emu();
        #[cfg(not(feature = "agent-server"))]
        &mut self.emu
    }
}
