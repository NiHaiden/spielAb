use airplay_core::Device;
use gpui::{
    App, Application, Bounds, Context, FocusHandle, KeyDownEvent, SharedString, Window,
    WindowBounds, WindowOptions, div, prelude::*, px, rgb, size,
};
use spielab::{
    backend::{Backend, Command, Event},
    settings::{CaptureMode, LatencyMode, VideoQuality},
};
use std::path::PathBuf;

struct Spielab {
    backend: Backend,
    devices: Vec<Device>,
    status: String,
    receiver: Option<String>,
    pin_required: bool,
    pin: String,
    busy: bool,
    authenticated: bool,
    streaming: bool,
    screen: Option<String>,
    quality: VideoQuality,
    latency: LatencyMode,
    encoder: String,
    capture_mode: CaptureMode,
    virtual_supported: Option<bool>,
    error: bool,
    focus: FocusHandle,
}

impl Spielab {
    fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let events = backend.events.clone();
        cx.spawn(async move |view, cx| {
            while let Ok(event) = events.recv().await {
                if view
                    .update(cx, |view, cx| {
                        view.event(event);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let _ = backend.send(Command::Discover);
        Self {
            backend,
            devices: vec![],
            status: "Looking for AirPlay receivers…".into(),
            receiver: None,
            pin_required: false,
            pin: String::new(),
            busy: true,
            authenticated: false,
            streaming: false,
            screen: None,
            quality: VideoQuality::default(),
            latency: LatencyMode::default(),
            encoder: "Automatic encoder".into(),
            capture_mode: CaptureMode::Mirror,
            virtual_supported: None,
            error: false,
            focus: cx.focus_handle(),
        }
    }

    fn event(&mut self, event: Event) {
        if let Event::EncoderSelected(encoder) = event {
            self.encoder = encoder;
            return;
        }
        if let Event::VirtualSupport(supported) = event {
            self.virtual_supported = Some(supported);
            return;
        }
        self.busy = false;
        self.error = false;
        match event {
            Event::Devices(devices) => {
                self.status = if devices.is_empty() {
                    "No receivers found. Check that Apple TV is awake and on the same network."
                        .into()
                } else {
                    format!("{} AirPlay receiver(s) found", devices.len())
                };
                self.devices = devices;
            }
            Event::Busy(status) => {
                self.status = status;
                self.busy = true;
            }
            Event::PinRequired(name) => {
                self.status = format!("Enter the four-digit code shown on {name}");
                self.receiver = Some(name);
                self.pin_required = true;
                self.pin.clear();
            }
            Event::Authenticated(name) => {
                self.status = format!("Authenticated with {name}");
                self.receiver = Some(name);
                self.authenticated = true;
                self.pin_required = false;
                self.pin.clear();
            }
            Event::VirtualSupport(_) => unreachable!("Handled above"),
            Event::EncoderSelected(_) => unreachable!("Handled above"),
            Event::ScreenSelected { size, mode } => {
                self.capture_mode = mode;
                self.screen = Some(match size {
                    Some((w, h)) => format!("{} · {w} × {h}", mode.label()),
                    None => mode.label().into(),
                });
                self.status = match mode {
                    CaptureMode::Mirror => "Screen selected. Start sharing to send your desktop and system audio.",
                    CaptureMode::Extend => "Extended display created. Move windows onto it, then start sharing. Arrange it in your desktop’s Display Settings.",
                }.into();
            }
            Event::Streaming(frames) => {
                self.streaming = true;
                self.busy = true;
                self.status = format!(
                    "{} · {} · {} · System audio · {frames} frames",
                    self.capture_mode.label(),
                    self.quality.label(),
                    self.encoder,
                );
            }
            Event::Disconnected => self.reset("Disconnected".into()),
            Event::Error(error) => {
                self.reset(error);
                self.error = true;
            }
        }
    }

    fn reset(&mut self, status: String) {
        self.status = status;
        self.receiver = None;
        self.authenticated = false;
        self.streaming = false;
        self.pin_required = false;
        self.pin.clear();
        self.screen = None;
    }

    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        match self.backend.send(command) {
            Ok(()) => {
                self.busy = true;
                self.error = false;
            }
            Err(error) => {
                self.status = error.to_string();
                self.error = true;
            }
        }
        cx.notify();
    }

    fn digit(&mut self, value: &str, cx: &mut Context<Self>) {
        if self.pin_required && !self.busy && self.pin.len() < 4 {
            self.pin.push_str(value);
            cx.notify();
        }
    }

    fn key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if key == "escape" {
            self.backend.disconnect();
        }
        if !self.pin_required || self.busy {
            return;
        }
        match key {
            "backspace" => {
                self.pin.pop();
            }
            "enter" if self.pin.len() == 4 => {
                let pin = std::mem::take(&mut self.pin);
                self.send(Command::Pair(pin), cx);
            }
            value if value.len() == 1 && value.as_bytes()[0].is_ascii_digit() => {
                self.digit(value, cx)
            }
            _ => {}
        }
        cx.notify();
    }
}

fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_4()
        .py_2()
        .rounded_lg()
        .bg(rgb(if enabled { 0x29433c } else { 0x202a2a }))
        .text_color(rgb(if enabled { 0xc6f7d9 } else { 0x75807b }))
        .when(enabled, |d| {
            d.cursor_pointer().hover(|s| s.bg(rgb(0x35584b)))
        })
        .child(label.into())
}

impl Render for Spielab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let idle = !self.busy;
        let quality_controls =
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .children(VideoQuality::ALL.into_iter().enumerate().map(
                            |(index, quality)| {
                                button(("quality", index), quality.label(), idle)
                                    .border_1()
                                    .border_color(rgb(if self.quality == quality {
                                        0x85d8ae
                                    } else {
                                        0x35433e
                                    }))
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        if !view.busy {
                                            view.quality = quality;
                                            cx.notify();
                                        }
                                    }))
                            },
                        )),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(0x9bafa5))
                        .child(self.quality.description()),
                );
        let latency_controls =
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .children(LatencyMode::ALL.into_iter().enumerate().map(
                            |(index, latency)| {
                                button(("latency", index), latency.label(), idle)
                                    .border_1()
                                    .border_color(rgb(if self.latency == latency {
                                        0x85d8ae
                                    } else {
                                        0x35433e
                                    }))
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        if !view.busy {
                                            view.latency = latency;
                                            cx.notify();
                                        }
                                    }))
                            },
                        )),
                )
                .child(
                    div().text_sm().text_color(rgb(0x9bafa5)).child(
                        "Playback buffer, not total delay. Increase it if playback stutters.",
                    ),
                );
        let extend_control = div().flex().flex_col().gap_2()
            .child(button("extend", "Create extended display", idle && self.virtual_supported == Some(true))
                .on_click(cx.listener(|view, _, _, cx| {
                    if !view.busy && view.virtual_supported == Some(true) {
                        view.send(Command::ChooseScreen(CaptureMode::Extend), cx);
                    }
                })))
            .child(div().text_sm().text_color(rgb(0x9bafa5)).child(match self.virtual_supported {
                Some(true) => "Single-click the virtual monitor, then press Share. Move windows there; disconnecting removes it.",
                Some(false) => "Your desktop’s screen-sharing portal does not provide virtual monitors.",
                None => "Checking virtual-monitor support…",
            }));
        let mut receivers = div().flex().flex_col().gap_2();
        for (index, device) in self.devices.iter().enumerate() {
            let device = device.clone();
            let detail = format!(
                "{} · {}",
                device.model,
                device
                    .socket_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_default()
            );
            let name = device.name.clone();
            receivers = receivers.child(
                div()
                    .id(("receiver", index))
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(0x202a2b))
                    .when(idle && !self.authenticated, |d| {
                        d.cursor_pointer()
                            .hover(|s| s.bg(rgb(0x293a37)))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if !view.busy && !view.authenticated {
                                    view.send(Command::Connect(Box::new(device.clone())), cx);
                                }
                            }))
                    })
                    .child(div().text_lg().child(name))
                    .child(div().text_sm().text_color(rgb(0x99ada5)).child(detail)),
            );
        }
        let mut pairing = div().flex().flex_col().gap_3();
        if self.pin_required {
            pairing = pairing.child(div().text_lg().child("Apple TV code")).child(
                div().text_3xl().child(format!(
                    "{}{}",
                    "● ".repeat(self.pin.len()),
                    "○ ".repeat(4 - self.pin.len())
                )),
            );
            for row in [
                vec!["1", "2", "3"],
                vec!["4", "5", "6"],
                vec!["7", "8", "9"],
                vec!["0"],
            ] {
                pairing =
                    pairing.child(div().flex().gap_2().children(row.into_iter().map(|digit| {
                        button(digit, digit, idle)
                            .on_click(cx.listener(move |view, _, _, cx| view.digit(digit, cx)))
                    })));
            }
            pairing = pairing.child(
                div()
                    .flex()
                    .gap_2()
                    .child(button("clear", "Clear", idle).on_click(cx.listener(
                        |view, _, _, cx| {
                            view.pin.clear();
                            cx.notify();
                        },
                    )))
                    .child(
                        button("pair", "Pair Apple TV", idle && self.pin.len() == 4).on_click(
                            cx.listener(|view, _, _, cx| {
                                if !view.busy && view.pin.len() == 4 {
                                    let pin = std::mem::take(&mut view.pin);
                                    view.send(Command::Pair(pin), cx);
                                }
                            }),
                        ),
                    ),
            );
        }
        div()
            .size_full()
            .bg(rgb(0x141c1e))
            .text_color(rgb(0xe6efe9))
            .font_family("sans-serif")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .flex()
            .flex_col()
            .p_8()
            .gap_6()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div().child(div().text_3xl().child("Spielab")).child(
                            div()
                                .text_sm()
                                .text_color(rgb(0x9db3a8))
                                .child("Your desktop, on Apple TV"),
                        ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x85d8ae))
                            .child("WAYLAND · AIRPLAY"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_6()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .w(px(310.))
                            .flex()
                            .flex_col()
                            .gap_4()
                            .child(
                                div()
                                    .flex()
                                    .justify_between()
                                    .items_center()
                                    .child("RECEIVERS")
                                    .child(
                                        button("scan", "Refresh", idle && !self.authenticated)
                                            .on_click(cx.listener(|view, _, _, cx| {
                                                if !view.busy && !view.authenticated {
                                                    view.send(Command::Discover, cx);
                                                }
                                            })),
                                    ),
                            )
                            .child(div().id("receivers").overflow_y_scroll().child(receivers)),
                    )
                    .child(
                        div()
                            .id("sharing-settings")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .gap_5()
                            .p_5()
                            .rounded_xl()
                            .bg(rgb(0x1b2527))
                            .child(
                                div().text_xl().child(
                                    self.receiver
                                        .clone()
                                        .unwrap_or("Connect to your Apple TV".into()),
                                ),
                            )
                            .child(
                                div()
                                    .text_color(rgb(0xa7b8b0))
                                    .child(if self.authenticated {
                                        "Pairing verified over an encrypted connection."
                                    } else {
                                        "Select a receiver on your local network to begin."
                                    }),
                            )
                            .child(pairing)
                            .child(
                                div()
                                    .border_t_1()
                                    .border_color(rgb(0x35433e))
                                    .pt_4()
                                    .flex()
                                    .flex_col()
                                    .gap_3()
                                    .child("DISPLAY & QUALITY")
                                    .child(quality_controls)
                                    .child(latency_controls)
                                    .child(
                                        self.screen.clone().unwrap_or("No screen selected".into()),
                                    )
                                    .child(
                                        button("screen", "Choose screen or window", idle).on_click(
                                            cx.listener(|view, _, _, cx| {
                                                if !view.busy {
                                                    view.send(Command::ChooseScreen(CaptureMode::Mirror), cx);
                                                }
                                            }),
                                        ),
                                    )
                                    .child(extend_control)
                                    .child(
                                        button(
                                            "start-mirroring",
                                            if self.streaming {
                                                "Sharing…"
                                            } else {
                                                "Start sharing"
                                            },
                                            idle && self.authenticated && self.screen.is_some(),
                                        )
                                        .on_click(
                                            cx.listener(|view, _, _, cx| {
                                                if !view.busy
                                                    && view.authenticated
                                                    && view.screen.is_some()
                                                {
                                                    view.send(Command::StartMirroring(view.quality, view.latency), cx);
                                                }
                                            }),
                                        ),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(0x9bafa5))
                                            .child("System audio included · Settings apply when sharing starts"),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(if self.error { 0x442b2b } else { 0x202e29 }))
                    .child(self.status.clone()),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8d9f96))
                            .child("Esc cancels · Your desktop controls screen-sharing permission"),
                    )
                    .child(
                        button("disconnect", "Disconnect / cancel", true)
                            .on_click(cx.listener(|view, _, _, _| view.backend.disconnect())),
                    ),
            )
    }
}

fn main() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        "Spielab requires a Wayland session (WAYLAND_DISPLAY is unset)"
    );
    let state_directory = std::env::var_os("SPIELAB_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_STATE_HOME").map(|p| PathBuf::from(p).join("spielab")))
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state/spielab")))
        .ok_or_else(|| anyhow::anyhow!("Set SPIELAB_STATE_DIR or XDG_STATE_HOME"))?;
    let backend = Backend::start(state_directory)?;
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(980.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("dev.spielab.Spielab".into()),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| Spielab::new(backend, cx));
                view.read(cx).focus.focus(window);
                view
            },
        )
        .expect("Failed to open native Wayland window");
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
    Ok(())
}
