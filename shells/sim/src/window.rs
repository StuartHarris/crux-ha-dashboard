//! The window of the live simulator, made with egui.
//!
//! The window uses `eframe` with the OpenGL renderer (`glow`). `eframe` uses
//! `winit`, which loads X11 or Wayland at run time, so a build on Linux needs
//! no system packages.
//!
//! The panel is in the centre. The controls are on the left, the view model
//! is on the right, and the effect log and the MQTT log are at the bottom.
//! The panel shows at an integer scale of screen pixels, so each panel pixel
//! is a square of screen pixels. egui works in points, and on a Retina
//! display a point is 2 screen pixels.
//!
//! The shell owns the core on the UI thread. The tasks on the tokio runtime
//! send their signals through a channel and ask egui to repaint, so the
//! window repaints only when something changes.

use std::{
    sync::{Arc, mpsc},
    time::Instant,
};

use eframe::egui::{
    self, CentralPanel, Color32, ColorImage, Context, Key, Label, Modifiers, Panel, Pos2, Rect,
    RichText, ScrollArea, Sense, Stroke, StrokeKind, TextEdit, TextStyle, TextureHandle,
    TextureOptions, Ui, Vec2, ViewportCommand,
};
use embedded_graphics::{prelude::*, primitives::Rectangle};
use shared::{Event, PowerSource, ViewModel, fake::FakeRadio};

use crate::{
    inspector::{Direction, Inspector, Log, ROUTINE_EFFECTS},
    panel::{Scale, Viewport, share},
    screen::Screen,
    settings::Options,
    shell::{Notifier, Shell, Signal, reading},
    stream,
};

/// The window title.
const TITLE: &str = "Dashboard simulator";
/// The change of the charge of the battery for each press of `[` or `]`,
/// in percent.
const LEVEL_STEP: u8 = 10;
/// The colour of the outline of a partial refresh.
const OUTLINE_COLOR: Color32 = Color32::RED;
/// The format of the time of a log entry.
const TIME: &str = "%H:%M:%S%.3f";
/// The colour of the warning that taps switch real devices.
const LIVE_COLOR: Color32 = Color32::from_rgb(0xD0, 0x20, 0x20);
/// The width, in points, of the border round the panel while the shell is
/// live.
const LIVE_BORDER: f32 = 4.0;
/// The colour around the panel.
const BACKGROUND: Color32 = Color32::from_gray(0x30);
/// The warm colour of the front light.
const LIGHT_COLOR: (u8, u8, u8) = (0xFF, 0xC0, 0x60);
/// The opacity of the tint of the front light at 100%, from 0 to 255.
const LIGHT_MAX_ALPHA: u32 = 140;

/// Opens the window, starts the core and runs until the user quits.
///
/// # Errors
///
/// Returns a message if the window or the runtime cannot start.
pub fn run(options: Options) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    let handle = runtime.handle().clone();
    let Options {
        settings,
        power,
        level,
        path,
        ..
    } = options;
    if settings.read_only {
        eprintln!("read-only: a click does not change devices in the house");
    } else {
        eprintln!(
            "warning: a click switches real devices in the house. Give --read-only to prevent this."
        );
    }
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([1600.0, 1000.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        TITLE,
        native,
        Box::new(move |creation| {
            // A task asks egui to repaint after it sends a signal, so the
            // window does not repaint all the time.
            let context = creation.egui_ctx.clone();
            let (sender, signals) = mpsc::channel();
            let notifier = Notifier::new(sender, Arc::new(move || context.request_repaint()));
            let mut shell = Shell::new(handle, notifier, stream::tcp_opener());
            shell.start(settings, power, path);
            shell.send(Event::BatteryChanged(Some(level)));
            shell.send(Event::PowerRead(reading(power)));
            Ok(Box::new(App::new(shell, signals, power, level)))
        }),
    );
    // eframe dropped the app and its shell. The tasks stopped with no
    // DISCONNECT packet, so the broker publishes the last will, as for a
    // device that loses power.
    runtime.shutdown_background();
    result.map_err(|error| error.to_string())
}

/// The state of the window.
struct App {
    shell: Shell,
    signals: mpsc::Receiver<Signal>,
    screen: Screen,
    power: PowerSource,
    /// The charge of the battery, in percent.
    level: u8,
    scale: Scale,
    show_outline: bool,
    texture: Option<TextureHandle>,
    /// The image version and the flash state of the texture.
    uploaded: Option<(u64, bool)>,
    /// The level of the front light in the last frame, in percent.
    light: u8,
    /// The guard on the switch from a dry run to live service calls.
    live: LiveGuard,
    view_text: ViewText,
    hide_routine: bool,
    effect_filter: String,
    mqtt_filter: String,
}

/// The guard on the switch from a dry run to live service calls. The user
/// must ask and then confirm, so one click cannot switch real devices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LiveGuard {
    asking: bool,
}

impl LiveGuard {
    /// The user asked to go live. The window shows a confirm button.
    const fn ask(&mut self) {
        self.asking = true;
    }

    /// The user cancelled the request.
    const fn cancel(&mut self) {
        self.asking = false;
    }

    /// The user confirmed. Returns `true` if the user asked first, and
    /// the shell must then go live.
    const fn confirm(&mut self) -> bool {
        std::mem::replace(&mut self.asking, false)
    }
}

/// The pretty `Debug` text of the view model. It is made again only when the
/// view model changes.
#[derive(Default)]
struct ViewText {
    view: Option<ViewModel>,
    text: String,
    /// The number of times that the text was made.
    made: u64,
}

impl ViewText {
    /// Returns the text of `view`.
    fn text(&mut self, view: &ViewModel) -> &str {
        if self.view.as_ref() != Some(view) {
            self.text = format!("{view:#?}");
            self.view = Some(view.clone());
            self.made += 1;
        }
        &self.text
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &Context, _: &mut eframe::Frame) {
        while let Ok(signal) = self.signals.try_recv() {
            self.shell.handle(signal);
        }
        self.refresh(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _: &mut eframe::Frame) {
        // `logic` also runs while the window is hidden, with the input of the
        // last frame, so the keys are read here.
        self.keys(ui.ctx());
        let view = self.shell.view();
        Panel::left("controls")
            .resizable(true)
            .default_size(260.0)
            .show(ui, |ui| {
                ScrollArea::vertical().show(ui, |ui| self.controls(ui, &view));
            });
        Panel::right("inspector")
            .resizable(true)
            .default_size(320.0)
            .show(ui, |ui| self.view_model(ui, &view));
        Panel::bottom("logs")
            .resizable(true)
            .default_size(280.0)
            .show(ui, |ui| {
                ui.columns(2, |columns| {
                    self.effect_log(&mut columns[0]);
                    self.mqtt_log(&mut columns[1]);
                });
            });
        CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(BACKGROUND))
            .show(ui, |ui| self.panel(ui, &view));
        // A control can make the core ask for a render. Draw it now and
        // paint it in the next frame.
        self.refresh(ui.ctx());
    }
}

impl App {
    fn new(shell: Shell, signals: mpsc::Receiver<Signal>, power: PowerSource, level: u8) -> Self {
        Self {
            shell,
            signals,
            screen: Screen::new(),
            power,
            level,
            scale: Scale::Fit,
            show_outline: false,
            texture: None,
            uploaded: None,
            light: 0,
            live: LiveGuard::default(),
            view_text: ViewText::default(),
            hide_routine: true,
            effect_filter: String::new(),
            mqtt_filter: String::new(),
        }
    }

    /// Draws the view if the core asked for a render, and asks for a repaint
    /// at the end of a flash. Also asks for a repaint if the level of the
    /// front light changed, because a tap can change the light with no
    /// render.
    fn refresh(&mut self, ctx: &Context) {
        let now = Instant::now();
        if self.shell.take_render() {
            self.screen.show(&self.shell.view(), now);
            ctx.request_repaint();
        }
        let light = self.shell.light();
        if light != self.light {
            self.light = light;
            ctx.request_repaint();
        }
        let flash = self.screen.flash();
        if flash.is_on(now)
            && let Some(remaining) = flash.remaining(now)
        {
            ctx.request_repaint_after(remaining);
        }
    }

    /// The keys work when no text box has the focus. The window takes each
    /// key that it uses, so egui does not also act on it.
    fn keys(&mut self, ctx: &Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let pressed = |key| ctx.input_mut(|input| input.consume_key(Modifiers::NONE, key));
        if pressed(Key::Q) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if pressed(Key::S) {
            let power = match self.power {
                PowerSource::Stand => PowerSource::Battery,
                PowerSource::Battery => PowerSource::Stand,
            };
            self.set_power(power);
        }
        if pressed(Key::OpenBracket) {
            self.set_level(self.level.saturating_sub(LEVEL_STEP));
        }
        if pressed(Key::CloseBracket) {
            self.set_level(self.level.saturating_add(LEVEL_STEP));
        }
        if pressed(Key::W) {
            self.shell.wake();
        }
        if pressed(Key::Z) {
            self.shell.sleep_now();
        }
        if pressed(Key::R) {
            self.shell.restart();
        }
        if pressed(Key::L) {
            self.shell.drop_link();
        }
        if pressed(Key::B) {
            self.shell.toggle_blocked();
        }
        if pressed(Key::A) {
            self.shell.move_strongest();
        }
        if pressed(Key::K) {
            self.shell.toggle_stuck();
        }
        if pressed(Key::D) {
            self.show_outline = !self.show_outline;
        }
        let previous = pressed(Key::ArrowLeft);
        let next = pressed(Key::ArrowRight);
        if previous {
            self.shell.send(Event::PreviousPage);
        }
        if next {
            self.shell.send(Event::NextPage);
        }
        if previous || next {
            // egui can also move the focus with an arrow key, so remove the
            // focus from the controls.
            ctx.memory_mut(|memory| {
                if let Some(id) = memory.focused() {
                    memory.surrender_focus(id);
                }
            });
        }
    }

    fn set_power(&mut self, power: PowerSource) {
        if power != self.power {
            self.power = power;
            eprintln!("power: {power:?}");
            self.shell.set_power_source(power);
        }
    }

    /// Sets the charge of the battery to `level` percent, at most 100, and
    /// sends the change to the core.
    fn set_level(&mut self, level: u8) {
        let level = level.min(100);
        if level != self.level {
            self.level = level;
            eprintln!("power: battery {level}%");
            self.shell.send(Event::BatteryChanged(Some(level)));
        }
    }

    /// Uploads the image to the texture if the image or the flash changed.
    fn texture(&mut self, ctx: &Context) -> egui::TextureId {
        let flash = self.screen.flash().is_on(Instant::now());
        let key = (self.screen.version(), flash);
        if self.uploaded != Some(key) || self.texture.is_none() {
            let size = [ui::WIDTH as usize, ui::HEIGHT as usize];
            let image = ColorImage::from_gray_iter(size, self.screen.levels(flash));
            match &mut self.texture {
                Some(texture) => texture.set(image, TextureOptions::NEAREST),
                None => {
                    self.texture = Some(ctx.load_texture("panel", image, TextureOptions::NEAREST));
                }
            }
            self.uploaded = Some(key);
        }
        self.texture
            .as_ref()
            .map_or_else(egui::TextureId::default, TextureHandle::id)
    }

    /// Shows the panel at an integer scale in a scroll area. A click is a
    /// touch. While the shell is live, a red border is round the panel.
    /// While the front light is on, a warm tint is over the panel.
    fn panel(&mut self, ui: &mut Ui, view: &ViewModel) {
        let texture = self.texture(ui.ctx());
        let pixels_per_point = ui.ctx().pixels_per_point();
        let area = ui.available_rect_before_wrap();
        let to_pixels = |points: f32| {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the window is smaller than u32::MAX pixels"
            )]
            let pixels = (points * pixels_per_point).floor().max(0.0) as u32;
            pixels
        };
        let viewport = Viewport::place(
            self.scale,
            to_pixels(area.width()),
            to_pixels(area.height()),
        );
        #[allow(
            clippy::cast_precision_loss,
            reason = "the values are much smaller than 2^24"
        )]
        let to_points = |pixels: u32| pixels as f32 / pixels_per_point;
        let offset = Vec2::new(to_points(viewport.left), to_points(viewport.top));
        let size = Vec2::new(
            to_points(ui::WIDTH * viewport.scale),
            to_points(ui::HEIGHT * viewport.scale),
        );
        let live = !view.read_only;
        let outline = self.show_outline.then(|| self.screen.outline()).flatten();
        let mut touch = None;
        ScrollArea::both().auto_shrink(false).show(ui, |ui| {
            let (space, response) = ui.allocate_exact_size(offset + size, Sense::click());
            // Round the top left of the panel to a screen pixel.
            let min = (space.min + offset) * pixels_per_point;
            let min = Pos2::new(min.x.round(), min.y.round()) / pixels_per_point;
            let rect = Rect::from_min_size(min, size);
            let painter = ui.painter();
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            painter.image(texture, rect, uv, Color32::WHITE);
            if let Some(tint) = light_tint(self.shell.light()) {
                painter.rect_filled(rect, 0.0, tint);
            }
            if let Some(changed) = outline {
                let outline = outline_rect(changed, rect.min, viewport.scale, pixels_per_point);
                painter.rect_stroke(
                    outline,
                    0.0,
                    Stroke::new(1.0, OUTLINE_COLOR),
                    StrokeKind::Inside,
                );
            }
            if live {
                painter.rect_stroke(
                    rect,
                    0.0,
                    Stroke::new(LIVE_BORDER, LIVE_COLOR),
                    StrokeKind::Inside,
                );
            }
            if response.clicked() {
                let visible = ui.clip_rect();
                touch = response.interact_pointer_pos().and_then(|position| {
                    visible_panel_point(
                        position,
                        visible,
                        rect.min,
                        viewport.scale,
                        pixels_per_point,
                    )
                });
            }
        });
        if let Some(point) = touch {
            self.shell.tap(point);
        }
    }

    fn controls(&mut self, ui: &mut Ui, view: &ViewModel) {
        ui.heading("Device");
        ui.label("Power source");
        let mut power = self.power;
        ui.horizontal(|ui| {
            ui.radio_value(&mut power, PowerSource::Stand, "Stand");
            ui.radio_value(&mut power, PowerSource::Battery, "Battery");
        });
        self.set_power(power);
        ui.label(format!("Battery: {}% ([ and ])", self.level));
        ui.horizontal(|ui| {
            ui.label(if view.asleep { "Asleep" } else { "Awake" });
            if ui
                .add_enabled(view.asleep, egui::Button::new("Wake"))
                .on_hover_text(
                    "A wake with no cause. The device sleeps again after 3 s. \
                     Click the panel to wake the device.",
                )
                .clicked()
            {
                self.shell.wake();
            }
            self.sleep_now_control(ui, view);
            self.restart_control(ui);
        });
        ui.label(format!("Connection: {:?}", view.connection));

        ui.separator();
        self.radio_controls(ui);

        ui.separator();
        ui.heading("MQTT");
        self.read_only_control(ui, view);

        ui.separator();
        ui.heading("Panel");
        ui.label("Scale (screen pixels for each panel pixel)");
        ui.horizontal(|ui| {
            ui.radio_value(&mut self.scale, Scale::Fit, "Fit");
            ui.radio_value(&mut self.scale, Scale::Fixed(1), "1×");
            ui.radio_value(&mut self.scale, Scale::Fixed(2), "2×");
        });
        ui.checkbox(&mut self.show_outline, "Outline of the partial refresh (D)");
        let counters = self.screen.counters();
        ui.label(format!("Full refreshes: {}", counters.full));
        ui.label(format!("Partial refreshes: {}", counters.partial));
        match counters.last_partial {
            Some(area) => ui.label(format!(
                "Last partial: {}×{} at ({}, {}), {:.1}% of the panel",
                area.size.width,
                area.size.height,
                area.top_left.x,
                area.top_left.y,
                share(area)
            )),
            None => ui.label("Last partial: none"),
        };

        ui.separator();
        ui.label(
            RichText::new(
                "Keys (when no text box has the focus): S stand/battery, [ and ] \
                 battery −10% and +10%, W wake with no cause, Z sleep now, R restart, \
                 L drop the link, B block or unblock the device, A move the strongest \
                 access point, K make the driver stuck or free, D outline, \
                 Left/Right page, Q quit",
            )
            .small(),
        );
    }

    /// Shows the state of the fake radio, and the buttons that change it.
    /// Each button has a key.
    fn radio_controls(&mut self, ui: &mut Ui) {
        ui.heading("Radio");
        ui.label(radio_text(self.shell.radio_state()));
        let radio = self.shell.radio_state();
        let block = if radio.blocked {
            "Unblock (B)"
        } else {
            "Block (B)"
        };
        let stuck = if radio.stuck {
            "Free driver (K)"
        } else {
            "Stick driver (K)"
        };
        ui.horizontal_wrapped(|ui| {
            if ui
                .button("Drop link (L)")
                .on_hover_text(
                    "The device loses the link. The stream ends with \"link lost\", and the \
                     core joins again.",
                )
                .clicked()
            {
                self.shell.drop_link();
            }
            if ui
                .button(block)
                .on_hover_text("The router blocks the device. Each join fails.")
                .clicked()
            {
                self.shell.toggle_blocked();
            }
            if ui
                .button("Move access point (A)")
                .on_hover_text(
                    "Another access point gets the strongest signal. The core roams at the \
                     next quiet scan.",
                )
                .clicked()
            {
                self.shell.move_strongest();
            }
            if ui
                .button(stuck)
                .on_hover_text(
                    "DHCP gives no address. After 3 joins with no address, the core \
                     restarts the chip.",
                )
                .clicked()
            {
                self.shell.toggle_stuck();
            }
        });
    }

    /// Shows the "Sleep now" button. The button ends the idle time of the
    /// core now. The core then decides to sleep, as after a real idle
    /// timeout.
    fn sleep_now_control(&mut self, ui: &mut Ui, view: &ViewModel) {
        let enabled = self.shell.can_sleep_now();
        let reason = if view.power == PowerSource::Stand {
            "The device does not sleep on the stand."
        } else if view.asleep {
            "The device is asleep."
        } else {
            "The device cannot sleep now."
        };
        let clicked = ui
            .add_enabled(enabled, egui::Button::new("Sleep now (Z)"))
            .on_hover_text(
                "Ends the idle time now. The core then sleeps as after the idle timeout. \
                 If taps wait to be sent, the core first waits one more idle period.",
            )
            .on_disabled_hover_text(reason)
            .clicked();
        if clicked {
            self.shell.sleep_now();
        }
    }

    /// Shows the "Restart" button. The button simulates the wake from a deep
    /// sleep: the shell makes a new core and keeps the page and the time of
    /// the last update.
    fn restart_control(&mut self, ui: &mut Ui) {
        let clicked = ui
            .add_enabled(self.shell.can_restart(), egui::Button::new("Restart (R)"))
            .on_hover_text(
                "Simulates the wake from a deep sleep. The shell makes a new core. \
                 The page and the time of the last update stay.",
            )
            .on_disabled_hover_text("The device is awake. A restart needs a sleeping device.")
            .clicked();
        if clicked {
            self.shell.restart();
        }
    }

    /// Shows if a tap switches real devices. The core keeps the dry run, and
    /// the control sends `SetReadOnly`. The switch from a dry run to live
    /// needs a second click to confirm. The switch back is immediate.
    fn read_only_control(&mut self, ui: &mut Ui, view: &ViewModel) {
        if view.read_only {
            ui.label("Dry run: taps change no device. The core does not publish service calls.");
            if self.live.asking {
                ui.colored_label(LIVE_COLOR, "Taps will switch real devices in the house.");
                ui.horizontal(|ui| {
                    let confirm =
                        egui::Button::new(RichText::new("Confirm: go live").color(Color32::WHITE))
                            .fill(LIVE_COLOR);
                    if ui.add(confirm).clicked() && self.live.confirm() {
                        self.shell.send(Event::SetReadOnly(false));
                    }
                    if ui.button("Cancel").clicked() {
                        self.live.cancel();
                    }
                });
            } else if ui.button("Go live…").clicked() {
                self.live.ask();
            }
        } else {
            ui.label(
                RichText::new("LIVE: taps switch real devices")
                    .strong()
                    .color(LIVE_COLOR),
            );
            if ui.button("Back to dry run").clicked() {
                self.shell.send(Event::SetReadOnly(true));
            }
        }
    }

    fn view_model(&mut self, ui: &mut Ui, view: &ViewModel) {
        ui.heading("Shell and model");
        egui::Grid::new("facts").num_columns(2).show(ui, |ui| {
            let mut fact = |name: &str, value: String| {
                ui.label(name);
                ui.monospace(value);
                ui.end_row();
            };
            fact("Route", view.path.clone());
            fact("Connection", format!("{:?}", view.connection));
            fact("Power", format!("{:?}", view.power));
            fact("Asleep", view.asleep.to_string());
            fact("Read-only", view.read_only.to_string());
            fact(
                "MQTT connection",
                self.shell
                    .current_connection()
                    .map_or_else(|| "none".into(), |id| id.to_string()),
            );
            fact("Timers", self.shell.timer_count().to_string());
            fact("Full refresh seq", view.full_refresh_seq.to_string());
        });
        ui.separator();
        egui::CollapsingHeader::new("View model")
            .default_open(true)
            .show(ui, |ui| {
                let text = self.view_text.text(view);
                ScrollArea::both().show(ui, |ui| {
                    ui.monospace(text);
                });
            });
    }

    fn effect_log(&mut self, ui: &mut Ui) {
        let header = log_header("Effects", &self.shell.inspector().effects);
        ui.horizontal(|ui| {
            ui.heading(header);
            ui.add(TextEdit::singleline(&mut self.effect_filter).hint_text("Filter"));
            if ui.button("Clear").clicked() {
                self.shell.inspector_mut().effects.clear();
            }
        });
        ui.checkbox(
            &mut self.hide_routine,
            format!("Hide routine effects ({})", ROUTINE_EFFECTS.join(", ")),
        );
        let inspector: &Inspector = self.shell.inspector();
        let entries: Vec<_> = inspector
            .effects
            .shown(&self.effect_filter, self.hide_routine)
            .collect();
        rows(ui, "effect log", entries.len(), |ui, index| {
            let entry = entries[index];
            ui.monospace(entry.time.format(TIME).to_string());
            ui.monospace(RichText::new(entry.effect).strong());
            if !entry.request.is_empty() {
                ui.monospace(&entry.request);
            }
            ui.add(Label::new(&entry.outcome).truncate());
        });
    }

    fn mqtt_log(&mut self, ui: &mut Ui) {
        let header = log_header("MQTT", &self.shell.inspector().mqtt);
        ui.horizontal(|ui| {
            ui.heading(header);
            ui.add(TextEdit::singleline(&mut self.mqtt_filter).hint_text("Topic filter"));
            if ui.button("Clear").clicked() {
                self.shell.inspector_mut().mqtt.clear();
            }
        });
        let inspector: &Inspector = self.shell.inspector();
        let entries: Vec<_> = inspector.mqtt.filtered(&self.mqtt_filter).collect();
        rows(ui, "mqtt log", entries.len(), |ui, index| {
            let entry = entries[index];
            ui.monospace(entry.time.format(TIME).to_string());
            ui.monospace(match entry.direction {
                Direction::In => "in ",
                Direction::Out => "out",
            });
            ui.monospace(format!("{:?}", entry.packet));
            if entry.retained {
                ui.monospace("retained");
            }
            let topic = ui.monospace(&entry.topic);
            if !entry.topics.is_empty() {
                topic.on_hover_text(entry.topics.join("\n"));
            }
            ui.add(Label::new(&entry.payload).truncate());
        });
    }
}

/// Returns the state of the fake radio as text.
fn radio_text(radio: &FakeRadio) -> String {
    let access_point = radio.associated.map_or_else(
        || "none".to_string(),
        |access_point| {
            let bssid = access_point.bssid.map(|byte| format!("{byte:02x}"));
            format!("{} (channel {})", bssid.join(":"), access_point.channel)
        },
    );
    let yes = |flag| if flag { "yes" } else { "no" };
    format!(
        "Radio: on {}, access point {access_point}, address {}, blocked {}, stuck {}",
        yes(radio.on),
        yes(radio.has_address),
        yes(radio.blocked),
        yes(radio.stuck)
    )
}

/// Returns the header of a log: its name, the number of entries, and the
/// number of entries that the log dropped because it was full.
fn log_header<T>(name: &str, log: &Log<T>) -> String {
    match log.dropped() {
        0 => format!("{name} ({})", log.len()),
        dropped => format!("{name} ({}, {dropped} dropped)", log.len()),
    }
}

/// Shows `count` rows of a log in a scroll area. Draws only the rows that
/// are visible. `row` draws the row with an index.
fn rows(ui: &mut Ui, id: &str, count: usize, mut row: impl FnMut(&mut Ui, usize)) {
    if count == 0 {
        ui.label("No entries");
        return;
    }
    let height = ui
        .text_style_height(&TextStyle::Monospace)
        .max(ui.text_style_height(&TextStyle::Body));
    ScrollArea::both()
        .id_salt(id)
        .auto_shrink(false)
        .show_rows(ui, height, count, |ui, range| {
            for index in range {
                ui.horizontal(|ui| row(ui, index));
            }
        });
}

/// Returns the tint of the front light at `level` percent, or `None` if the
/// light is off. A higher level gives a stronger tint.
fn light_tint(level: u8) -> Option<Color32> {
    if level == 0 {
        return None;
    }
    let alpha = u32::from(level.min(100)) * LIGHT_MAX_ALPHA / 100;
    let alpha = u8::try_from(alpha).unwrap_or(u8::MAX);
    let (red, green, blue) = LIGHT_COLOR;
    Some(Color32::from_rgba_unmultiplied(red, green, blue, alpha))
}

/// Converts a position in the window to a point on the panel, or `None` if
/// the position is not on the panel. The top left of the panel is at
/// `origin`, and each panel pixel is `scale` screen pixels wide. Both
/// positions are in egui points.
fn panel_point(position: Pos2, origin: Pos2, scale: u32, pixels_per_point: f32) -> Option<Point> {
    let x = f64::from((position.x - origin.x) * pixels_per_point);
    let y = f64::from((position.y - origin.y) * pixels_per_point);
    let viewport = Viewport {
        scale,
        left: 0,
        top: 0,
    };
    viewport.to_panel(x, y)
}

/// Converts a position in the window to a point on the panel, as
/// [`panel_point`] does, but only if the position is in `visible`, the part
/// of the scroll area that shows.
fn visible_panel_point(
    position: Pos2,
    visible: Rect,
    origin: Pos2,
    scale: u32,
    pixels_per_point: f32,
) -> Option<Point> {
    if visible.contains(position) {
        panel_point(position, origin, scale, pixels_per_point)
    } else {
        None
    }
}

/// Returns the outline of `area` in egui points, for a panel whose top left
/// is at `origin` and whose pixels are `scale` screen pixels wide.
fn outline_rect(area: Rectangle, origin: Pos2, scale: u32, pixels_per_point: f32) -> Rect {
    #[allow(
        clippy::cast_precision_loss,
        reason = "the values are much smaller than 2^24"
    )]
    let to_points = |value: i64| value as f32 * scale as f32 / pixels_per_point;
    let left = to_points(i64::from(area.top_left.x));
    let top = to_points(i64::from(area.top_left.y));
    let width = to_points(i64::from(area.size.width));
    let height = to_points(i64::from(area.size.height));
    Rect::from_min_size(origin + Vec2::new(left, top), Vec2::new(width, height))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_tint_of_the_front_light_is_stronger_at_a_higher_level() {
        use shared::features::light::{BATTERY_LEVEL, STAND_LEVEL};

        assert_eq!(super::light_tint(0), None);
        let battery = super::light_tint(BATTERY_LEVEL).unwrap();
        let stand = super::light_tint(STAND_LEVEL).unwrap();
        assert!(battery.a() < stand.a());
        assert!(super::light_tint(100).unwrap().a() >= stand.a());
    }

    use eframe::egui::{Pos2, Rect, Vec2};
    use embedded_graphics::{prelude::*, primitives::Rectangle};

    use shared::{PowerSource, fixtures::FixedShell};

    use super::{LiveGuard, ViewText, log_header, outline_rect, panel_point, visible_panel_point};
    use crate::inspector::Log;

    #[test]
    fn a_click_maps_to_the_panel_at_each_scale() {
        let origin = Pos2::new(100.0, 50.0);
        // One screen pixel for each point, scale 1.
        assert_eq!(
            panel_point(Pos2::new(100.0, 50.0), origin, 1, 1.0),
            Some(Point::new(0, 0))
        );
        assert_eq!(
            panel_point(Pos2::new(1059.5, 589.5), origin, 1, 1.0),
            Some(Point::new(959, 539))
        );
        assert_eq!(panel_point(Pos2::new(1060.0, 60.0), origin, 1, 1.0), None);
        assert_eq!(panel_point(Pos2::new(99.0, 60.0), origin, 1, 1.0), None);

        // A Retina display at scale 1: a panel pixel is half a point.
        assert_eq!(
            panel_point(Pos2::new(110.0, 60.0), origin, 1, 2.0),
            Some(Point::new(20, 20))
        );
        assert_eq!(
            panel_point(Pos2::new(579.75, 319.75), origin, 1, 2.0),
            Some(Point::new(959, 539))
        );
        assert_eq!(panel_point(Pos2::new(580.0, 60.0), origin, 1, 2.0), None);

        // A Retina display at scale 2: a panel pixel is one point.
        assert_eq!(
            panel_point(Pos2::new(110.5, 60.5), origin, 2, 2.0),
            Some(Point::new(10, 10))
        );
        assert_eq!(
            panel_point(Pos2::new(1059.5, 589.5), origin, 2, 2.0),
            Some(Point::new(959, 539))
        );
        assert_eq!(panel_point(Pos2::new(1060.0, 60.0), origin, 2, 2.0), None);

        // Scale 2 with one screen pixel for each point.
        assert_eq!(
            panel_point(Pos2::new(103.0, 55.0), origin, 2, 1.0),
            Some(Point::new(1, 2))
        );
    }

    #[test]
    fn the_outline_covers_the_area_in_points() {
        let area = Rectangle::new(Point::new(10, 20), Size::new(30, 40));
        let origin = Pos2::new(100.0, 50.0);
        assert_eq!(
            outline_rect(area, origin, 2, 1.0),
            Rect::from_min_size(Pos2::new(120.0, 90.0), Vec2::new(60.0, 80.0))
        );
        assert_eq!(
            outline_rect(area, origin, 1, 2.0),
            Rect::from_min_size(Pos2::new(105.0, 60.0), Vec2::new(15.0, 20.0))
        );
    }

    #[test]
    fn a_click_maps_only_in_the_visible_part_of_a_scrolled_panel() {
        // The scroll area shows 400 × 300 points from (300, 100). The user
        // scrolled 500 points right and 200 down, so the panel at 2 screen
        // pixels for each panel pixel starts above and left of the area.
        let visible = Rect::from_min_size(Pos2::new(300.0, 100.0), Vec2::new(400.0, 300.0));
        let origin = Pos2::new(300.0 - 500.0, 100.0 - 200.0);
        assert_eq!(
            visible_panel_point(Pos2::new(300.0, 100.0), visible, origin, 2, 1.0),
            Some(Point::new(250, 100))
        );
        assert_eq!(
            visible_panel_point(Pos2::new(699.0, 399.0), visible, origin, 2, 1.0),
            Some(Point::new(449, 249))
        );
        // A position on the panel but out of the visible part is not a touch.
        assert_eq!(
            visible_panel_point(Pos2::new(299.0, 150.0), visible, origin, 2, 1.0),
            None
        );
        assert_eq!(
            visible_panel_point(Pos2::new(500.0, 401.0), visible, origin, 2, 1.0),
            None
        );
    }

    #[test]
    fn going_live_needs_a_request_and_a_confirmation() {
        let mut guard = LiveGuard::default();
        assert!(!guard.confirm(), "a confirm with no request does nothing");
        guard.ask();
        guard.cancel();
        assert!(!guard.confirm());
        guard.ask();
        assert!(guard.confirm());
        assert!(!guard.confirm(), "each request allows one switch");
    }

    #[test]
    fn the_view_text_is_made_again_only_when_the_view_changes() {
        let mut shell = FixedShell::start(PowerSource::Stand, "/room/living_room");
        let mut text = ViewText::default();
        let view = shell.view();
        assert!(text.text(&view).contains("living"));
        text.text(&view);
        text.text(&shell.view());
        assert_eq!(text.made, 1);
        shell.drop_connection();
        text.text(&shell.view());
        assert_eq!(text.made, 2);
    }

    #[test]
    fn the_log_header_shows_the_dropped_entries() {
        let mut log = Log::new(2);
        log.push(1);
        assert_eq!(log_header("MQTT", &log), "MQTT (1)");
        log.push(2);
        log.push(3);
        assert_eq!(log_header("MQTT", &log), "MQTT (2, 1 dropped)");
    }
}
