use airplay_core::Device;
use gpui::{
    Animation, AnimationExt, App, Application, Bounds, Context, FocusHandle, KeyDownEvent,
    PathBuilder, SharedString, Window, WindowBounds, WindowOptions, canvas, div, point, prelude::*,
    px, rgb, size,
};
use spielab::{
    backend::{Backend, Command, Event},
    settings::{CaptureMode, LatencyMode, VideoQuality},
};
use std::{f32::consts::TAU, path::PathBuf, time::Duration};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Connection,
    Sharing,
    Settings,
}

#[derive(Clone, Copy)]
struct Theme {
    background: u32,
    panel: u32,
    text: u32,
    muted: u32,
    border: u32,
    button: u32,
    hover: u32,
    accent: u32,
    danger: u32,
}
impl Theme {
    fn new(dark: bool) -> Self {
        if dark {
            Self {
                background: 0x101817,
                panel: 0x1a2522,
                text: 0xedf5ef,
                muted: 0xa1b5a9,
                border: 0x34473e,
                button: 0x283d32,
                hover: 0x365340,
                accent: 0x9ee4b1,
                danger: 0xa73443,
            }
        } else {
            Self {
                background: 0xf3f6f3,
                panel: 0xffffff,
                text: 0x192c20,
                muted: 0x526859,
                border: 0xcdd9d0,
                button: 0xe2eee5,
                hover: 0xd1e5d6,
                accent: 0x206838,
                danger: 0xb3273a,
            }
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Preferences {
    dark_mode: bool,
}

struct Spielab {
    backend: Backend,
    page: Page,
    dark_mode: bool,
    preferences_path: PathBuf,
    devices: Vec<Device>,
    status: String,
    receiver: Option<String>,
    pin_required: bool,
    pin: String,
    busy: bool,
    discovering: bool,
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
    fn new(backend: Backend, preferences_path: PathBuf, cx: &mut Context<Self>) -> Self {
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
        let dark_mode = std::fs::read(&preferences_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Preferences>(&bytes).ok())
            .map(|preferences| preferences.dark_mode)
            .unwrap_or(true);
        let _ = backend.send(Command::Discover);
        Self {
            backend,
            page: Page::Connection,
            dark_mode,
            preferences_path,
            devices: vec![],
            status: "Looking for AirPlay receivers…".into(),
            receiver: None,
            pin_required: false,
            pin: String::new(),
            busy: true,
            discovering: true,
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
        if !matches!(event, Event::Busy(_)) {
            self.discovering = false;
        }
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
                self.page = Page::Sharing;
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
            Event::Streaming(_) => {
                self.streaming = true;
                self.busy = true;
                self.status = format!(
                    "Sharing to {} · System audio included",
                    self.receiver.as_deref().unwrap_or("Apple TV")
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
        self.discovering = false;
        self.pin_required = false;
        self.pin.clear();
        self.screen = None;
        if self.page == Page::Sharing {
            self.page = Page::Connection;
        }
    }

    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        let discovering = matches!(command, Command::Discover);
        match self.backend.send(command) {
            Ok(()) => {
                self.busy = true;
                self.discovering = discovering;
                self.error = false;
                if discovering {
                    self.status = "Looking for AirPlay receivers…".into();
                }
            }
            Err(error) => {
                self.discovering = false;
                self.status = error.to_string();
                self.error = true;
            }
        }
        cx.notify();
    }

    fn toggle_dark_mode(&mut self, cx: &mut Context<Self>) {
        self.dark_mode = !self.dark_mode;
        let preferences = Preferences {
            dark_mode: self.dark_mode,
        };
        let result = (|| -> anyhow::Result<()> {
            let temporary = self.preferences_path.with_extension("tmp");
            std::fs::write(&temporary, serde_json::to_vec(&preferences)?)?;
            std::fs::rename(temporary, &self.preferences_path)?;
            Ok(())
        })();
        if let Err(error) = result {
            self.status = format!("Could not save appearance: {error}");
            self.error = true;
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
        if !self.pin_required || self.busy || self.page != Page::Connection {
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
    theme: Theme,
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_4()
        .py_3()
        .rounded_lg()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.button))
        .text_color(rgb(if enabled { theme.text } else { theme.muted }))
        .when(enabled, |d| {
            d.cursor_pointer().hover(move |s| s.bg(rgb(theme.hover)))
        })
        .when(!enabled, |d| d.opacity(0.55))
        .child(label.into())
}
fn card(theme: Theme, title: &str, description: &str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_4()
        .p_6()
        .rounded_xl()
        .border_1()
        .border_color(rgb(theme.border))
        .bg(rgb(theme.panel))
        .child(div().text_xl().child(title.to_owned()))
        .child(
            div()
                .text_sm()
                .text_color(rgb(theme.muted))
                .child(description.to_owned()),
        )
}

fn discovery_spinner(theme: Theme) -> impl IntoElement {
    // GPUI schedules frames only while this element is mounted during discovery.
    div().size(px(18.)).flex_shrink_0().with_animation(
        "discovery-spinner",
        Animation::new(Duration::from_millis(900)).repeat(),
        move |element, progress| {
            element.child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        let center = bounds.center();
                        let radius = bounds.size.width / 2. - px(2.);
                        let at = |angle: f32| {
                            point(
                                center.x + radius * angle.cos(),
                                center.y + radius * angle.sin(),
                            )
                        };
                        let radii = point(radius, radius);
                        let mut track = PathBuilder::stroke(px(2.));
                        track.move_to(at(0.));
                        track.arc_to(radii, px(0.), false, true, at(TAU / 2.));
                        track.arc_to(radii, px(0.), false, true, at(0.));
                        track.close();
                        if let Ok(path) = track.build() {
                            window.paint_path(path, rgb(theme.border));
                        }
                        let start = progress * TAU;
                        let mut arc = PathBuilder::stroke(px(2.));
                        arc.move_to(at(start));
                        arc.arc_to(radii, px(0.), true, true, at(start + TAU * 0.75));
                        if let Ok(path) = arc.build() {
                            window.paint_path(path, rgb(theme.accent));
                        }
                    },
                )
                .size_full(),
            )
        },
    )
}

impl Spielab {
    fn connection_page(&self, theme: Theme, cx: &mut Context<Self>) -> gpui::Div {
        let idle = !self.busy;
        if self.authenticated {
            return card(
                theme,
                "You're connected",
                self.receiver.as_deref().unwrap_or("Apple TV"),
            )
            .child(
                button(theme, "continue-sharing", "Continue to sharing →", true).on_click(
                    cx.listener(|view, _, _, cx| {
                        view.page = Page::Sharing;
                        cx.notify();
                    }),
                ),
            );
        }
        if self.pin_required {
            let mut pairing = card(
                theme,
                "Authorize Apple TV",
                "Enter the four-digit code shown on your TV. You can use your keyboard.",
            )
            .child(div().text_3xl().child(format!(
                "{}{}",
                "● ".repeat(self.pin.len()),
                "○ ".repeat(4 - self.pin.len())
            )));
            for (index, row) in [
                ["1", "2", "3"],
                ["4", "5", "6"],
                ["7", "8", "9"],
                ["Clear", "0", "⌫"],
            ]
            .into_iter()
            .enumerate()
            {
                pairing = pairing.child(div().flex().gap_2().children(
                    row.into_iter().enumerate().map(|(column, digit)| {
                        button(theme, ("pin", index * 3 + column), digit, idle)
                            .w(px(80.))
                            .text_center()
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if view.busy {
                                    return;
                                }
                                match digit {
                                    "Clear" => view.pin.clear(),
                                    "⌫" => {
                                        view.pin.pop();
                                    }
                                    value => view.digit(value, cx),
                                }
                                cx.notify();
                            }))
                    }),
                ));
            }
            return pairing.child(
                button(
                    theme,
                    "authorize",
                    "Authorize and continue →",
                    idle && self.pin.len() == 4,
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    if !view.busy && view.pin.len() == 4 {
                        let pin = std::mem::take(&mut view.pin);
                        view.send(Command::Pair(pin), cx);
                    }
                })),
            );
        }
        let mut content = card(
            theme,
            "Connect your Apple TV",
            "Choose a TV to authorize. Sharing options appear once you're connected.",
        )
        .child(
            button(
                theme,
                "refresh",
                if self.discovering {
                    "Looking for TVs…"
                } else {
                    "Refresh devices"
                },
                idle,
            )
            .flex()
            .items_center()
            .gap_2()
            .when(self.discovering, |button| {
                button.opacity(1.).child(discovery_spinner(theme))
            })
            .on_click(cx.listener(|view, _, _, cx| {
                if !view.busy {
                    view.send(Command::Discover, cx);
                }
            })),
        );
        if self.devices.is_empty() {
            content = content.child(
                div()
                    .py_6()
                    .text_color(rgb(theme.muted))
                    .child("Keep Apple TV awake and connected to the same network."),
            );
        }
        for (index, device) in self.devices.iter().enumerate() {
            let device = device.clone();
            content = content.child(
                button(theme, ("device", index), device.name.clone(), idle)
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(div().text_sm().child("Connect →"))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if !view.busy && !view.authenticated {
                            view.receiver = Some(device.name.clone());
                            view.send(Command::Connect(Box::new(device.clone())), cx);
                        }
                    })),
            );
        }
        content
    }

    fn sharing_page(&self, theme: Theme, cx: &mut Context<Self>) -> gpui::Div {
        if !self.authenticated {
            return self.connection_page(theme, cx);
        }
        if self.streaming {
            return card(
                theme,
                "You're sharing",
                self.receiver.as_deref().unwrap_or("Apple TV"),
            )
            .child(
                div()
                    .text_lg()
                    .child(self.screen.clone().unwrap_or_default()),
            )
            .child(
                div()
                    .text_color(rgb(theme.muted))
                    .child(format!("{} · System audio on", self.quality.label())),
            )
            .child(
                div()
                    .text_sm()
                    .child("Use Disconnect at the top right to stop sharing."),
            );
        }
        let idle = !self.busy;
        let source = card(theme, "1. Choose what to share", "Single-click one source in the desktop dialog, then press Share.")
            .child(div().flex().flex_wrap().gap_3()
                .child(button(theme, "mirror", "Screen or window", idle)
                    .on_click(cx.listener(|view, _, _, cx| {
                        if view.authenticated && !view.busy { view.send(Command::ChooseScreen(CaptureMode::Mirror), cx); }
                    })))
                .child(button(theme, "extend", "Extended display", idle && self.virtual_supported == Some(true))
                    .on_click(cx.listener(|view, _, _, cx| {
                        if view.authenticated && !view.busy && view.virtual_supported == Some(true) { view.send(Command::ChooseScreen(CaptureMode::Extend), cx); }
                    }))))
            .child(div().text_sm().text_color(rgb(theme.muted)).child(match self.virtual_supported {
                Some(true) => "Extended display adds a separate desktop for windows you move onto your TV.",
                Some(false) => "Extended display isn't supported by this desktop.",
                None => "Checking extended-display support…",
            }))
            .when(self.screen.is_some(), |d| d.child(div().text_color(rgb(theme.accent)).child(format!("Selected: {}", self.screen.as_deref().unwrap_or_default()))));
        let quality = card(
            theme,
            "2. Tune your stream",
            "Choose picture quality and responsiveness before sharing.",
        )
        .child(div().text_sm().child("Video quality"))
        .child(
            div().flex().flex_wrap().gap_2().children(
                VideoQuality::ALL
                    .into_iter()
                    .enumerate()
                    .map(|(index, quality)| {
                        button(theme, ("quality", index), quality.label(), idle)
                            .when(self.quality == quality, |d| {
                                d.border_color(rgb(theme.accent))
                                    .text_color(rgb(theme.accent))
                            })
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if !view.busy {
                                    view.quality = quality;
                                    cx.notify();
                                }
                            }))
                    }),
            ),
        )
        .child(
            div()
                .text_sm()
                .text_color(rgb(theme.muted))
                .child(self.quality.description()),
        )
        .child(div().text_sm().child("Playback buffer"))
        .child(
            div().flex().flex_wrap().gap_2().children(
                LatencyMode::ALL
                    .into_iter()
                    .enumerate()
                    .map(|(index, latency)| {
                        button(theme, ("latency", index), latency.label(), idle)
                            .when(self.latency == latency, |d| {
                                d.border_color(rgb(theme.accent))
                                    .text_color(rgb(theme.accent))
                            })
                            .on_click(cx.listener(move |view, _, _, cx| {
                                if !view.busy {
                                    view.latency = latency;
                                    cx.notify();
                                }
                            }))
                    }),
            ),
        )
        .child(
            div()
                .text_sm()
                .text_color(rgb(theme.muted))
                .child("Lower feels faster. Increase the buffer if playback stutters."),
        );
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(source)
            .child(quality)
            .child(
                button(
                    theme,
                    "start",
                    if self.busy {
                        "Preparing sharing…"
                    } else {
                        "Start sharing →"
                    },
                    idle && self.screen.is_some(),
                )
                .text_center()
                .text_lg()
                .border_color(rgb(theme.accent))
                .on_click(cx.listener(|view, _, _, cx| {
                    if view.authenticated && !view.busy && view.screen.is_some() {
                        view.send(Command::StartMirroring(view.quality, view.latency), cx);
                    }
                })),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(theme.muted))
                    .child("System audio is included automatically."),
            )
    }

    fn settings_page(&self, theme: Theme, cx: &mut Context<Self>) -> gpui::Div {
        card(
            theme,
            "Appearance",
            "Make Spielab comfortable for your workspace.",
        )
        .child(
            div()
                .flex()
                .justify_between()
                .items_center()
                .gap_4()
                .child(
                    div().child("Dark mode").child(
                        div()
                            .text_sm()
                            .text_color(rgb(theme.muted))
                            .child("Saved for your next visit."),
                    ),
                )
                .child(
                    div()
                        .id("dark-mode")
                        .focusable()
                        .cursor_pointer()
                        .flex_shrink_0()
                        .w(px(52.))
                        .h(px(30.))
                        .p(px(2.))
                        .rounded_full()
                        .border_2()
                        .border_color(rgb(if self.dark_mode {
                            theme.accent
                        } else {
                            theme.border
                        }))
                        .bg(rgb(if self.dark_mode {
                            theme.accent
                        } else {
                            theme.button
                        }))
                        .focus(move |style| style.border_color(rgb(theme.text)))
                        .hover(|style| style.opacity(0.85))
                        .flex()
                        .items_center()
                        .when(self.dark_mode, |switch| switch.justify_end())
                        .child(
                            div()
                                .size(px(22.))
                                .rounded_full()
                                .bg(rgb(if self.dark_mode {
                                    theme.background
                                } else {
                                    theme.panel
                                }))
                                .shadow_sm(),
                        )
                        .on_click(cx.listener(|view, _, _, cx| view.toggle_dark_mode(cx)))
                        .on_key_down(cx.listener(|view, event: &KeyDownEvent, _, cx| {
                            if matches!(event.keystroke.key.as_str(), "space" | "enter") {
                                view.toggle_dark_mode(cx);
                                cx.stop_propagation();
                            }
                        })),
                ),
        )
    }
}

impl Render for Spielab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::new(self.dark_mode);
        let can_stop = self.busy || self.authenticated || self.pin_required;
        let stop_theme = Theme {
            button: theme.danger,
            hover: 0x8f2433,
            text: 0xffffff,
            ..theme
        };
        let mut navigation = div().w(px(180.)).flex_shrink_0().flex().flex_col().gap_2();
        for (index, page, label) in [
            (0, Page::Connection, "Connection"),
            (1, Page::Sharing, "Sharing"),
            (2, Page::Settings, "Settings"),
        ] {
            let enabled = page != Page::Sharing || self.authenticated;
            navigation = navigation.child(
                button(theme, ("nav", index as usize), label, enabled)
                    .when(self.page == page, |d| {
                        d.border_color(rgb(theme.accent))
                            .text_color(rgb(theme.accent))
                    })
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if page != Page::Sharing || view.authenticated {
                            view.page = page;
                            cx.notify();
                        }
                    })),
            );
        }
        let content = match self.page {
            Page::Connection => self.connection_page(theme, cx),
            Page::Sharing => self.sharing_page(theme, cx),
            Page::Settings => self.settings_page(theme, cx),
        };
        div()
            .size_full()
            .bg(rgb(theme.background))
            .text_color(rgb(theme.text))
            .font_family("sans-serif")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key))
            .flex()
            .flex_col()
            .p_6()
            .gap_5()
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .justify_between()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_3xl().child("Spielab"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(if self.error {
                                        theme.danger
                                    } else {
                                        theme.muted
                                    }))
                                    .child(self.status.clone()),
                            ),
                    )
                    .child(
                        button(
                            stop_theme,
                            "disconnect",
                            if self.authenticated {
                                "Disconnect"
                            } else {
                                "Cancel"
                            },
                            can_stop,
                        )
                        .flex_shrink_0()
                        .bg(rgb(theme.danger))
                        .text_color(rgb(0xffffff))
                        .border_color(rgb(theme.danger))
                        .on_click(cx.listener(|view, _, _, cx| {
                            if view.busy || view.authenticated || view.pin_required {
                                view.backend.disconnect();
                                view.status = "Disconnecting…".into();
                                view.busy = true;
                                cx.notify();
                            }
                        })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap_5()
                    .child(navigation)
                    .child(
                        div()
                            .id(match self.page {
                                Page::Connection => "connection-page",
                                Page::Sharing => "sharing-page",
                                Page::Settings => "settings-page",
                            })
                            .flex_1()
                            .min_w_0()
                            .overflow_y_scroll()
                            .child(content),
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
    let preferences_path = state_directory.join("preferences.json");
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
                let view = cx.new(|cx| Spielab::new(backend, preferences_path, cx));
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
