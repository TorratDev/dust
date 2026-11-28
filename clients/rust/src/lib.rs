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

pub use pb::{
    Button, Control, Cpu, HookKind, Image, ImageFormat, MemRange, Regs, SaveFormat, StateBytes,
    WatchChange,
};

use pb::dust_agent_client::DustAgentClient;
use std::{
    collections::HashMap,
    fmt,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::Mutex,
};
use tonic::{transport::Channel, Streaming};

/// Environment variable naming the `dust-headless` binary used by [`launch`] when
/// [`LaunchOptions::headless_bin`] is unset.
pub const HEADLESS_BIN_ENV: &str = "DUST_HEADLESS";

const LISTENING_PREFIX: &str = "dust-headless listening on ";
/// Largest request or reply (save states and big reads exceed gRPC's default 4 MiB); the
/// server uses the same limit.
const MAX_MESSAGE_SIZE: usize = 64 << 20;
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
    /// Cartridge save (raw `.sav` or DeSmuME `.dsv`); its size sets the save type. Without one
    /// the cartridge has no save memory until [`Session::import_save`].
    pub save: Option<PathBuf>,
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
            grpc: DustAgentClient::connect(endpoint.into())
                .await?
                .max_decoding_message_size(MAX_MESSAGE_SIZE)
                .max_encoding_message_size(MAX_MESSAGE_SIZE),
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
                save_path: optional_absolute(&opts.save)?,
            })
            .await?
            .into_inner();
        Ok(Session {
            client: self.clone(),
            id: response.session_id,
            _server: None,
            hooks: Arc::default(),
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

/// A breakpoint or watchpoint hit. The emulator is stopped mid-frame until it is continued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub id: u32,
    pub kind: HookKind,
    pub cpu: Cpu,
    /// Executed address for breakpoints, written address for watchpoints.
    pub addr: u32,
    /// Written value for watchpoints (as wide as the store); 0 for breakpoints.
    pub value: u32,
    /// Frames finished before the hit.
    pub frame: u64,
    /// As [`Session::regs`] returns them: r15 reads ahead of the next instruction to execute.
    pub regs: Regs,
    /// Results of the reads registered with the hook, in order.
    pub reads: Vec<Vec<u8>>,
}

impl From<pb::HookHit> for Hit {
    fn from(hit: pb::HookHit) -> Self {
        Hit {
            kind: hit.kind(),
            cpu: hit.cpu(),
            id: hit.id,
            addr: hit.addr,
            value: hit.value,
            frame: hit.frame,
            regs: hit.regs.unwrap_or_default(),
            reads: hit.reads,
        }
    }
}

/// A registered breakpoint or watchpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HookId {
    pub id: u32,
    pub kind: HookKind,
}

type BoxedHandler = Box<dyn FnMut(Session, Hit) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

/// Hit handlers registered with [`Session::on_exec`] and [`Session::on_write`], shared by a
/// session's clones and run by one dispatcher task.
#[derive(Default)]
struct Hooks {
    handlers: Mutex<HashMap<u32, Arc<Mutex<BoxedHandler>>>>,
    dispatcher_started: Mutex<bool>,
}

/// One emulator instance on a server.
#[derive(Clone)]
pub struct Session {
    client: Client,
    id: String,
    _server: Option<Arc<ServerProcess>>,
    hooks: Arc<Hooks>,
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

    /// Both screens, top above bottom (256x384): 3 bytes per pixel for
    /// [`ImageFormat::RawRgb`], a PNG file for [`ImageFormat::Png`].
    pub async fn screenshot(&self, format: ImageFormat) -> Result<Image> {
        Ok(self
            .client
            .grpc()
            .screenshot(pb::ScreenshotRequest {
                session_id: self.id.clone(),
                format: format as i32,
            })
            .await?
            .into_inner())
    }

    /// Writes a state file (the desktop GUI's savestate format) on the server;
    /// `include_save` also stores the cartridge save.
    pub async fn save_state(&self, path: impl AsRef<Path>, include_save: bool) -> Result<()> {
        self.client
            .grpc()
            .save_state(pb::StatePathRequest {
                session_id: self.id.clone(),
                path: absolute(path.as_ref())?,
                include_save,
            })
            .await?;
        Ok(())
    }

    /// Loads a state file from [`Session::save_state`] or the desktop GUI; the frame counter is
    /// restored too. On error the session is unchanged.
    pub async fn load_state(&self, path: impl AsRef<Path>) -> Result<Status> {
        Ok(self
            .client
            .grpc()
            .load_state(pb::StatePathRequest {
                session_id: self.id.clone(),
                path: absolute(path.as_ref())?,
                include_save: false,
            })
            .await?
            .into_inner()
            .into())
    }

    /// An in-memory state, for fast resets with [`Session::load_state_bytes`].
    pub async fn save_state_bytes(&self, include_save: bool) -> Result<StateBytes> {
        Ok(self
            .client
            .grpc()
            .save_state_bytes(pb::SaveStateBytesRequest {
                session_id: self.id.clone(),
                include_save,
            })
            .await?
            .into_inner())
    }

    pub async fn load_state_bytes(&self, state: &StateBytes) -> Result<Status> {
        Ok(self
            .client
            .grpc()
            .load_state_bytes(pb::LoadStateBytesRequest {
                session_id: self.id.clone(),
                state: state.state.clone(),
                save: state.save.clone(),
            })
            .await?
            .into_inner()
            .into())
    }

    /// Writes the cartridge save as a raw `.sav` or a DeSmuME `.dsv` on the server.
    pub async fn export_save(&self, path: impl AsRef<Path>, format: SaveFormat) -> Result<()> {
        self.client
            .grpc()
            .export_save(pb::SaveFileRequest {
                session_id: self.id.clone(),
                path: absolute(path.as_ref())?,
                format: format as i32,
            })
            .await?;
        Ok(())
    }

    /// Replaces the cartridge save with a `.sav` or `.dsv` file (a DeSmuME footer is stripped,
    /// like py-desmume's `backup.import_file`). The save type follows the file size and the save
    /// is kept across [`Session::reset`]; games usually read it after a reset.
    pub async fn import_save(&self, path: impl AsRef<Path>) -> Result<()> {
        self.client
            .grpc()
            .import_save(pb::SaveFileRequest {
                session_id: self.id.clone(),
                path: absolute(path.as_ref())?,
                format: SaveFormat::Raw as i32,
            })
            .await?;
        Ok(())
    }

    /// Stops before `cpu` executes the instruction at `addr`. `reads` are read on every hit and
    /// sent with it. Hits arrive on [`Session::events`] and must be continued with
    /// [`Session::continue_hit`]; [`Session::on_exec`] does both.
    pub async fn break_add(&self, cpu: Cpu, addr: u32, reads: &[MemRange]) -> Result<HookId> {
        let id = self
            .client
            .grpc()
            .break_add(pb::BreakAddRequest {
                session_id: self.id.clone(),
                cpu: cpu as i32,
                addr,
                reads: reads.to_vec(),
            })
            .await?
            .into_inner()
            .id;
        Ok(HookId {
            id,
            kind: HookKind::Breakpoint,
        })
    }

    /// Stops after `cpu` stores to any of the `size` bytes from `addr`; see
    /// [`Session::break_add`].
    pub async fn watch_write_add(
        &self,
        cpu: Cpu,
        addr: u32,
        size: u32,
        reads: &[MemRange],
    ) -> Result<HookId> {
        let id = self
            .client
            .grpc()
            .watch_write_add(pb::WatchWriteAddRequest {
                session_id: self.id.clone(),
                cpu: cpu as i32,
                addr,
                size,
                reads: reads.to_vec(),
            })
            .await?
            .into_inner()
            .id;
        Ok(HookId {
            id,
            kind: HookKind::WriteWatchpoint,
        })
    }

    /// Removes a breakpoint or watchpoint, and its handler if it has one.
    pub async fn remove_hook(&self, hook: HookId) -> Result<()> {
        self.hooks.handlers.lock().await.remove(&hook.id);
        let request = pb::HookRequest {
            session_id: self.id.clone(),
            id: hook.id,
        };
        match hook.kind {
            HookKind::Breakpoint => self.client.grpc().break_remove(request).await?,
            HookKind::WriteWatchpoint => self.client.grpc().watch_write_remove(request).await?,
        };
        Ok(())
    }

    /// Registered breakpoints and watchpoints, oldest first.
    pub async fn hooks(&self) -> Result<Vec<pb::Hook>> {
        Ok(self
            .client
            .grpc()
            .break_list(self.request())
            .await?
            .into_inner()
            .hooks)
    }

    /// The session's event stream: hook hits from now on, starting with the one the emulator is
    /// stopped at, if any.
    pub async fn events(&self) -> Result<Streaming<pb::Event>> {
        Ok(self
            .client
            .grpc()
            .events(self.request())
            .await?
            .into_inner())
    }

    /// Lets the emulator go on after a hit; call it once per hit event.
    pub async fn continue_hit(&self) -> Result<()> {
        self.client.grpc().r#continue(self.request()).await?;
        Ok(())
    }

    /// Calls `handler` whenever `cpu` is about to execute `addr`, then continues. The emulator
    /// stays stopped while the handler's future runs, so it can read memory and use the session.
    ///
    /// ```no_run
    /// # async fn run(session: dust_client::Session) -> Result<(), dust_client::Error> {
    /// use dust_client::{Cpu, MemRange};
    /// let reads = [MemRange { addr: 0x0200_1000, len: 4 }];
    /// session
    ///     .on_exec(Cpu::Arm9, 0x0200_0000, &reads, |_emu, hit| async move {
    ///         println!("r0 = {:#x}, data = {:?}", hit.regs.gprs[0], hit.reads[0]);
    ///     })
    ///     .await?;
    /// session.cycle(60).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn on_exec<F, Fut>(
        &self,
        cpu: Cpu,
        addr: u32,
        reads: &[MemRange],
        handler: F,
    ) -> Result<HookId>
    where
        F: FnMut(Session, Hit) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.add_handled(self.break_add(cpu, addr, reads), handler)
            .await
    }

    /// Calls `handler` after every store by `cpu` to the `size` bytes from `addr`, then
    /// continues; see [`Session::on_exec`].
    pub async fn on_write<F, Fut>(
        &self,
        cpu: Cpu,
        addr: u32,
        size: u32,
        reads: &[MemRange],
        handler: F,
    ) -> Result<HookId>
    where
        F: FnMut(Session, Hit) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.add_handled(self.watch_write_add(cpu, addr, size, reads), handler)
            .await
    }

    async fn add_handled<F, Fut>(
        &self,
        add: impl Future<Output = Result<HookId>>,
        mut handler: F,
    ) -> Result<HookId>
    where
        F: FnMut(Session, Hit) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.start_dispatcher().await?;
        // Holding the lock until the handler is in makes the dispatcher wait for it if the hook
        // fires right away.
        let mut handlers = self.hooks.handlers.lock().await;
        let hook = add.await?;
        let handler: BoxedHandler = Box::new(move |session, hit| Box::pin(handler(session, hit)));
        handlers.insert(hook.id, Arc::new(Mutex::new(handler)));
        Ok(hook)
    }

    async fn start_dispatcher(&self) -> Result<()> {
        let mut started = self.hooks.dispatcher_started.lock().await;
        if *started {
            return Ok(());
        }
        let mut events = self.events().await?;
        // Handlers get a session that doesn't keep a launched server alive, so dropping the
        // user's sessions still shuts it down and ends this task.
        let session = Session {
            client: self.client.clone(),
            id: self.id.clone(),
            _server: None,
            hooks: Arc::clone(&self.hooks),
        };
        tokio::spawn(async move {
            while let Ok(Some(event)) = events.message().await {
                let Some(pb::event::Kind::HookHit(hit)) = event.kind else {
                    continue;
                };
                // Hits of hooks without a handler are left to whoever added them.
                let Some(handler) = session.hooks.handlers.lock().await.get(&hit.id).cloned()
                else {
                    continue;
                };
                let handled = (handler.lock().await)(session.clone(), hit.into());
                handled.await;
                if session.continue_hit().await.is_err() {
                    break;
                }
            }
            *session.hooks.dispatcher_started.lock().await = false;
        });
        *started = true;
        Ok(())
    }

    /// Ends the session on the server.
    pub async fn destroy(self) -> Result<()> {
        self.client.grpc().destroy_session(self.request()).await?;
        Ok(())
    }
}
