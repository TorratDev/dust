//! Client for dust's gRPC control API.
//!
//! dust itself runs in its own process (`dust-headless`); this crate only speaks the protocol in
//! `proto/dust_agent.proto`. Either [`launch`] a local `dust-headless` and get a [`Session`] on it,
//! or [`Client::connect`] to a running server and [`Client::create_session`].
//!
//! ```no_run
//! # async fn run() -> Result<(), dust_client::Error> {
//! let session = dust_client::launch("game.nds", &Default::default()).await?;
//! session.cycle(60).await?;
//! assert_eq!(session.status().await?.frame, 60);
//! # Ok(())
//! # }
//! ```

pub mod pb {
    tonic::include_proto!("dust_agent");
}

pub use pb::{Button, Control, Cpu, MemRange, Regs, WatchChange};

use pb::dust_agent_client::DustAgentClient;
use std::{
    fmt, io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};
use tonic::transport::Channel;

/// Environment variable naming the `dust-headless` binary used by [`launch`] when
/// [`LaunchOptions::headless_bin`] is unset.
pub const HEADLESS_BIN_ENV: &str = "DUST_HEADLESS";

const LISTENING_PREFIX: &str = "dust-headless listening on ";
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum Error {
    /// Couldn't connect to the server.
    Connect(tonic::transport::Error),
    /// The server answered with an error.
    Rpc(tonic::Status),
    /// Couldn't start `dust-headless`.
    Launch(String),
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Connect(err) => write!(f, "couldn't connect to dust: {err}"),
            Error::Rpc(status) => {
                write!(f, "dust returned {:?}: {}", status.code(), status.message())
            }
            Error::Launch(msg) => write!(f, "couldn't launch dust-headless: {msg}"),
            Error::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Connect(err) => Some(err),
            Error::Rpc(status) => Some(status),
            Error::Launch(_) => None,
            Error::Io(err) => Some(err),
        }
    }
}

impl From<tonic::Status> for Error {
    fn from(status: tonic::Status) -> Self {
        Error::Rpc(status)
    }
}

impl From<tonic::transport::Error> for Error {
    fn from(err: tonic::transport::Error) -> Self {
        Error::Connect(err)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Emulator settings for a new session. Paths are read by the server.
#[derive(Clone, Debug, Default)]
pub struct SessionOptions {
    /// Firmware dump; dust's built-in firmware is used without one. The boot seed of many games
    /// comes from the firmware MAC, so use the same dump as other emulators to match them.
    pub firmware: Option<PathBuf>,
    /// BIOS dumps. Booting through the firmware needs both; otherwise the ROM is booted directly.
    pub arm7_bios: Option<PathBuf>,
    pub arm9_bios: Option<PathBuf>,
    /// Fixed RTC time in Unix seconds (UTC), for reproducible runs; `None` uses the host clock.
    pub rtc_unix_time: Option<i64>,
}

/// One step of [`Session::sequence`]: hold exactly `buttons` (and touch `touch`, in bottom-screen
/// pixels) for `frames` frames.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputStep {
    pub buttons: Vec<Button>,
    pub touch: Option<(u32, u32)>,
    pub frames: u32,
}

#[derive(Clone, Debug, Default)]
pub struct LaunchOptions {
    /// `dust-headless` binary; defaults to `$DUST_HEADLESS`, then `dust-headless` on `PATH`.
    pub headless_bin: Option<PathBuf>,
    pub session: SessionOptions,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// Frames emulated since the session was created or last reset.
    pub frame: u64,
    pub game_title: String,
    pub paused: bool,
    pub control: Control,
}

impl From<pb::SessionStatus> for Status {
    fn from(status: pb::SessionStatus) -> Self {
        Status {
            frame: status.frame,
            control: status.control(),
            game_title: status.game_title,
            paused: status.paused,
        }
    }
}

fn absolute(path: &Path) -> Result<String> {
    Ok(std::path::absolute(path)?.to_string_lossy().into_owned())
}

fn optional_absolute(path: &Option<PathBuf>) -> Result<String> {
    path.as_deref()
        .map(absolute)
        .transpose()
        .map(Option::unwrap_or_default)
}

/// A connection to a dust server.
#[derive(Clone)]
pub struct Client {
    grpc: DustAgentClient<Channel>,
}

impl Client {
    /// Connects to a server, e.g. `"http://127.0.0.1:50051"`.
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self> {
        Ok(Client {
            grpc: DustAgentClient::connect(endpoint.into()).await?,
        })
    }

    /// The generated gRPC client, for RPCs this crate doesn't wrap yet.
    pub fn grpc(&self) -> DustAgentClient<Channel> {
        self.grpc.clone()
    }

    /// Starts a new emulator session on the server with `rom`.
    pub async fn create_session(
        &self,
        rom: impl AsRef<Path>,
        opts: &SessionOptions,
    ) -> Result<Session> {
        let response = self
            .grpc()
            .create_session(pb::CreateSessionRequest {
                rom_path: absolute(rom.as_ref())?,
                firmware_path: optional_absolute(&opts.firmware)?,
                arm7_bios_path: optional_absolute(&opts.arm7_bios)?,
                arm9_bios_path: optional_absolute(&opts.arm9_bios)?,
                rtc_unix_time: opts.rtc_unix_time,
            })
            .await?
            .into_inner();
        Ok(Session {
            client: self.clone(),
            id: response.session_id,
            _server: None,
        })
    }
}

/// A `dust-headless` process started by [`launch`]; killed when the last session using it drops.
struct ServerProcess {
    _child: Child,
}

/// Spawns `dust-headless` on a free local port, connects to it and creates a session with `rom`.
/// The server process is killed when the returned session (and its clones' sources) drop.
pub async fn launch(rom: impl AsRef<Path>, opts: &LaunchOptions) -> Result<Session> {
    let bin = opts
        .headless_bin
        .clone()
        .or_else(|| std::env::var_os(HEADLESS_BIN_ENV).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("dust-headless"));
    let mut child = Command::new(&bin)
        .args(["--port", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| Error::Launch(format!("couldn't run {}: {err}", bin.display())))?;

    let stdout = child.stdout.take().expect("stdout is piped");
    let mut lines = BufReader::new(stdout).lines();
    let addr = tokio::time::timeout(LAUNCH_TIMEOUT, async {
        while let Some(line) = lines.next_line().await? {
            if let Some(addr) = line.strip_prefix(LISTENING_PREFIX) {
                return Ok(addr.trim().to_owned());
            }
        }
        Err(Error::Launch(
            "dust-headless exited before listening".to_owned(),
        ))
    })
    .await
    .map_err(|_| Error::Launch("timed out waiting for dust-headless to listen".to_owned()))??;

    let client = Client::connect(format!("http://{addr}")).await?;
    let mut session = client.create_session(rom, &opts.session).await?;
    session._server = Some(Arc::new(ServerProcess { _child: child }));
    Ok(session)
}

/// One emulator instance on a server.
#[derive(Clone)]
pub struct Session {
    client: Client,
    id: String,
    _server: Option<Arc<ServerProcess>>,
}

impl Session {
    /// See [`launch`].
    pub async fn launch(rom: impl AsRef<Path>, opts: &LaunchOptions) -> Result<Self> {
        launch(rom, opts).await
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    fn request(&self) -> pb::SessionRequest {
        pb::SessionRequest {
            session_id: self.id.clone(),
        }
    }

    pub async fn status(&self) -> Result<Status> {
        let status = self.client.grpc().get_status(self.request()).await?;
        Ok(status.into_inner().into())
    }

    /// Emulates `frames` frames and returns the status afterwards.
    pub async fn cycle(&self, frames: u32) -> Result<Status> {
        Ok(self.step(frames).await?.0)
    }

    /// Like [`Session::cycle`], also returning the watched ranges that changed (see
    /// [`Session::watch`]).
    pub async fn step(&self, frames: u32) -> Result<(Status, Vec<WatchChange>)> {
        let response = self
            .client
            .grpc()
            .step(pb::StepRequest {
                session_id: self.id.clone(),
                frames,
            })
            .await?
            .into_inner();
        Ok((response.status.unwrap_or_default().into(), response.changes))
    }

    /// Reads `len` bytes at `addr` as `cpu` sees them.
    pub async fn read(&self, cpu: Cpu, addr: u32, len: u32) -> Result<Vec<u8>> {
        let response = self
            .client
            .grpc()
            .read(pb::ReadRequest {
                session_id: self.id.clone(),
                cpu: cpu as i32,
                addr,
                len,
            })
            .await?;
        Ok(response.into_inner().data)
    }

    pub async fn read_u8(&self, cpu: Cpu, addr: u32) -> Result<u8> {
        Ok(self.read(cpu, addr, 1).await?[0])
    }

    pub async fn read_u16(&self, cpu: Cpu, addr: u32) -> Result<u16> {
        let data = self.read(cpu, addr, 2).await?;
        Ok(u16::from_le_bytes([data[0], data[1]]))
    }

    pub async fn read_u32(&self, cpu: Cpu, addr: u32) -> Result<u32> {
        let data = self.read(cpu, addr, 4).await?;
        Ok(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
    }

    /// Reads several ranges in one round trip.
    pub async fn read_many(&self, cpu: Cpu, ranges: &[MemRange]) -> Result<Vec<Vec<u8>>> {
        let response = self
            .client
            .grpc()
            .read_many(pb::ReadManyRequest {
                session_id: self.id.clone(),
                cpu: cpu as i32,
                ranges: ranges.to_vec(),
            })
            .await?;
        Ok(response.into_inner().data)
    }

    async fn write_request(
        &self,
        cpu: Cpu,
        addr: u32,
        value: pb::write_request::Value,
    ) -> Result<()> {
        self.client
            .grpc()
            .write(pb::WriteRequest {
                session_id: self.id.clone(),
                cpu: cpu as i32,
                addr,
                value: Some(value),
            })
            .await?;
        Ok(())
    }

    /// Writes `data` byte by byte.
    pub async fn write(&self, cpu: Cpu, addr: u32, data: &[u8]) -> Result<()> {
        self.write_request(cpu, addr, pb::write_request::Value::Data(data.to_vec()))
            .await
    }

    /// Writes `value` as a single `size`-byte (1, 2 or 4) little-endian access.
    pub async fn write_value(&self, cpu: Cpu, addr: u32, value: u32, size: u32) -> Result<()> {
        self.write_request(
            cpu,
            addr,
            pb::write_request::Value::Word(pb::ValueWrite { value, size }),
        )
        .await
    }

    pub async fn write_u8(&self, cpu: Cpu, addr: u32, value: u8) -> Result<()> {
        self.write_value(cpu, addr, value.into(), 1).await
    }

    pub async fn write_u16(&self, cpu: Cpu, addr: u32, value: u16) -> Result<()> {
        self.write_value(cpu, addr, value.into(), 2).await
    }

    pub async fn write_u32(&self, cpu: Cpu, addr: u32, value: u32) -> Result<()> {
        self.write_value(cpu, addr, value, 4).await
    }

    /// r0-r15 (r15 reads ahead of the executing instruction) and cpsr.
    pub async fn regs(&self, cpu: Cpu) -> Result<Regs> {
        let response = self
            .client
            .grpc()
            .get_regs(pb::RegsRequest {
                session_id: self.id.clone(),
                cpu: cpu as i32,
            })
            .await?;
        Ok(response.into_inner())
    }

    /// `reg` 0-14 are r0-r14, 15 sets the next instruction to execute, 16 is cpsr.
    pub async fn set_reg(&self, cpu: Cpu, reg: u32, value: u32) -> Result<()> {
        self.client
            .grpc()
            .set_reg(pb::SetRegRequest {
                session_id: self.id.clone(),
                cpu: cpu as i32,
                reg,
                value,
            })
            .await?;
        Ok(())
    }

    /// Watches `len` bytes at `addr`; changes are returned by [`Session::step`].
    pub async fn watch(&self, cpu: Cpu, addr: u32, len: u32) -> Result<u32> {
        let response = self
            .client
            .grpc()
            .watch(pb::WatchRequest {
                session_id: self.id.clone(),
                cpu: cpu as i32,
                addr,
                len,
            })
            .await?;
        Ok(response.into_inner().watch_id)
    }

    pub async fn unwatch(&self, watch_id: u32) -> Result<()> {
        self.client
            .grpc()
            .unwatch(pb::UnwatchRequest {
                session_id: self.id.clone(),
                watch_id,
            })
            .await?;
        Ok(())
    }

    pub async fn pause(&self) -> Result<Status> {
        Ok(self
            .client
            .grpc()
            .pause(self.request())
            .await?
            .into_inner()
            .into())
    }

    pub async fn resume(&self) -> Result<Status> {
        Ok(self
            .client
            .grpc()
            .resume(self.request())
            .await?
            .into_inner()
            .into())
    }

    /// Restarts the emulator with the same ROM and options; the frame counter goes back to 0.
    pub async fn reset(&self) -> Result<Status> {
        Ok(self
            .client
            .grpc()
            .reset(self.request())
            .await?
            .into_inner()
            .into())
    }

    fn buttons_request(&self, buttons: &[Button]) -> pb::ButtonsRequest {
        pb::ButtonsRequest {
            session_id: self.id.clone(),
            buttons: buttons.iter().map(|&button| button as i32).collect(),
        }
    }

    /// Holds `buttons` for `frames` frames, then releases them.
    pub async fn press(&self, buttons: &[Button], frames: u32) -> Result<Status> {
        let response = self
            .client
            .grpc()
            .press(pb::PressRequest {
                session_id: self.id.clone(),
                buttons: buttons.iter().map(|&button| button as i32).collect(),
                frames,
            })
            .await?
            .into_inner();
        Ok(response.status.unwrap_or_default().into())
    }

    /// Holds `buttons` from the next frame on, until [`Session::release`].
    pub async fn hold(&self, buttons: &[Button]) -> Result<()> {
        self.client
            .grpc()
            .hold(self.buttons_request(buttons))
            .await?;
        Ok(())
    }

    pub async fn release(&self, buttons: &[Button]) -> Result<()> {
        self.client
            .grpc()
            .release(self.buttons_request(buttons))
            .await?;
        Ok(())
    }

    /// Touches pixel (`x`, `y`) of the 256x192 bottom screen for `frames` frames, then lifts the
    /// pen. With `frames` = 0 the touch stays until [`Session::touch_end`].
    pub async fn touch(&self, x: u32, y: u32, frames: u32) -> Result<Status> {
        let response = self
            .client
            .grpc()
            .touch(pb::TouchRequest {
                session_id: self.id.clone(),
                x,
                y,
                frames,
            })
            .await?
            .into_inner();
        Ok(response.status.unwrap_or_default().into())
    }

    pub async fn touch_end(&self) -> Result<()> {
        self.client.grpc().touch_end(self.request()).await?;
        Ok(())
    }

    /// Runs `steps` in order; afterwards the held buttons and touch are back to what they were.
    pub async fn sequence(&self, steps: &[InputStep]) -> Result<Status> {
        let response = self
            .client
            .grpc()
            .sequence(pb::SequenceRequest {
                session_id: self.id.clone(),
                steps: steps
                    .iter()
                    .map(|step| pb::InputStep {
                        buttons: step.buttons.iter().map(|&button| button as i32).collect(),
                        touch: step.touch.map(|(x, y)| pb::TouchPoint { x, y }),
                        frames: step.frames,
                    })
                    .collect(),
            })
            .await?
            .into_inner();
        Ok(response.status.unwrap_or_default().into())
    }

    /// Replaces the held buttons with a raw mask in py-desmume's bit order (`1 << (KEY_x - 1)`),
    /// so existing py-desmume input files work unchanged.
    pub async fn keypad(&self, mask: u32) -> Result<()> {
        self.client
            .grpc()
            .set_keypad(pb::KeypadRequest {
                session_id: self.id.clone(),
                mask,
            })
            .await?;
        Ok(())
    }

    /// Ends the session on the server.
    pub async fn destroy(self) -> Result<()> {
        self.client.grpc().destroy_session(self.request()).await?;
        Ok(())
    }
}
