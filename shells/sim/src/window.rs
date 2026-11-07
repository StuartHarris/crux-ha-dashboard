//! The window of the live simulator.
//!
//! The window uses `winit` and `softbuffer`. Both crates are pure Rust and
//! load X11 or Wayland at run time, so they need no SDL and no system
//! packages to build.
//!
//! The panel shows at an integer scale, so each panel pixel is a square of
//! screen pixels. On a Retina display the scale is 2.

use std::{
    num::NonZeroU32,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use embedded_graphics::{prelude::*, primitives::Rectangle};
use shared::{Event, PowerSource, Settings};
use softbuffer::{Context, Surface};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, KeyEvent, MouseButton, StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowId},
};

use crate::{
    mqtt,
    panel::{Panel, Refresh, Viewport, share},
    settings::Options,
    shell::{Notifier, Shell, Signal},
};

/// The time for which a full refresh shows the inverted image.
const FLASH: Duration = Duration::from_millis(150);
/// The time for which the outline of a partial refresh shows.
const OUTLINE: Duration = Duration::from_millis(500);
/// The colour around the panel.
const BACKGROUND: u32 = 0x0030_3030;
/// The colour of the outline of a partial refresh.
const OUTLINE_COLOR: u32 = 0x00FF_0000;
/// The window title.
const TITLE: &str = "Dashboard simulator";

/// Opens the window, starts the core and runs until the user quits.
///
/// # Errors
///
/// Returns a message if the window or the runtime cannot start.
pub fn run(options: Options) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    let event_loop = EventLoop::new().map_err(|error| error.to_string())?;
    let proxy = Mutex::new(event_loop.create_proxy());
    let (sender, signals) = mpsc::channel();
    let notifier = Notifier::new(
        sender,
        Arc::new(move || {
            if let Ok(proxy) = proxy.lock() {
                // If the event loop stopped, there is nothing to wake.
                let _ = proxy.send_event(());
            }
        }),
    );
    let Options {
        settings,
        power,
        path,
        read_only,
        ..
    } = options;
    if read_only {
        eprintln!("read-only: a click does not change devices in the house");
    } else {
        eprintln!(
            "warning: a click switches real devices in the house. Give --read-only to prevent this."
        );
    }
    let open = mqtt::opener(runtime.handle().clone());
    let mut app = App {
        shell: Shell::new(runtime.handle().clone(), notifier, open, read_only),
        signals,
        panel: Panel::new(),
        lumas: Vec::new(),
        start: Some((settings, path)),
        power,
        window: None,
        surface: None,
        cursor: None,
        flash_until: None,
        outline: None,
        show_outline: false,
    };
    eprintln!("keys: S stand/battery, W wake, D show refresh areas, Left/Right page, Q quit");
    let result = event_loop.run_app(&mut app);
    // Stop the tasks with no disconnect packet. The broker publishes the
    // last will, as for a device that loses power.
    drop(app);
    runtime.shutdown_background();
    result.map_err(|error| error.to_string())
}

struct App {
    shell: Shell,
    signals: mpsc::Receiver<Signal>,
    panel: Panel,
    /// The grey level of each panel pixel, from the last draw.
    lumas: Vec<u8>,
    start: Option<(Settings, Option<String>)>,
    power: PowerSource,
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    cursor: Option<(f64, f64)>,
    flash_until: Option<Instant>,
    outline: Option<(Rectangle, Instant)>,
    show_outline: bool,
}

impl ApplicationHandler for App {
    fn new_events(&mut self, _: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            let now = Instant::now();
            if self.flash_until.is_some_and(|until| until <= now) {
                self.flash_until = None;
            }
            if self.outline.is_some_and(|(_, until)| until <= now) {
                self.outline = None;
            }
            self.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(TITLE)
            .with_inner_size(LogicalSize::new(
                f64::from(ui::WIDTH),
                f64::from(ui::HEIGHT),
            ));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Rc::new(window),
            Err(error) => {
                eprintln!("cannot open the window: {error}");
                event_loop.exit();
                return;
            }
        };
        let surface =
            Context::new(window.clone()).and_then(|context| Surface::new(&context, window.clone()));
        match surface {
            Ok(surface) => self.surface = Some(surface),
            Err(error) => {
                eprintln!("cannot draw in the window: {error}");
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
        // Start the core after the window exists, so the first render has
        // somewhere to go.
        if let Some((settings, path)) = self.start.take() {
            self.shell.start(settings, self.power, path);
        }
        self.after_events();
    }

    fn user_event(&mut self, _: &ActiveEventLoop, (): ()) {
        while let Ok(signal) = self.signals.try_recv() {
            self.shell.handle(signal);
        }
        self.after_events();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.paint(),
            WindowEvent::Resized(_) => self.request_redraw(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some((position.x, position.y));
            }
            WindowEvent::CursorLeft { .. } => self.cursor = None,
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => self.click(),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state: ElementState::Pressed,
                        repeat: false,
                        ..
                    },
                ..
            } => self.key(code, event_loop),
            _ => {}
        }
        self.after_events();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let deadline = [self.flash_until, self.outline.map(|(_, until)| until)]
            .into_iter()
            .flatten()
            .min();
        event_loop.set_control_flow(deadline.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}

impl App {
    /// A click is a touch. A touch while the device sleeps wakes it and is
    /// not a tap.
    fn click(&mut self) {
        let Some(window) = &self.window else {
            return;
        };
        let Some((x, y)) = self.cursor else {
            return;
        };
        let size = window.inner_size();
        let Some(point) = Viewport::fit(size.width, size.height).to_panel(x, y) else {
            return;
        };
        let view = self.shell.view();
        if view.asleep {
            self.shell.wake();
        } else if let Some(event) = ui::hit_test(&view, point) {
            self.shell.send(event);
        }
    }

    fn key(&mut self, code: KeyCode, event_loop: &ActiveEventLoop) {
        match code {
            KeyCode::KeyQ | KeyCode::Escape => event_loop.exit(),
            KeyCode::KeyS => {
                self.power = match self.power {
                    PowerSource::Stand => PowerSource::Battery,
                    PowerSource::Battery => PowerSource::Stand,
                };
                eprintln!("power: {:?}", self.power);
                self.shell.send(Event::PowerSourceChanged(self.power));
            }
            KeyCode::KeyW => self.shell.wake(),
            KeyCode::KeyD => {
                self.show_outline = !self.show_outline;
                let state = if self.show_outline { "on" } else { "off" };
                eprintln!("panel: the outline of each partial refresh is {state}");
            }
            KeyCode::ArrowLeft => self.shell.send(Event::PreviousPage),
            KeyCode::ArrowRight => self.shell.send(Event::NextPage),
            _ => {}
        }
    }

    /// Draws the view if the core asked for a render, and simulates the
    /// refresh of the panel.
    fn after_events(&mut self) {
        if !self.shell.take_render() {
            return;
        }
        let view = self.shell.view();
        let update = self.panel.draw(&view);
        self.lumas = self.panel.lumas();
        let now = Instant::now();
        match (update.refresh, update.changed) {
            (Refresh::Full, _) => {
                eprintln!("panel: full refresh");
                self.flash_until = Some(now + FLASH);
                self.outline = None;
            }
            (Refresh::Partial, Some(area)) => {
                eprintln!(
                    "panel: partial refresh of {}x{} at ({}, {}), {:.1}% of the panel",
                    area.size.width,
                    area.size.height,
                    area.top_left.x,
                    area.top_left.y,
                    share(area)
                );
                if self.show_outline {
                    self.outline = Some((area, now + OUTLINE));
                }
            }
            (Refresh::Partial, None) => {}
        }
        if let Some(window) = &self.window {
            if view.asleep {
                window.set_title(&format!("{TITLE} (asleep: click or press W to wake)"));
            } else {
                window.set_title(TITLE);
            }
        }
        self.request_redraw();
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// Copies the panel to the window at an integer scale.
    fn paint(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return;
        };
        let size = window.inner_size();
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };
        if surface.resize(width, height).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        let viewport = Viewport::fit(size.width, size.height);
        let flash = self.flash_until.is_some_and(|until| Instant::now() < until);
        paint_panel(&mut buffer, size.width, viewport, &self.lumas, flash);
        if let Some((area, _)) = self.outline {
            paint_outline(&mut buffer, size.width, viewport, area);
        }
        if let Err(error) = buffer.present() {
            eprintln!("cannot show the frame: {error}");
        }
    }
}

/// Fills `buffer` (rows of `width` pixels) with the panel image. If `flash`
/// is `true`, inverts the image, as a full refresh of the panel does.
fn paint_panel(buffer: &mut [u32], width: u32, viewport: Viewport, lumas: &[u8], flash: bool) {
    let panel_width = ui::WIDTH as usize;
    let Viewport { scale, left, top } = viewport;
    let row_length = width as usize;
    for (y, row) in (0_u32..).zip(buffer.chunks_mut(row_length)) {
        let panel_y = y
            .checked_sub(top)
            .map(|y| y / scale)
            .filter(|y| *y < ui::HEIGHT);
        for (x, pixel) in (0_u32..).zip(row.iter_mut()) {
            let panel_x = x
                .checked_sub(left)
                .map(|x| x / scale)
                .filter(|x| *x < ui::WIDTH);
            *pixel = match (panel_x, panel_y) {
                (Some(px), Some(py)) => {
                    let index = py as usize * panel_width + px as usize;
                    let luma = lumas.get(index).copied().unwrap_or(15);
                    let luma = if flash { 15 - luma } else { luma };
                    let level = u32::from(luma) * 17;
                    level << 16 | level << 8 | level
                }
                _ => BACKGROUND,
            };
        }
    }
}

/// Draws a line of one panel pixel around `area`.
fn paint_outline(buffer: &mut [u32], width: u32, viewport: Viewport, area: Rectangle) {
    let Some(bottom_right) = area.bottom_right() else {
        return;
    };
    let Viewport { scale, left, top } = viewport;
    let to_window = |point: Point| {
        (
            left + point.x.unsigned_abs() * scale,
            top + point.y.unsigned_abs() * scale,
        )
    };
    let (x0, y0) = to_window(area.top_left);
    let (x1, y1) = to_window(bottom_right + Point::new(1, 1));
    let mut set = |x: u32, y: u32| {
        // A pixel past the right edge is not in this row, so skip it.
        if x >= width {
            return;
        }
        if let Some(pixel) = buffer.get_mut(y as usize * width as usize + x as usize) {
            *pixel = OUTLINE_COLOR;
        }
    };
    for t in 0..scale {
        for x in x0..x1 {
            set(x, y0 + t);
            set(x, y1 - 1 - t);
        }
        for y in y0..y1 {
            set(x0 + t, y);
            set(x1 - 1 - t, y);
        }
    }
}
