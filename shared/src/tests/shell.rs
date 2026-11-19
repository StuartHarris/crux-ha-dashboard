//! A test shell. It runs the core and answers each effect as a real shell
//! does, with the fake broker for MQTT.
//!
//! Timers do not expire by themselves. A test fires them with
//! [`Shell::fire_timers`], so the test controls the time.
//!
//! The full-refresh timer runs all the time while the device is awake, so the
//! shell keeps it apart from the other timers. [`Shell::timers`] and
//! [`Shell::fire_timers`] do not include it. A test fires it with
//! [`Shell::fire_full_refresh_timer`].
//!
//! The draw timer is short, and the steps of a test are far apart in time.
//! Thus, by default, [`Shell::run`] fires the draw timers when the core
//! waits, and draws. A test of the draw timer sets [`Shell::auto_draw`] to
//! `false`, and then fires them with [`Shell::fire_draw_timers`].

use alloc::{format, string::String, vec::Vec};
use core::time::Duration;

use crux_core::{App, Request};
use crux_time::operation::NotifyAfter;

use super::broker::Broker;
use crate::{
    Cmd, Dashboard, Effect, Event, Model, ViewModel,
    capabilities::{
        network::WifiStatus,
        power::{Sleep, Wake},
    },
    features::refresh::{DRAW_DELAY, FULL_REFRESH_INTERVAL},
};

/// One image on the panel.
pub struct Frame {
    /// The view that the shell drew.
    pub view: ViewModel,
    /// The shell did a full refresh.
    pub full: bool,
}

/// The test shell.
///
/// Like an e-paper shell, it draws once at the end of [`Shell::run`], for all
/// the `Render` requests of the run.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is a separate fact of the fake device"
)]
pub struct Shell {
    app: Dashboard,
    pub model: Model,
    commands: Vec<Cmd>,
    pub broker: Broker,
    /// If `false`, the shell answers `WifiOn` with `Failed`.
    pub wifi_works: bool,
    /// Wi-Fi is on.
    pub wifi_on: bool,
    timers: Vec<Request<NotifyAfter>>,
    /// The full-refresh timers that wait.
    refresh_timers: Vec<Request<NotifyAfter>>,
    /// The draw timers that wait.
    draw_timers: Vec<Request<NotifyAfter>>,
    /// If `true`, [`Shell::run`] fires the draw timers when the core waits.
    pub auto_draw: bool,
    /// The frames that the shell drew, in order.
    pub frames: Vec<Frame>,
    /// The `full_refresh_seq` of the last full refresh.
    drawn_seq: u32,
    /// The core asked for a render that the shell did not draw yet.
    render_requested: bool,
    /// The `Sleep` request that waits for its answer. A start or a wake
    /// clears it.
    pub sleep: Option<Sleep>,
    sleep_request: Option<Request<Sleep>>,
    /// A short name for each effect, in order.
    pub log: Vec<String>,
}

impl Shell {
    pub fn new() -> Self {
        let mut broker = Broker::default();
        broker.time = super::NOW;
        Self {
            app: Dashboard,
            model: Model::default(),
            commands: Vec::new(),
            broker,
            wifi_works: true,
            wifi_on: false,
            timers: Vec::new(),
            refresh_timers: Vec::new(),
            draw_timers: Vec::new(),
            auto_draw: true,
            frames: Vec::new(),
            drawn_seq: 0,
            render_requested: false,
            sleep: None,
            sleep_request: None,
            log: Vec::new(),
        }
    }

    /// Sends an event to the core and answers effects until the core waits.
    ///
    /// As a real shell does, while a `Sleep` request waits, the shell answers
    /// it with `Wake::Touch` and the control, in place of a tap. After a
    /// change of the power source, it answers it with `Wake::PowerSource`.
    pub fn send(&mut self, event: Event) {
        match event {
            Event::Start(_) => {
                self.sleep = None;
                self.sleep_request = None;
            }
            Event::Tapped(control) if self.sleep_request.is_some() => {
                self.wake(Wake::Touch(control));
                return;
            }
            _ => {}
        }
        let power_changed = matches!(event, Event::PowerSourceChanged(_));
        self.queue(event);
        self.run();
        if power_changed && self.sleep_request.is_some() {
            self.wake(Wake::PowerSource);
        }
    }

    /// Answers the `Sleep` request that waits with `wake`, and answers
    /// effects until the core waits. If no request waits, does nothing.
    pub fn wake(&mut self, wake: Wake) {
        self.sleep = None;
        if let Some(mut request) = self.sleep_request.take() {
            self.log.push(format!("Wake {wake:?}"));
            request.resolve(wake).expect("the core waits for the wake");
            self.run();
        }
    }

    /// Sends an event to the core, but does not answer the effects yet.
    pub fn queue(&mut self, event: Event) {
        let command = self.app.update(event, &mut self.model);
        self.commands.push(command);
    }

    /// Answers effects and sends events until the core waits. If
    /// [`Shell::auto_draw`] is `true`, it then fires the draw timers and
    /// answers the effects again.
    pub fn run(&mut self) {
        self.settle();
        if self.auto_draw {
            while !self.draw_timers.is_empty() {
                self.fire_draw_timers();
            }
        }
    }

    /// Answers effects and sends events until the core waits, and then
    /// draws if the core asked for a render.
    fn settle(&mut self) {
        loop {
            let mut effects = Vec::new();
            let mut events = Vec::new();
            for command in &mut self.commands {
                effects.extend(command.effects());
                events.extend(command.events());
            }
            if effects.is_empty() && events.is_empty() {
                break;
            }
            for effect in effects {
                self.handle(effect);
            }
            for event in events {
                let command = self.app.update(event, &mut self.model);
                self.commands.push(command);
            }
        }
        self.commands.retain_mut(|command| !command.is_done());
        if core::mem::take(&mut self.render_requested) {
            let view = self.app.view(&self.model);
            let full = view.full_refresh_seq != self.drawn_seq;
            self.drawn_seq = view.full_refresh_seq;
            self.frames.push(Frame { view, full });
        }
    }

    /// Returns the view of the core now.
    pub fn view(&self) -> ViewModel {
        self.app.view(&self.model)
    }

    /// Returns the number of full-refresh timers that wait.
    pub const fn full_refresh_timers(&self) -> usize {
        self.refresh_timers.len()
    }

    /// Returns the number of draw timers that wait.
    pub const fn draw_timers(&self) -> usize {
        self.draw_timers.len()
    }

    /// Fires the draw timers that wait, in order, and answers the effects
    /// until the core waits.
    pub fn fire_draw_timers(&mut self) {
        for mut timer in core::mem::take(&mut self.draw_timers) {
            let id = timer.operation.id;
            timer.resolve(id).expect("the core waits for the timer");
        }
        self.settle();
    }

    /// Fires the full-refresh timers that wait, as if
    /// [`FULL_REFRESH_INTERVAL`] passed. The other timers do not fire.
    pub fn fire_full_refresh_timer(&mut self) {
        self.broker.time += FULL_REFRESH_INTERVAL.as_secs();
        for mut timer in core::mem::take(&mut self.refresh_timers) {
            let id = timer.operation.id;
            timer.resolve(id).expect("the core waits for the timer");
        }
        self.run();
    }

    /// Returns the durations of the timers that wait, other than the
    /// full-refresh timer.
    pub fn timers(&self) -> Vec<Duration> {
        self.timers
            .iter()
            .map(|timer| timer.operation.duration.into())
            .collect()
    }

    /// Fires all the timers that wait, other than the full-refresh timer, as
    /// if their time passed.
    pub fn fire_timers(&mut self) {
        let longest = self.timers().into_iter().max().unwrap_or_default();
        self.broker.time += longest.as_secs();
        for mut timer in core::mem::take(&mut self.timers) {
            let id = timer.operation.id;
            timer.resolve(id).expect("the core waits for the timer");
        }
        self.run();
    }

    /// Fires the timers with the shortest wait, as if that time passed.
    pub fn fire_shortest_timers(&mut self) {
        let Some(shortest) = self.timers().into_iter().min() else {
            return;
        };
        self.broker.time += shortest.as_secs();
        let (due, waiting) = core::mem::take(&mut self.timers)
            .into_iter()
            .partition(|timer| Duration::from(timer.operation.duration) == shortest);
        self.timers = waiting;
        for mut timer in due {
            let id = timer.operation.id;
            timer.resolve(id).expect("the core waits for the timer");
        }
        self.run();
    }

    /// Returns the log and clears it, so that a test can check the effects
    /// of the next step.
    pub fn take_log(&mut self) -> Vec<String> {
        core::mem::take(&mut self.log)
    }

    fn handle(&mut self, effect: Effect) {
        match effect {
            Effect::Render(_) => {
                self.log.push("Render".into());
                self.render_requested = true;
            }
            Effect::MqttConnect(request) => {
                self.log.push("MqttConnect".into());
                self.broker.connect(request);
            }
            Effect::MqttSubscribe(request) => {
                self.log.push("MqttSubscribe".into());
                self.broker.subscribe(&request.operation.topics);
            }
            Effect::MqttPublish(request) => {
                let publish = request.operation;
                let payload = String::from_utf8_lossy(&publish.payload);
                self.log
                    .push(format!("MqttPublish {} {payload}", publish.topic));
                self.broker.publish_from_dashboard(publish);
            }
            Effect::MqttDisconnect(mut request) => {
                self.log.push("MqttDisconnect".into());
                // The stream ends first, then the request.
                self.broker.disconnect();
                request
                    .resolve(())
                    .expect("the core waits for the disconnect");
            }
            Effect::WifiOn(mut request) => {
                self.log.push("WifiOn".into());
                self.wifi_on = self.wifi_works;
                let status = if self.wifi_works {
                    WifiStatus::Connected
                } else {
                    WifiStatus::Failed
                };
                request.resolve(status).expect("the core waits for Wi-Fi");
            }
            Effect::WifiOff(mut request) => {
                self.log.push("WifiOff".into());
                self.wifi_on = false;
                self.broker.drop_connection();
                request.resolve(()).expect("the core waits for Wi-Fi off");
            }
            Effect::Sleep(request) => {
                self.log.push("Sleep".into());
                self.sleep = Some(request.operation.clone());
                self.sleep_request = Some(request);
            }
            Effect::TimeNotifyAfter(request) => {
                let duration = Duration::from(request.operation.duration);
                self.log
                    .push(format!("TimeNotifyAfter {}ms", duration.as_millis()));
                if duration == FULL_REFRESH_INTERVAL {
                    self.refresh_timers.push(request);
                } else if duration == DRAW_DELAY {
                    self.draw_timers.push(request);
                } else {
                    self.timers.push(request);
                }
            }
            Effect::TimeClear(mut request) => {
                self.log.push("TimeClear".into());
                let id = request.operation.id;
                self.timers.retain(|timer| timer.operation.id != id);
                self.refresh_timers.retain(|timer| timer.operation.id != id);
                self.draw_timers.retain(|timer| timer.operation.id != id);
                request.resolve(id).expect("the core waits for the clear");
            }
        }
    }
}
