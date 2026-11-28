//! tonic server for `proto/dust_agent.proto`: many sessions per process, one worker thread each.

mod worker;

pub mod pb {
    tonic::include_proto!("dust_agent");
}

use dust_agent::{Control, ImageFormat, InputStep, Keys, SaveFormat, SessionOptions, LID_CLOSED};
use pb::dust_agent_server::{DustAgent, DustAgentServer};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};
use tokio::net::TcpListener;
use tokio_stream::{wrappers::TcpListenerStream, Stream, StreamExt};
use tonic::{Request, Response, Status};
use worker::{CallError, Worker};

/// Largest request or reply: save states (main RAM alone is 4 MiB) and big reads don't fit in
/// gRPC's default 4 MiB.
pub const MAX_MESSAGE_SIZE: usize = 64 << 20;

#[derive(Default)]
pub struct AgentService {
    sessions: Mutex<HashMap<String, Worker>>,
    next_id: AtomicU64,
}

impl AgentService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_server(self) -> DustAgentServer<Self> {
        DustAgentServer::new(self)
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
    }

    fn worker(&self, session_id: &str) -> Result<Worker, Status> {
        self.sessions
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("no session with id {session_id:?}")))
    }

    async fn call<R: Send + 'static>(
        &self,
        session_id: &str,
        f: impl FnOnce(&mut dust_agent::Session) -> R + Send + 'static,
    ) -> Result<R, Status> {
        self.worker(session_id)?
            .call(f)
            .await
            .map_err(|CallError::Stopped| Status::aborted("the session's worker thread stopped"))
    }
}

/// Serves the API on `listener` until the process exits.
pub async fn serve(listener: TcpListener) -> Result<(), tonic::transport::Error> {
    tonic::transport::Server::builder()
        .add_service(AgentService::new().into_server())
        // Without TCP_NODELAY, replies past a few segments (such as StepBatch with many sessions)
        // stall ~40 ms on Nagle's algorithm meeting the client's delayed ACK.
        .serve_with_incoming(TcpListenerStream::new(listener).map(|stream| {
            stream.inspect(|stream| {
                let _ = stream.set_nodelay(true);
            })
        }))
        .await
}

/// Binds `addr` (port 0 picks a free port) and returns the listener with its bound address.
pub async fn bind(addr: SocketAddr) -> std::io::Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    Ok((listener, local_addr))
}

fn status_to_pb(status: dust_agent::Status) -> pb::SessionStatus {
    pb::SessionStatus {
        frame: status.frame,
        game_title: status.game_title,
        paused: status.paused,
        control: match status.control {
            Control::Agent => pb::Control::Agent,
            Control::Human => pb::Control::Human,
            Control::Shared => pb::Control::Shared,
        } as i32,
    }
}

fn agent_error(err: dust_agent::Error) -> Status {
    match err {
        dust_agent::Error::Shutdown => Status::failed_precondition(err.to_string()),
        dust_agent::Error::NoSave => Status::failed_precondition(err.to_string()),
        _ => Status::invalid_argument(err.to_string()),
    }
}

fn cpu_from_pb(cpu: i32) -> Result<dust_agent::Cpu, Status> {
    match pb::Cpu::try_from(cpu) {
        Ok(pb::Cpu::Arm9) => Ok(dust_agent::Cpu::Arm9),
        Ok(pb::Cpu::Arm7) => Ok(dust_agent::Cpu::Arm7),
        Err(_) => Err(Status::invalid_argument(format!("unknown cpu {cpu}"))),
    }
}

fn keys_from_pb(buttons: &[i32]) -> Result<Keys, Status> {
    buttons.iter().try_fold(Keys::empty(), |keys, &button| {
        let key = match pb::Button::try_from(button) {
            Ok(pb::Button::A) => Keys::A,
            Ok(pb::Button::B) => Keys::B,
            Ok(pb::Button::Select) => Keys::SELECT,
            Ok(pb::Button::Start) => Keys::START,
            Ok(pb::Button::Right) => Keys::RIGHT,
            Ok(pb::Button::Left) => Keys::LEFT,
            Ok(pb::Button::Up) => Keys::UP,
            Ok(pb::Button::Down) => Keys::DOWN,
            Ok(pb::Button::R) => Keys::R,
            Ok(pb::Button::L) => Keys::L,
            Ok(pb::Button::X) => Keys::X,
            Ok(pb::Button::Y) => Keys::Y,
            Ok(pb::Button::Debug) => Keys::DEBUG,
            Ok(pb::Button::LidClosed) => LID_CLOSED,
            Err(_) => return Err(Status::invalid_argument(format!("unknown button {button}"))),
        };
        Ok(keys | key)
    })
}

fn image_format_from_pb(format: i32) -> Result<ImageFormat, Status> {
    match pb::ImageFormat::try_from(format) {
        Ok(pb::ImageFormat::RawRgb) => Ok(ImageFormat::RawRgb),
        Ok(pb::ImageFormat::Png) => Ok(ImageFormat::Png),
        Ok(pb::ImageFormat::RawGray) => Ok(ImageFormat::RawGray),
        Err(_) => Err(Status::invalid_argument(format!(
            "unknown image format {format}"
        ))),
    }
}

fn save_format_from_pb(format: i32) -> Result<SaveFormat, Status> {
    match pb::SaveFormat::try_from(format) {
        Ok(pb::SaveFormat::Raw) => Ok(SaveFormat::Raw),
        Ok(pb::SaveFormat::Dsv) => Ok(SaveFormat::Dsv),
        Err(_) => Err(Status::invalid_argument(format!(
            "unknown save format {format}"
        ))),
    }
}

fn required_path(path: String) -> Result<PathBuf, Status> {
    if path.is_empty() {
        return Err(Status::invalid_argument("path is required"));
    }
    Ok(PathBuf::from(path))
}

fn touch_coord(value: u32) -> u16 {
    // Out-of-range values stay out of range and are rejected by the session.
    value.min(u16::MAX as u32) as u16
}

fn step_response(result: dust_agent::StepResult) -> pb::StepResponse {
    pb::StepResponse {
        status: Some(status_to_pb(result.status)),
        changes: result
            .changes
            .into_iter()
            .map(|change| pb::WatchChange {
                watch_id: change.watch_id,
                addr: change.addr,
                old_data: change.old_data,
                new_data: change.new_data,
            })
            .collect(),
    }
}

fn image_format_to_pb(format: ImageFormat) -> pb::ImageFormat {
    match format {
        ImageFormat::RawRgb => pb::ImageFormat::RawRgb,
        ImageFormat::Png => pb::ImageFormat::Png,
        ImageFormat::RawGray => pb::ImageFormat::RawGray,
    }
}

/// A `BatchItem`, checked and converted before any session steps.
struct BatchJob {
    worker: Worker,
    session_id: String,
    step: InputStep,
}

fn batch_job(service: &AgentService, item: pb::BatchItem) -> Result<BatchJob, Status> {
    let action = item.action.unwrap_or_default();
    if action.discrete.is_some() {
        return Err(Status::failed_precondition(format!(
            "session {:?}: discrete actions need a loaded scenario (LoadScenario)",
            item.session_id
        )));
    }
    let touch = action
        .touch
        .map(|p| {
            let (x, y) = (touch_coord(p.x), touch_coord(p.y));
            if x >= dust_agent::TOUCH_WIDTH || y >= dust_agent::TOUCH_HEIGHT {
                return Err(Status::invalid_argument(format!(
                    "session {:?}: touch position ({}, {}) is outside the 256x192 screen",
                    item.session_id, p.x, p.y
                )));
            }
            Ok([x, y])
        })
        .transpose()?;
    Ok(BatchJob {
        worker: service.worker(&item.session_id)?,
        step: InputStep {
            keys: keys_from_pb(&action.buttons)?,
            touch,
            // Without a scenario the default frame skip is 1.
            frames: item.frames.max(1),
        },
        session_id: item.session_id,
    })
}

/// Steps one batch item and builds its observation; runs on the session's thread.
fn step_and_observe(
    session: &mut dust_agent::Session,
    step: InputStep,
    spec: &pb::ObservationSpec,
) -> Result<pb::Observation, dust_agent::Error> {
    let status = session.sequence(&[step])?.status;
    let screen = if spec.screen {
        let image = session.observe_screen(spec.grayscale, spec.downscale)?;
        Some(pb::Image {
            width: image.width,
            height: image.height,
            format: image_format_to_pb(image.format) as i32,
            data: image.data,
        })
    } else {
        None
    };
    let ram = spec
        .ram
        .iter()
        .map(|range| session.read(dust_agent::Cpu::Arm9, range.addr, range.len))
        .collect::<Result<_, _>>()?;
    Ok(pb::Observation {
        session_id: String::new(),
        status: Some(status_to_pb(status)),
        screen,
        ram,
        variables: Vec::new(),
        reward: 0.0,
        terminated: false,
        truncated: false,
    })
}

fn optional_path(path: String) -> Option<PathBuf> {
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Implements the listed methods and makes every other RPC return `UNIMPLEMENTED`.
macro_rules! service_impl {
    (
        { $($implemented:tt)* }
        $( $method:ident ( $req:ty ) -> $resp:ty; )*
    ) => {
        #[tonic::async_trait]
        impl DustAgent for AgentService {
            $($implemented)*

            $(
                async fn $method(
                    &self,
                    _request: Request<$req>,
                ) -> Result<Response<$resp>, Status> {
                    Err(Status::unimplemented(concat!(
                        stringify!($method),
                        " is not implemented yet"
                    )))
                }
            )*
        }
    };
}

service_impl! {
    {
        type EventsStream = Pin<Box<dyn Stream<Item = Result<pb::Event, Status>> + Send>>;

        async fn create_session(
            &self,
            request: Request<pb::CreateSessionRequest>,
        ) -> Result<Response<pb::CreateSessionResponse>, Status> {
            let request = request.into_inner();
            if request.rom_path.is_empty() {
                return Err(Status::invalid_argument("rom_path is required"));
            }
            let opts = SessionOptions {
                firmware: optional_path(request.firmware_path),
                arm7_bios: optional_path(request.arm7_bios_path),
                arm9_bios: optional_path(request.arm9_bios_path),
                rtc_unix_time: request.rtc_unix_time,
                save: optional_path(request.save_path),
            };
            let session_id = (self.next_id.fetch_add(1, Ordering::Relaxed) + 1).to_string();
            let worker = Worker::spawn(session_id.clone(), request.rom_path.into(), opts)
                .await
                .map_err(agent_error)?;
            let status = worker
                .call(|session| session.status())
                .await
                .map_err(|CallError::Stopped| Status::internal("session thread stopped"))?;
            self.sessions
                .lock()
                .unwrap()
                .insert(session_id.clone(), worker);
            Ok(Response::new(pb::CreateSessionResponse {
                session_id,
                status: Some(status_to_pb(status)),
            }))
        }

        async fn destroy_session(
            &self,
            request: Request<pb::SessionRequest>,
        ) -> Result<Response<pb::Empty>, Status> {
            let session_id = request.into_inner().session_id;
            self.sessions
                .lock()
                .unwrap()
                .remove(&session_id)
                .ok_or_else(|| Status::not_found(format!("no session with id {session_id:?}")))?;
            Ok(Response::new(pb::Empty {}))
        }

        async fn get_status(
            &self,
            request: Request<pb::SessionRequest>,
        ) -> Result<Response<pb::SessionStatus>, Status> {
            let status = self
                .call(&request.into_inner().session_id, |session| session.status())
                .await?;
            Ok(Response::new(status_to_pb(status)))
        }

        async fn step(
            &self,
            request: Request<pb::StepRequest>,
        ) -> Result<Response<pb::StepResponse>, Status> {
            let request = request.into_inner();
            let frames = request.frames;
            let result = self
                .call(&request.session_id, move |session| session.step(frames))
                .await?
                .map_err(agent_error)?;
            Ok(Response::new(step_response(result)))
        }

        async fn read(
            &self,
            request: Request<pb::ReadRequest>,
        ) -> Result<Response<pb::ReadResponse>, Status> {
            let request = request.into_inner();
            let cpu = cpu_from_pb(request.cpu)?;
            let data = self
                .call(&request.session_id, move |session| {
                    session.read(cpu, request.addr, request.len)
                })
                .await?
                .map_err(agent_error)?;
            Ok(Response::new(pb::ReadResponse { data }))
        }

        async fn read_many(
            &self,
            request: Request<pb::ReadManyRequest>,
        ) -> Result<Response<pb::ReadManyResponse>, Status> {
            let request = request.into_inner();
            let cpu = cpu_from_pb(request.cpu)?;
            let data = self
                .call(&request.session_id, move |session| {
                    request
                        .ranges
                        .iter()
                        .map(|range| session.read(cpu, range.addr, range.len))
                        .collect::<Result<Vec<_>, _>>()
                })
                .await?
                .map_err(agent_error)?;
            Ok(Response::new(pb::ReadManyResponse { data }))
        }

        async fn write(
            &self,
            request: Request<pb::WriteRequest>,
        ) -> Result<Response<pb::Empty>, Status> {
            let request = request.into_inner();
            let cpu = cpu_from_pb(request.cpu)?;
            let value = request
                .value
                .ok_or_else(|| Status::invalid_argument("data or word is required"))?;
            self.call(&request.session_id, move |session| match value {
                pb::write_request::Value::Data(data) => session.write(cpu, request.addr, &data),
                pb::write_request::Value::Word(word) => {
                    session.write_value(cpu, request.addr, word.value, word.size)
                }
            })
            .await?
            .map_err(agent_error)?;
            Ok(Response::new(pb::Empty {}))
        }

        async fn get_regs(
            &self,
            request: Request<pb::RegsRequest>,
        ) -> Result<Response<pb::Regs>, Status> {
            let request = request.into_inner();
            let cpu = cpu_from_pb(request.cpu)?;
            let regs = self
                .call(&request.session_id, move |session| session.regs(cpu))
                .await?;
            Ok(Response::new(pb::Regs {
                gprs: regs.gprs.to_vec(),
                cpsr: regs.cpsr,
            }))
        }

        async fn set_reg(
            &self,
            request: Request<pb::SetRegRequest>,
        ) -> Result<Response<pb::Empty>, Status> {
            let request = request.into_inner();
            let cpu = cpu_from_pb(request.cpu)?;
            self.call(&request.session_id, move |session| {
                session.set_reg(cpu, request.reg, request.value)
            })
            .await?
            .map_err(agent_error)?;
            Ok(Response::new(pb::Empty {}))
        }

        async fn watch(
            &self,
            request: Request<pb::WatchRequest>,
        ) -> Result<Response<pb::WatchResponse>, Status> {
            let request = request.into_inner();
            let cpu = cpu_from_pb(request.cpu)?;
            let watch_id = self
                .call(&request.session_id, move |session| {
                    session.watch(cpu, request.addr, request.len)
                })
                .await?
                .map_err(agent_error)?;
            Ok(Response::new(pb::WatchResponse { watch_id }))
        }

        async fn unwatch(
            &self,
            request: Request<pb::UnwatchRequest>,
        ) -> Result<Response<pb::Empty>, Status> {
            let request = request.into_inner();
            self.call(&request.session_id, move |session| {
                session.unwatch(request.watch_id)
            })
            .await?
            .map_err(agent_error)?;
            Ok(Response::new(pb::Empty {}))
        }

        async fn pause(
            &self,
            request: Request<pb::SessionRequest>,
        ) -> Result<Response<pb::SessionStatus>, Status> {
            let status = self
                .call(&request.into_inner().session_id, |session| {
                    session.pause();
                    session.status()
                })
                .await?;
            Ok(Response::new(status_to_pb(status)))
        }

        async fn resume(
            &self,
            request: Request<pb::SessionRequest>,
        ) -> Result<Response<pb::SessionStatus>, Status> {
            let status = self
                .call(&request.into_inner().session_id, |session| {
                    session.resume();
                    session.status()
                })
                .await?;
            Ok(Response::new(status_to_pb(status)))
        }

        async fn reset(
            &self,
            request: Request<pb::SessionRequest>,
        ) -> Result<Response<pb::SessionStatus>, Status> {
            let status = self
                .call(&request.into_inner().session_id, |session| {
                    session.reset().map(|()| session.status())
                })
                .await?
                .map_err(agent_error)?;
            Ok(Response::new(status_to_pb(status)))
        }

        async fn press(
            &self,
            request: Request<pb::PressRequest>,
        ) -> Result<Response<pb::StepResponse>, Status> {
            let request = request.into_inner();
            let (keys, frames) = (keys_from_pb(&request.buttons)?, request.frames);
            let status = self
                .call(&request.session_id, move |session| session.press(keys, frames))
                .await?
                .map_err(agent_error)?;
            Ok(Response::new(step_response(status)))
        }

        async fn hold(
            &self,
            request: Request<pb::ButtonsRequest>,
        ) -> Result<Response<pb::Empty>, Status> {
            let request = request.into_inner();
            let keys = keys_from_pb(&request.buttons)?;
            self.call(&request.session_id, move |session| session.hold(keys))
                .await?;
            Ok(Response::new(pb::Empty {}))
        }

        async fn release(
            &self,
            request: Request<pb::ButtonsRequest>,
        ) -> Result<Response<pb::Empty>, Status> {
            let request = request.into_inner();
            let keys = keys_from_pb(&request.buttons)?;
            self.call(&request.session_id, move |session| session.release(keys))
                .await?;
            Ok(Response::new(pb::Empty {}))
        }

        async fn touch(
            &self,
            request: Request<pb::TouchRequest>,
        ) -> Result<Response<pb::StepResponse>, Status> {
            let request = request.into_inner();
            let (x, y, frames) = (touch_coord(request.x), touch_coord(request.y), request.frames);
            let status = self
                .call(&request.session_id, move |session| session.touch(x, y, frames))
                .await?
                .map_err(agent_error)?;
            Ok(Response::new(step_response(status)))
        }

        async fn touch_end(
            &self,
            request: Request<pb::SessionRequest>,
        ) -> Result<Response<pb::Empty>, Status> {
            self.call(&request.into_inner().session_id, |session| session.touch_end())
                .await?;
            Ok(Response::new(pb::Empty {}))
        }

        async fn sequence(
            &self,
            request: Request<pb::SequenceRequest>,
        ) -> Result<Response<pb::StepResponse>, Status> {
            let request = request.into_inner();
            let steps = request
                .steps
                .iter()
                .map(|step| {
                    Ok(InputStep {
                        keys: keys_from_pb(&step.buttons)?,
                        touch: step.touch.map(|p| [touch_coord(p.x), touch_coord(p.y)]),
                        frames: step.frames,
                    })
                })
                .collect::<Result<Vec<_>, Status>>()?;
            let status = self
                .call(&request.session_id, move |session| session.sequence(&steps))
                .await?
                .map_err(agent_error)?;
            Ok(Response::new(step_response(status)))
        }

        async fn screenshot(
            &self,
            request: Request<pb::ScreenshotRequest>,
        ) -> Result<Response<pb::Image>, Status> {
            let request = request.into_inner();
            let format = image_format_from_pb(request.format)?;
            let image = self
                .call(&request.session_id, move |session| session.screenshot(format))
                .await?
                .map_err(agent_error)?;
            Ok(Response::new(pb::Image {
                width: image.width,
                height: image.height,
                format: request.format,
                data: image.data,
            }))
        }

        async fn step_batch(
            &self,
            request: Request<pb::StepBatchRequest>,
        ) -> Result<Response<pb::StepBatchResponse>, Status> {
            let request = request.into_inner();
            let spec = std::sync::Arc::new(request.observation.unwrap_or_default());
            if spec.variables {
                return Err(Status::failed_precondition(
                    "observation variables need a loaded scenario (LoadScenario)",
                ));
            }
            if spec.downscale > dust_agent::SCREENSHOT_WIDTH {
                return Err(Status::invalid_argument(format!(
                    "downscale {} is larger than the screen width",
                    spec.downscale
                )));
            }
            let mut seen = std::collections::HashSet::new();
            let jobs = request
                .items
                .into_iter()
                .map(|item| {
                    if !seen.insert(item.session_id.clone()) {
                        return Err(Status::invalid_argument(format!(
                            "session {:?} appears more than once",
                            item.session_id
                        )));
                    }
                    batch_job(self, item)
                })
                .collect::<Result<Vec<_>, _>>()?;

            // Queue every session's step before waiting on any, so they all run at once.
            let pending: Vec<_> = jobs
                .into_iter()
                .map(|job| {
                    let spec = spec.clone();
                    let reply = job
                        .worker
                        .submit(move |session| step_and_observe(session, job.step, &spec));
                    (job.session_id, reply)
                })
                .collect();
            let mut observations = Vec::with_capacity(pending.len());
            let mut first_error = None;
            for (session_id, reply) in pending {
                let result = match reply.await {
                    Ok(Ok(observation)) => Ok(observation),
                    Ok(Err(err)) => Err(agent_error(err)),
                    Err(CallError::Stopped) => {
                        Err(Status::aborted("the session's worker thread stopped"))
                    }
                };
                match result {
                    Ok(mut observation) => {
                        observation.session_id = session_id;
                        observations.push(observation);
                    }
                    Err(status) => {
                        first_error.get_or_insert_with(|| {
                            Status::new(
                                status.code(),
                                format!("session {session_id:?}: {}", status.message()),
                            )
                        });
                    }
                }
            }
            match first_error {
                Some(status) => Err(status),
                None => Ok(Response::new(pb::StepBatchResponse { observations })),
            }
        }

        async fn save_state(
            &self,
            request: Request<pb::StatePathRequest>,
        ) -> Result<Response<pb::Empty>, Status> {
            let request = request.into_inner();
            let (path, include_save) = (required_path(request.path)?, request.include_save);
            self.call(&request.session_id, move |session| {
                session.save_state(path, include_save)
            })
            .await?
            .map_err(agent_error)?;
            Ok(Response::new(pb::Empty {}))
        }

        async fn load_state(
            &self,
            request: Request<pb::StatePathRequest>,
        ) -> Result<Response<pb::SessionStatus>, Status> {
            let request = request.into_inner();
            let path = required_path(request.path)?;
            let status = self
                .call(&request.session_id, move |session| {
                    session.load_state(path).map(|()| session.status())
                })
                .await?
                .map_err(agent_error)?;
            Ok(Response::new(status_to_pb(status)))
        }

        async fn save_state_bytes(
            &self,
            request: Request<pb::SaveStateBytesRequest>,
        ) -> Result<Response<pb::StateBytes>, Status> {
            let request = request.into_inner();
            let include_save = request.include_save;
            let state = self
                .call(&request.session_id, move |session| {
                    session.save_state_bytes(include_save)
                })
                .await?
                .map_err(agent_error)?;
            Ok(Response::new(pb::StateBytes {
                state: state.state,
                save: state.save,
            }))
        }

        async fn load_state_bytes(
            &self,
            request: Request<pb::LoadStateBytesRequest>,
        ) -> Result<Response<pb::SessionStatus>, Status> {
            let request = request.into_inner();
            let (state, save) = (request.state, request.save);
            let status = self
                .call(&request.session_id, move |session| {
                    session
                        .load_state_bytes(&state, save.as_deref())
                        .map(|()| session.status())
                })
                .await?
                .map_err(agent_error)?;
            Ok(Response::new(status_to_pb(status)))
        }

        async fn export_save(
            &self,
            request: Request<pb::SaveFileRequest>,
        ) -> Result<Response<pb::Empty>, Status> {
            let request = request.into_inner();
            let format = save_format_from_pb(request.format)?;
            let path = required_path(request.path)?;
            self.call(&request.session_id, move |session| {
                session.export_save(path, format)
            })
            .await?
            .map_err(agent_error)?;
            Ok(Response::new(pb::Empty {}))
        }

        async fn import_save(
            &self,
            request: Request<pb::SaveFileRequest>,
        ) -> Result<Response<pb::Empty>, Status> {
            let request = request.into_inner();
            save_format_from_pb(request.format)?;
            let path = required_path(request.path)?;
            self.call(&request.session_id, move |session| session.import_save(path))
                .await?
                .map_err(agent_error)?;
            Ok(Response::new(pb::Empty {}))
        }

        async fn set_keypad(
            &self,
            request: Request<pb::KeypadRequest>,
        ) -> Result<Response<pb::Empty>, Status> {
            let request = request.into_inner();
            let mask = request.mask;
            self.call(&request.session_id, move |session| session.set_keypad(mask))
                .await?;
            Ok(Response::new(pb::Empty {}))
        }
    }

    // Breakpoints and watchpoints
    break_add(pb::BreakAddRequest) -> pb::HookId;
    break_remove(pb::HookRequest) -> pb::Empty;
    break_list(pb::SessionRequest) -> pb::HookList;
    watch_write_add(pb::WatchWriteAddRequest) -> pb::HookId;
    watch_write_remove(pb::HookRequest) -> pb::Empty;
    events(pb::SessionRequest) -> Self::EventsStream;
    r#continue(pb::SessionRequest) -> pb::Empty;

    // Observability
    memory_map(pb::SessionRequest) -> pb::MemoryMapResponse;
    dump(pb::DumpRequest) -> pb::ReadResponse;
    disasm(pb::DisasmRequest) -> pb::DisasmResponse;
    stats(pb::StatsRequest) -> pb::EmuStats;
    logs(pb::LogsRequest) -> pb::LogsResponse;

    // Parallel sessions and RL
    load_scenario(pb::LoadScenarioRequest) -> pb::Empty;
    reset_episode(pb::ResetEpisodeRequest) -> pb::Observation;

    // Human handoff, control and settings
    request_help(pb::RequestHelpRequest) -> pb::Empty;
    wait_for_control(pb::SessionRequest) -> pb::ControlReturned;
    set_control(pb::SetControlRequest) -> pb::SessionStatus;
    get_config(pb::SessionRequest) -> pb::Config;
    set_config(pb::SetConfigRequest) -> pb::Config;
}
