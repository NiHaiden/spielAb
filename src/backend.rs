//! AirPlay session worker. Network operations never run on the GPUI thread.
use crate::{
    capture::ScreenCapture,
    media::{AudioCapture, VideoCapture, VideoEncoder},
    mirror::MirrorSession,
    pairing::{ControlSession, CredentialStore},
    settings::{CaptureMode, LatencyMode, VideoQuality},
};
use airplay_core::Device;
use airplay_discovery::{Discovery, ServiceBrowser};
use anyhow::{Context, Result};
use async_channel::{Receiver, Sender};
use std::{path::PathBuf, time::Duration};
use tokio::sync::watch;

pub enum Command {
    Discover,
    Connect(Box<Device>),
    Pair(String),
    ChooseScreen(CaptureMode),
    StartMirroring(VideoQuality, LatencyMode),
}

pub enum Event {
    Devices(Vec<Device>),
    Busy(String),
    PinRequired(String),
    Authenticated(String),
    ScreenSelected {
        size: Option<(i32, i32)>,
        mode: CaptureMode,
    },
    VirtualSupport(bool),
    EncoderSelected(String),
    Streaming(u64),
    Disconnected,
    Error(String),
}

pub struct Backend {
    commands: Sender<Command>,
    cancel: watch::Sender<u64>,
    pub events: Receiver<Event>,
}

impl Backend {
    pub fn start(state_directory: PathBuf) -> Result<Self> {
        let store = CredentialStore::new(state_directory)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let (commands, receiver) = async_channel::bounded(8);
        let (events, event_receiver) = async_channel::unbounded();
        let (cancel, cancel_receiver) = watch::channel(0);
        std::thread::Builder::new()
            .name("airplay-worker".into())
            .spawn(move || {
                runtime.block_on(run(receiver, events, cancel_receiver, store));
            })?;
        Ok(Self {
            commands,
            cancel,
            events: event_receiver,
        })
    }

    pub fn send(&self, command: Command) -> Result<()> {
        self.commands
            .try_send(command)
            .map_err(|_| anyhow::anyhow!("AirPlay worker is busy or has stopped"))
    }

    pub fn disconnect(&self) {
        self.cancel
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.commands.close();
        self.disconnect();
    }
}

struct Worker {
    session: Option<ControlSession>,
    screen: Option<ScreenCapture>,
    authenticated: bool,
    store: CredentialStore,
    events: Sender<Event>,
}

impl Worker {
    fn emit(&self, event: Event) {
        let _ = self.events.try_send(event);
    }

    async fn reset(&mut self) {
        self.authenticated = false;
        if let Some(screen) = self.screen.take() {
            let _ = screen.close().await;
        }
        if let Some(mut session) = self.session.take() {
            let _ = session.stop_mirroring().await;
            let _ = session.transport.close().await;
        }
    }

    async fn execute(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Discover => {
                self.emit(Event::Busy("Looking for AirPlay receivers…".into()));
                self.emit(Event::VirtualSupport(
                    ScreenCapture::supports_virtual().await.unwrap_or(false),
                ));
                let browser = ServiceBrowser::new()?;
                let mut devices = browser.scan(Duration::from_secs(5)).await?;
                devices.sort_by(|a, b| a.name.cmp(&b.name));
                self.emit(Event::Devices(devices));
            }
            Command::Connect(device) => {
                self.reset().await;
                self.emit(Event::Busy(format!("Connecting to {}…", device.name)));
                let credentials = self.store.load(&device)?;
                let mut session = ControlSession::open(*device).await?;
                if let Some(credentials) = credentials {
                    match session.verify(&credentials).await {
                        Ok(()) => {
                            self.authenticated = true;
                            self.emit(Event::Authenticated(session.device.name.clone()));
                        }
                        Err(_) => {
                            // Failed encrypted exchanges cannot safely reuse the transport.
                            let device = session.device.clone();
                            session.transport.close().await?;
                            session = ControlSession::open(device).await?;
                            session.request_pin().await?;
                            self.emit(Event::PinRequired(session.device.name.clone()));
                        }
                    }
                } else {
                    session.request_pin().await?;
                    self.emit(Event::PinRequired(session.device.name.clone()));
                }
                self.session = Some(session);
            }
            Command::Pair(pin) => {
                self.emit(Event::Busy("Verifying Apple TV code…".into()));
                let session = self.session.as_mut().context("Select a receiver first")?;
                let credentials = session.pair(&pin).await?;
                self.store.save(&session.device, &credentials)?;
                let name = session.device.name.clone();
                self.authenticated = true;
                self.emit(Event::Authenticated(name));
            }
            Command::ChooseScreen(mode) => {
                self.emit(Event::Busy(
                    match mode {
                        CaptureMode::Mirror => "Single-click one screen or window, then press Share…",
                        CaptureMode::Extend => {
                            "Single-click the virtual monitor, then press Share (avoid double-clicking)…"
                        }
                    }
                    .into(),
                ));
                if let Some(screen) = self.screen.take() {
                    screen.close().await?;
                }
                let screen = ScreenCapture::select_mode(mode).await?;
                self.emit(Event::ScreenSelected {
                    size: screen.size,
                    mode: screen.mode,
                });
                self.screen = Some(screen);
            }
            Command::StartMirroring(quality, latency) => {
                anyhow::ensure!(self.authenticated, "Connect to Apple TV first");
                let screen = self.screen.as_ref().context("Choose a screen first")?;
                self.emit(Event::Busy(
                    "Starting desktop and system audio sharing…".into(),
                ));
                let config = quality.config();
                let encoder = VideoEncoder::detect(config).await;
                self.emit(Event::EncoderSelected(encoder.label().into()));
                let session = self.session.as_mut().context("Connect to Apple TV first")?;
                let mut mirror = MirrorSession::setup_with_latency(
                    session,
                    config.width,
                    config.height,
                    latency,
                )
                .await?;
                // Start capture only once the receiver is ready, so setup does
                // not accumulate stale video or audio in the process pipes.
                let video = VideoCapture::start_with_encoder(Some(screen), config, &encoder)?;
                let audio = AudioCapture::start()?;
                let events = self.events.clone();
                mirror
                    .run(session, video, audio, move |frames| {
                        let _ = events.try_send(Event::Streaming(frames));
                    })
                    .await?;
            }
        }
        Ok(())
    }
}

async fn run(
    commands: Receiver<Command>,
    events: Sender<Event>,
    mut cancel: watch::Receiver<u64>,
    store: CredentialStore,
) {
    let mut worker = Worker {
        session: None,
        screen: None,
        authenticated: false,
        store,
        events,
    };
    loop {
        let command = tokio::select! {
            biased;
            changed = cancel.changed() => {
                worker.reset().await;
                while commands.try_recv().is_ok() {}
                worker.emit(Event::Disconnected);
                if changed.is_err() || commands.is_closed() { break; }
                continue;
            }
            command = commands.recv() => match command { Ok(command) => command, Err(_) => break },
        };
        let result = tokio::select! {
            biased;
            _ = cancel.changed() => None,
            result = worker.execute(command) => Some(result),
        };
        match result {
            Some(Ok(())) => {}
            Some(Err(error)) => {
                worker.reset().await;
                worker.emit(Event::Error(format!("{error:#}")));
            }
            None => {
                worker.reset().await;
                while commands.try_recv().is_ok() {}
                worker.emit(Event::Disconnected);
            }
        }
    }
    worker.reset().await;
}
