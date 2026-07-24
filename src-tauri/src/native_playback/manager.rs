use super::{
    BridgeError, BridgeErrorCode, CommandResult, EngineEvent, EngineMedia, NativeDiagnosticsEvent,
    NativePlaybackState, NativeStateEvent, NativeVideoSurface, PageInstanceId,
    PlaybackCapabilities, PlaybackCommand, PlaybackCommandRequest, PlaybackDiagnostics,
    PlaybackEngine, PlaybackEngineFactory, RendererLifecycle, RendererSessionId, StateUpdateKind,
    TerminationReason, NATIVE_PLAYBACK_PROTOCOL_VERSION,
};
use std::{
    collections::HashMap,
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(25);
const POSITION_PUBLICATION_INTERVAL: Duration = Duration::from_millis(250);
const DIAGNOSTICS_PUBLICATION_INTERVAL: Duration = Duration::from_secs(1);
const WORKER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebPlaybackOwner {
    pub origin: String,
    pub page_instance_id: PageInstanceId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaybackOwner {
    Web(WebPlaybackOwner),
    #[cfg(debug_assertions)]
    NativeDevelopmentHarness,
}

impl PlaybackOwner {
    fn matches_web(&self, owner: &WebPlaybackOwner) -> bool {
        matches!(self, Self::Web(active) if active == owner)
    }
}

pub trait PlaybackEventSink: Send + Sync + 'static {
    fn state(&self, owner: &PlaybackOwner, event: &NativeStateEvent);
    fn diagnostics(&self, owner: &PlaybackOwner, event: &NativeDiagnosticsEvent);
}

#[derive(Default)]
pub struct LogPlaybackEventSink;

impl PlaybackEventSink for LogPlaybackEventSink {
    fn state(&self, _owner: &PlaybackOwner, event: &NativeStateEvent) {
        log::debug!(
            "native state renderer={} revision={} position={:.3}",
            event.renderer_session_id.as_str(),
            event.state_revision,
            event.payload.current_time
        );
    }

    fn diagnostics(&self, _owner: &PlaybackOwner, event: &NativeDiagnosticsEvent) {
        log::debug!(
            "native diagnostics renderer={} revision={}",
            event.renderer_session_id.as_str(),
            event.diagnostics_revision
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub renderer_session_id: RendererSessionId,
    pub owner: PlaybackOwner,
    pub lifecycle: RendererLifecycle,
    pub state_revision: u64,
    pub diagnostics_revision: u64,
    pub next_command_sequence: u64,
    pub requested_termination: Option<TerminationReason>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartedPlayback {
    pub renderer_session_id: RendererSessionId,
    pub video_surface: NativeVideoSurface,
}

struct ActiveSession {
    snapshot: SessionSnapshot,
    commands: Sender<WorkerRequest>,
    command_gate: Arc<Mutex<()>>,
    command_results: HashMap<super::CommandId, CommandResult>,
    latest_state: NativePlaybackState,
}

#[derive(Default)]
struct ManagerState {
    active: Option<ActiveSession>,
}

struct Shared {
    state: Mutex<ManagerState>,
    lifecycle_gate: Mutex<()>,
    sink: Arc<dyn PlaybackEventSink>,
}

#[derive(Clone)]
pub struct NativePlaybackManager {
    shared: Arc<Shared>,
    factory: Arc<dyn PlaybackEngineFactory>,
}

enum WorkerRequest {
    Command {
        command: PlaybackCommand,
        response: Sender<Result<(), super::EngineError>>,
    },
    Terminate {
        reason: TerminationReason,
        response: Sender<Result<(), super::EngineError>>,
    },
}

impl NativePlaybackManager {
    pub fn new(factory: Arc<dyn PlaybackEngineFactory>, sink: Arc<dyn PlaybackEventSink>) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(ManagerState::default()),
                lifecycle_gate: Mutex::new(()),
                sink,
            }),
            factory,
        }
    }

    pub fn capabilities(&self) -> PlaybackCapabilities {
        self.factory.capabilities()
    }

    pub fn snapshot(&self) -> Option<SessionSnapshot> {
        self.shared
            .state
            .lock()
            .ok()
            .and_then(|state| state.active.as_ref().map(|active| active.snapshot.clone()))
    }

    pub fn start(
        &self,
        owner: PlaybackOwner,
        media: EngineMedia,
    ) -> Result<StartedPlayback, BridgeError> {
        // Bridge requests run on Tauri's blocking pool and may therefore
        // overlap. Serialize renderer replacement with disposal so a page
        // disappearing during MPV initialization cannot leave an orphaned
        // renderer behind or race a second load.
        let _lifecycle = self.lock_lifecycle()?;
        // Replacement is explicit and synchronous: a new engine is never
        // created while the previous engine might still produce output.
        self.dispose_active_locked(TerminationReason::Disposed)?;

        let engine = self
            .factory
            .create(media)
            .map_err(|error| BridgeError::new(error.code, error.message))?;
        let video_surface = engine.video_surface();
        let renderer_session_id = RendererSessionId::generate();
        let (commands, requests) = mpsc::channel();
        let command_gate = Arc::new(Mutex::new(()));
        let state = NativePlaybackState {
            loading: true,
            paused: true,
            volume: 1.0,
            ..NativePlaybackState::default()
        };

        {
            let mut manager = self.shared.state.lock().map_err(|_| {
                BridgeError::new(
                    BridgeErrorCode::InternalError,
                    "native playback state is unavailable",
                )
            })?;
            manager.active = Some(ActiveSession {
                snapshot: SessionSnapshot {
                    renderer_session_id: renderer_session_id.clone(),
                    owner: owner.clone(),
                    lifecycle: RendererLifecycle::Loading,
                    state_revision: 0,
                    diagnostics_revision: 0,
                    next_command_sequence: 1,
                    requested_termination: None,
                },
                commands,
                command_gate,
                command_results: HashMap::new(),
                latest_state: state.clone(),
            });
        }

        publish_state(&self.shared, &renderer_session_id, state);

        let shared = self.shared.clone();
        let worker_session_id = renderer_session_id.clone();
        let spawn_result = thread::Builder::new()
            .name(format!(
                "heya-renderer-{}",
                &renderer_session_id.as_str()[..8]
            ))
            .spawn(move || renderer_worker(shared, worker_session_id, engine, requests));

        if let Err(error) = spawn_result {
            clear_if_current(&self.shared, &renderer_session_id);
            return Err(BridgeError::new(
                BridgeErrorCode::BackendUnavailable,
                format!("could not start the native renderer worker: {error}"),
            ));
        }

        Ok(StartedPlayback {
            renderer_session_id,
            video_surface,
        })
    }

    pub fn send_command(
        &self,
        owner: &WebPlaybackOwner,
        request: PlaybackCommandRequest,
    ) -> Result<CommandResult, BridgeError> {
        request.command.validate()?;

        let command_gate = {
            let state = self.lock_state()?;
            let active = state.active.as_ref().ok_or_else(unknown_session_error)?;
            ensure_session(active, owner, &request.renderer_session_id)?;
            active.command_gate.clone()
        };
        let _serial = command_gate.lock().map_err(|_| internal_lock_error())?;

        let (sender, sequence) = {
            let mut state = self.lock_state()?;
            let active = state.active.as_mut().ok_or_else(unknown_session_error)?;
            ensure_session(active, owner, &request.renderer_session_id)?;

            if active.snapshot.lifecycle == RendererLifecycle::Stopping {
                return Err(BridgeError::new(
                    BridgeErrorCode::RendererStopping,
                    "the native renderer is stopping",
                ));
            }
            if let Some(result) = active.command_results.get(&request.command_id) {
                let mut duplicate = result.clone();
                duplicate.duplicate = true;
                return Ok(duplicate);
            }

            let sequence = active.snapshot.next_command_sequence;
            active.snapshot.next_command_sequence = sequence.saturating_add(1);
            (active.commands.clone(), sequence)
        };

        let (response_tx, response_rx) = mpsc::channel();
        sender
            .send(WorkerRequest::Command {
                command: request.command,
                response: response_tx,
            })
            .map_err(|_| unknown_session_error())?;
        let engine_result = response_rx
            .recv_timeout(WORKER_RESPONSE_TIMEOUT)
            .map_err(|_| {
                BridgeError::new(
                    BridgeErrorCode::CommandFailed,
                    "the native renderer did not acknowledge the command",
                )
            })?;

        let result = CommandResult {
            renderer_session_id: request.renderer_session_id.clone(),
            command_id: request.command_id.clone(),
            command_sequence: sequence,
            accepted: engine_result.is_ok(),
            duplicate: false,
            error: engine_result.err().map(|error| super::PlaybackError {
                code: error.code,
                message: error.message,
            }),
        };

        if let Ok(mut state) = self.shared.state.lock() {
            if let Some(active) = state.active.as_mut() {
                if active.snapshot.renderer_session_id == request.renderer_session_id
                    && active.snapshot.owner.matches_web(owner)
                {
                    active
                        .command_results
                        .insert(request.command_id, result.clone());
                }
            }
        }
        Ok(result)
    }

    pub fn dispose_owned(
        &self,
        owner: &WebPlaybackOwner,
        renderer_session_id: Option<&RendererSessionId>,
        reason: TerminationReason,
    ) -> Result<(), BridgeError> {
        let _lifecycle = self.lock_lifecycle()?;
        let current = self.snapshot();
        let Some(current) = current else {
            return Ok(());
        };
        if !current.owner.matches_web(owner)
            || renderer_session_id.is_some_and(|id| id != &current.renderer_session_id)
        {
            return Err(unknown_session_error());
        }
        self.dispose_session(&current.renderer_session_id, reason)
    }

    pub fn dispose_active(&self, reason: TerminationReason) -> Result<(), BridgeError> {
        let _lifecycle = self.lock_lifecycle()?;
        self.dispose_active_locked(reason)
    }

    fn dispose_active_locked(&self, reason: TerminationReason) -> Result<(), BridgeError> {
        let Some(snapshot) = self.snapshot() else {
            return Ok(());
        };
        self.dispose_session(&snapshot.renderer_session_id, reason)
    }

    /// Native-only development control used by the MPV platform spike. This is
    /// not registered with the WebView bridge.
    #[cfg(debug_assertions)]
    pub fn send_development_command(&self, command: PlaybackCommand) -> Result<(), BridgeError> {
        command.validate()?;
        let command_gate = {
            let state = self.lock_state()?;
            let active = state.active.as_ref().ok_or_else(unknown_session_error)?;
            if active.snapshot.owner != PlaybackOwner::NativeDevelopmentHarness {
                return Err(unknown_session_error());
            }
            active.command_gate.clone()
        };
        let _serial = command_gate.lock().map_err(|_| internal_lock_error())?;
        let sender = {
            let state = self.lock_state()?;
            let active = state.active.as_ref().ok_or_else(unknown_session_error)?;
            if active.snapshot.owner != PlaybackOwner::NativeDevelopmentHarness {
                return Err(unknown_session_error());
            }
            active.commands.clone()
        };
        let (response_tx, response_rx) = mpsc::channel();
        sender
            .send(WorkerRequest::Command {
                command,
                response: response_tx,
            })
            .map_err(|_| unknown_session_error())?;
        response_rx
            .recv_timeout(WORKER_RESPONSE_TIMEOUT)
            .map_err(|_| {
                BridgeError::new(
                    BridgeErrorCode::CommandFailed,
                    "the native renderer did not acknowledge the development command",
                )
            })?
            .map_err(|error| BridgeError::new(error.code, error.message))
    }

    fn dispose_session(
        &self,
        renderer_session_id: &RendererSessionId,
        reason: TerminationReason,
    ) -> Result<(), BridgeError> {
        let command_gate = {
            let state = self.lock_state()?;
            let Some(active) = state.active.as_ref() else {
                return Ok(());
            };
            if &active.snapshot.renderer_session_id != renderer_session_id {
                return Err(unknown_session_error());
            }
            active.command_gate.clone()
        };
        let _serial = command_gate.lock().map_err(|_| internal_lock_error())?;

        let sender = {
            let mut state = self.lock_state()?;
            let Some(active) = state.active.as_mut() else {
                return Ok(());
            };
            if &active.snapshot.renderer_session_id != renderer_session_id {
                return Err(unknown_session_error());
            }
            active.snapshot.lifecycle = RendererLifecycle::Stopping;
            active.snapshot.requested_termination = Some(reason);
            active.commands.clone()
        };

        let (response_tx, response_rx) = mpsc::channel();
        if sender
            .send(WorkerRequest::Terminate {
                reason,
                response: response_tx,
            })
            .is_err()
        {
            clear_if_current(&self.shared, renderer_session_id);
            return Ok(());
        }

        response_rx
            .recv_timeout(WORKER_RESPONSE_TIMEOUT)
            .map_err(|_| {
                BridgeError::new(
                    BridgeErrorCode::CommandFailed,
                    "the native renderer did not stop in time",
                )
            })?
            .map_err(|error| BridgeError::new(error.code, error.message))
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, ManagerState>, BridgeError> {
        self.shared.state.lock().map_err(|_| internal_lock_error())
    }

    fn lock_lifecycle(&self) -> Result<std::sync::MutexGuard<'_, ()>, BridgeError> {
        self.shared
            .lifecycle_gate
            .lock()
            .map_err(|_| internal_lock_error())
    }
}

fn renderer_worker(
    shared: Arc<Shared>,
    renderer_session_id: RendererSessionId,
    mut engine: Box<dyn PlaybackEngine>,
    requests: Receiver<WorkerRequest>,
) {
    let mut last_position_publication = Instant::now() - POSITION_PUBLICATION_INTERVAL;
    let mut last_diagnostics_publication = Instant::now() - DIAGNOSTICS_PUBLICATION_INTERVAL;

    loop {
        // Wait on Heya's command channel, rather than inside mpv_wait_event.
        // A newly queued play/pause command now wakes this worker immediately;
        // MPV events are still drained at least every poll interval below.
        match requests.recv_timeout(WORKER_POLL_INTERVAL) {
            Ok(WorkerRequest::Command { command, response }) => {
                if command == PlaybackCommand::Stop {
                    let result = engine.stop(TerminationReason::Stopped);
                    let _ = response.send(result.clone());
                    finish_session(&shared, &renderer_session_id, TerminationReason::Stopped);
                    return;
                }
                let _ = response.send(engine.command(&command));
            }
            Ok(WorkerRequest::Terminate { reason, response }) => {
                let result = engine.stop(reason);
                let termination = if result.is_ok() {
                    reason
                } else {
                    TerminationReason::NativeCrashed
                };
                finish_session(&shared, &renderer_session_id, termination);
                let _ = response.send(result);
                return;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = engine.stop(TerminationReason::Disposed);
                finish_session(&shared, &renderer_session_id, TerminationReason::Disposed);
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        // One MPV callback can cover several events. Drain the queue without
        // blocking so state confirmation and diagnostics never delay the next
        // semantic command.
        loop {
            match engine.poll_event(Duration::ZERO) {
                Ok(Some(EngineEvent::State { state, kind })) => {
                    let publish = kind == StateUpdateKind::Structural
                        || last_position_publication.elapsed() >= POSITION_PUBLICATION_INTERVAL;
                    if publish {
                        publish_state(&shared, &renderer_session_id, state);
                        if kind == StateUpdateKind::Position {
                            last_position_publication = Instant::now();
                        }
                    } else {
                        remember_state(&shared, &renderer_session_id, state);
                    }
                }
                Ok(Some(EngineEvent::Diagnostics {
                    diagnostics,
                    structural,
                })) => {
                    if structural
                        || last_diagnostics_publication.elapsed()
                            >= DIAGNOSTICS_PUBLICATION_INTERVAL
                    {
                        publish_diagnostics(&shared, &renderer_session_id, *diagnostics);
                        last_diagnostics_publication = Instant::now();
                    }
                }
                Ok(Some(EngineEvent::Terminated(reason))) => {
                    finish_session(&shared, &renderer_session_id, reason);
                    return;
                }
                Ok(None) => break,
                Err(error) => {
                    log::error!(
                        "native renderer {} failed: {}",
                        renderer_session_id.as_str(),
                        error
                    );
                    let _ = engine.stop(TerminationReason::NativeCrashed);
                    finish_session(
                        &shared,
                        &renderer_session_id,
                        TerminationReason::NativeCrashed,
                    );
                    return;
                }
            }
        }
    }
}

fn ensure_session(
    active: &ActiveSession,
    owner: &WebPlaybackOwner,
    renderer_session_id: &RendererSessionId,
) -> Result<(), BridgeError> {
    if &active.snapshot.renderer_session_id != renderer_session_id
        || !active.snapshot.owner.matches_web(owner)
    {
        return Err(unknown_session_error());
    }
    Ok(())
}

fn remember_state(
    shared: &Shared,
    renderer_session_id: &RendererSessionId,
    state: NativePlaybackState,
) {
    if let Ok(mut manager) = shared.state.lock() {
        if let Some(active) = manager.active.as_mut() {
            if &active.snapshot.renderer_session_id == renderer_session_id {
                active.latest_state = state;
            }
        }
    }
}

fn publish_state(
    shared: &Shared,
    renderer_session_id: &RendererSessionId,
    state: NativePlaybackState,
) {
    let event = {
        let Ok(mut manager) = shared.state.lock() else {
            return;
        };
        let Some(active) = manager.active.as_mut() else {
            return;
        };
        if &active.snapshot.renderer_session_id != renderer_session_id {
            return;
        }
        active.latest_state = state.clone();
        active.snapshot.state_revision = active.snapshot.state_revision.saturating_add(1);
        if state.loading {
            active.snapshot.lifecycle = RendererLifecycle::Loading;
        } else if active.snapshot.lifecycle != RendererLifecycle::Stopping {
            active.snapshot.lifecycle = RendererLifecycle::Active;
        }
        (
            active.snapshot.owner.clone(),
            NativeStateEvent {
                protocol_version: NATIVE_PLAYBACK_PROTOCOL_VERSION,
                renderer_session_id: renderer_session_id.clone(),
                state_revision: active.snapshot.state_revision,
                payload: state,
            },
        )
    };
    shared.sink.state(&event.0, &event.1);
}

fn publish_diagnostics(
    shared: &Shared,
    renderer_session_id: &RendererSessionId,
    diagnostics: Option<PlaybackDiagnostics>,
) {
    let event = {
        let Ok(mut manager) = shared.state.lock() else {
            return;
        };
        let Some(active) = manager.active.as_mut() else {
            return;
        };
        if &active.snapshot.renderer_session_id != renderer_session_id {
            return;
        }
        active.snapshot.diagnostics_revision =
            active.snapshot.diagnostics_revision.saturating_add(1);
        (
            active.snapshot.owner.clone(),
            NativeDiagnosticsEvent {
                protocol_version: NATIVE_PLAYBACK_PROTOCOL_VERSION,
                renderer_session_id: renderer_session_id.clone(),
                diagnostics_revision: active.snapshot.diagnostics_revision,
                payload: diagnostics,
            },
        )
    };
    shared.sink.diagnostics(&event.0, &event.1);
}

fn finish_session(
    shared: &Shared,
    renderer_session_id: &RendererSessionId,
    reason: TerminationReason,
) {
    let event = {
        let Ok(mut manager) = shared.state.lock() else {
            return;
        };
        let Some(active) = manager.active.as_mut() else {
            return;
        };
        if &active.snapshot.renderer_session_id != renderer_session_id {
            return;
        }

        let mut state = active.latest_state.clone();
        state.playing = false;
        state.paused = true;
        state.loading = false;
        state.buffering = false;
        state.ended = reason == TerminationReason::Ended;
        // The renderer worker returns right after this, dropping the engine and
        // with it the embedded surface. Reporting the surface as still ready
        // would leave the web layer transparent over nothing at all — on macOS
        // that shows the desktop through the whole window (most visibly at the
        // end of an episode, where the Up Next card is all that is left).
        state.video_surface_ready = false;
        state.termination_reason = Some(reason);
        active.snapshot.state_revision = active.snapshot.state_revision.saturating_add(1);
        let event = (
            active.snapshot.owner.clone(),
            NativeStateEvent {
                protocol_version: NATIVE_PLAYBACK_PROTOCOL_VERSION,
                renderer_session_id: renderer_session_id.clone(),
                state_revision: active.snapshot.state_revision,
                payload: state,
            },
        );
        manager.active.take();
        event
    };
    shared.sink.state(&event.0, &event.1);
}

fn clear_if_current(shared: &Shared, renderer_session_id: &RendererSessionId) {
    if let Ok(mut manager) = shared.state.lock() {
        if manager
            .active
            .as_ref()
            .is_some_and(|active| &active.snapshot.renderer_session_id == renderer_session_id)
        {
            manager.active.take();
        }
    }
}

fn unknown_session_error() -> BridgeError {
    BridgeError::new(
        BridgeErrorCode::UnknownSession,
        "the native renderer session is no longer active",
    )
}

fn internal_lock_error() -> BridgeError {
    BridgeError::new(
        BridgeErrorCode::InternalError,
        "native playback state is unavailable",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_playback::{
        CommandId, EngineError, NormalizedTrackId, PlaybackDiagnostics, PlaybackError,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct RecordingSink {
        state: Mutex<Vec<NativeStateEvent>>,
        diagnostics: Mutex<Vec<NativeDiagnosticsEvent>>,
    }

    impl PlaybackEventSink for RecordingSink {
        fn state(&self, _owner: &PlaybackOwner, event: &NativeStateEvent) {
            self.state.lock().unwrap().push(event.clone());
        }

        fn diagnostics(&self, _owner: &PlaybackOwner, event: &NativeDiagnosticsEvent) {
            self.diagnostics.lock().unwrap().push(event.clone());
        }
    }

    #[derive(Default)]
    struct FakeShared {
        commands: Mutex<Vec<PlaybackCommand>>,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        next_events: Mutex<Vec<EngineEvent>>,
        stop_reasons: Mutex<Vec<TerminationReason>>,
    }

    struct FakeEngine {
        shared: Arc<FakeShared>,
    }

    impl PlaybackEngine for FakeEngine {
        fn video_surface(&self) -> NativeVideoSurface {
            NativeVideoSurface::NativeWindow
        }

        fn command(&mut self, command: &PlaybackCommand) -> Result<(), EngineError> {
            let in_flight = self.shared.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.shared
                .max_in_flight
                .fetch_max(in_flight, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(5));
            self.shared.commands.lock().unwrap().push(command.clone());
            self.shared.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }

        fn poll_event(&mut self, _timeout: Duration) -> Result<Option<EngineEvent>, EngineError> {
            thread::sleep(Duration::from_millis(1));
            Ok(self.shared.next_events.lock().unwrap().pop())
        }

        fn stop(&mut self, reason: TerminationReason) -> Result<(), EngineError> {
            self.shared.stop_reasons.lock().unwrap().push(reason);
            Ok(())
        }
    }

    struct FakeFactory {
        shared: Arc<FakeShared>,
    }

    impl PlaybackEngineFactory for FakeFactory {
        fn capabilities(&self) -> PlaybackCapabilities {
            PlaybackCapabilities::mpv(true, NativeVideoSurface::NativeWindow, None)
        }

        fn create(&self, _media: EngineMedia) -> Result<Box<dyn PlaybackEngine>, EngineError> {
            Ok(Box::new(FakeEngine {
                shared: self.shared.clone(),
            }))
        }
    }

    fn harness() -> (
        NativePlaybackManager,
        Arc<FakeShared>,
        Arc<RecordingSink>,
        WebPlaybackOwner,
    ) {
        let engine = Arc::new(FakeShared::default());
        let sink = Arc::new(RecordingSink::default());
        let manager = NativePlaybackManager::new(
            Arc::new(FakeFactory {
                shared: engine.clone(),
            }),
            sink.clone(),
        );
        let owner = WebPlaybackOwner {
            origin: "https://heya.example.com".to_string(),
            page_instance_id: PageInstanceId::parse("f362493e-a39f-4b21-bc8d-24f909d439ef")
                .unwrap(),
        };
        (manager, engine, sink, owner)
    }

    fn command(
        session: &RendererSessionId,
        id: &str,
        command: PlaybackCommand,
    ) -> PlaybackCommandRequest {
        PlaybackCommandRequest {
            renderer_session_id: session.clone(),
            command_id: CommandId::parse(id).unwrap(),
            command,
        }
    }

    #[test]
    fn commands_are_serialized_and_duplicate_ids_are_not_reexecuted() {
        let (manager, engine, _sink, owner) = harness();
        let session = manager
            .start(PlaybackOwner::Web(owner.clone()), EngineMedia::Synthetic)
            .unwrap()
            .renderer_session_id;

        let threads: Vec<_> = (0..8)
            .map(|index| {
                let manager = manager.clone();
                let owner = owner.clone();
                let session = session.clone();
                thread::spawn(move || {
                    manager
                        .send_command(
                            &owner,
                            command(
                                &session,
                                &format!("parallel-{index}"),
                                PlaybackCommand::Pause,
                            ),
                        )
                        .unwrap()
                })
            })
            .collect();
        for thread in threads {
            assert!(thread.join().unwrap().accepted);
        }

        let first = manager
            .send_command(
                &owner,
                command(&session, "same-command", PlaybackCommand::Play),
            )
            .unwrap();
        let duplicate = manager
            .send_command(
                &owner,
                command(&session, "same-command", PlaybackCommand::Play),
            )
            .unwrap();

        assert!(first.accepted);
        assert!(!first.duplicate);
        assert!(duplicate.duplicate);
        assert_eq!(engine.commands.lock().unwrap().len(), 9);
        assert_eq!(engine.max_in_flight.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn old_session_commands_and_events_are_rejected_after_replacement() {
        let (manager, engine, sink, owner) = harness();
        let old = manager
            .start(PlaybackOwner::Web(owner.clone()), EngineMedia::Synthetic)
            .unwrap()
            .renderer_session_id;
        let new = manager
            .start(PlaybackOwner::Web(owner.clone()), EngineMedia::Synthetic)
            .unwrap()
            .renderer_session_id;

        assert_ne!(old, new);
        assert_eq!(
            manager
                .send_command(&owner, command(&old, "late", PlaybackCommand::Pause))
                .unwrap_err()
                .code,
            BridgeErrorCode::UnknownSession
        );
        publish_state(&manager.shared, &old, NativePlaybackState::default());
        assert!(
            sink.state
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.renderer_session_id == old)
                .count()
                <= 2
        );
        assert_eq!(
            engine.stop_reasons.lock().unwrap().first(),
            Some(&TerminationReason::Disposed)
        );
    }

    #[test]
    fn state_and_diagnostics_revisions_are_independent_and_monotonic() {
        let (manager, _engine, sink, owner) = harness();
        let session = manager
            .start(PlaybackOwner::Web(owner), EngineMedia::Synthetic)
            .unwrap()
            .renderer_session_id;
        publish_state(&manager.shared, &session, NativePlaybackState::default());
        publish_diagnostics(
            &manager.shared,
            &session,
            Some(PlaybackDiagnostics::default()),
        );
        publish_state(&manager.shared, &session, NativePlaybackState::default());

        let state_revisions: Vec<_> = sink
            .state
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.renderer_session_id == session)
            .map(|event| event.state_revision)
            .collect();
        assert_eq!(state_revisions, vec![1, 2, 3]);
        assert_eq!(sink.diagnostics.lock().unwrap()[0].diagnostics_revision, 1);
    }

    #[test]
    fn only_eof_is_marked_ended() {
        for (reason, expected_ended) in [
            (TerminationReason::Ended, true),
            (TerminationReason::Stopped, false),
            (TerminationReason::WindowClosed, false),
            (TerminationReason::Disposed, false),
            (TerminationReason::Failed, false),
            (TerminationReason::NativeCrashed, false),
            (TerminationReason::LoggedOut, false),
            (TerminationReason::ServerSwitched, false),
            (TerminationReason::AppQuit, false),
        ] {
            let (manager, _engine, sink, owner) = harness();
            let session = manager
                .start(PlaybackOwner::Web(owner), EngineMedia::Synthetic)
                .unwrap()
                .renderer_session_id;
            finish_session(&manager.shared, &session, reason);
            let event = sink.state.lock().unwrap().last().unwrap().clone();
            assert_eq!(event.payload.ended, expected_ended, "{reason:?}");
            assert_eq!(event.payload.termination_reason, Some(reason));
        }
    }

    #[test]
    fn termination_reports_the_embedded_surface_as_gone() {
        let (manager, _engine, sink, owner) = harness();
        let session = manager
            .start(PlaybackOwner::Web(owner), EngineMedia::Synthetic)
            .unwrap()
            .renderer_session_id;
        publish_state(
            &manager.shared,
            &session,
            NativePlaybackState {
                video_surface_ready: true,
                ..NativePlaybackState::default()
            },
        );
        finish_session(&manager.shared, &session, TerminationReason::Ended);

        let event = sink.state.lock().unwrap().last().unwrap().clone();
        assert!(!event.payload.video_surface_ready);
    }

    #[test]
    fn server_switch_and_app_quit_dispose_with_exact_reasons() {
        for reason in [
            TerminationReason::ServerSwitched,
            TerminationReason::AppQuit,
        ] {
            let (manager, engine, _sink, owner) = harness();
            manager
                .start(PlaybackOwner::Web(owner), EngineMedia::Synthetic)
                .unwrap();
            manager.dispose_active(reason).unwrap();
            assert_eq!(engine.stop_reasons.lock().unwrap().as_slice(), &[reason]);
            assert!(manager.snapshot().is_none());
        }
    }

    #[test]
    fn invalid_track_ids_are_rejected_during_deserialization() {
        let parsed = serde_json::from_str::<PlaybackCommandRequest>(
            r#"{
                "rendererSessionId":"session",
                "commandId":"audio-1",
                "type":"selectAudioTrack",
                "trackId":"../../secret"
            }"#,
        );
        assert!(parsed.is_err());
        let _ = NormalizedTrackId::parse("audio:1").unwrap();
    }

    // Keep imported response types exercised so schema regressions are caught
    // when command errors are changed.
    #[test]
    fn command_error_payload_is_typed() {
        let error = PlaybackError {
            code: BridgeErrorCode::CommandFailed,
            message: "failed".to_string(),
        };
        assert_eq!(error.code, BridgeErrorCode::CommandFailed);
    }
}
