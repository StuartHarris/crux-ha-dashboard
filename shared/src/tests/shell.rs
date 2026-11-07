//! A test shell. It runs the core and answers each effect as a real shell
//! does, with the fake broker for MQTT.
//!
//! Timers do not expire by themselves. A test fires them with
//! [`Shell::fire_timers`], so the test controls the time.

use alloc::{format, string::String, vec::Vec};
use core::time::Duration;

use crux_core::{App, Request};
use crux_time::operation::NotifyAfter;

use super::{broker::Broker, instant};
use crate::{
    Cmd, Dashboard, Effect, Event, Model, ViewModel,
    capabilities::{network::WifiStatus, power::Sleep},
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
pub struct Shell {
    app: Dashboard,
    pub model: Model,
    commands: Vec<Cmd>,
    pub broker: Broker,
    /// If `false`, the shell answers `WifiOn` with `Failed`.
    pub wifi_works: bool,
    /// Wi-Fi is on.
    pub wifi_on: bool,
    /// The time in seconds since the Unix epoch.
    pub clock: u64,
    timers: Vec<Request<NotifyAfter>>,
    /// The frames that the shell drew, in order.
    pub frames: Vec<Frame>,
    /// The `full_refresh_seq` of the last full refresh.
    drawn_seq: u32,
    /// The core asked for a render that the shell did not draw yet.
    render_requested: bool,
    /// The last sleep request. A start or a wake clears it.
    pub sleep: Option<Sleep>,
    /// A short name for each effect, in order.
    pub log: Vec<String>,
}

impl Shell {
    pub fn new() -> Self {
        Self {
            app: Dashboard,
            model: Model::default(),
            commands: Vec::new(),
            broker: Broker::default(),
            wifi_works: true,
            wifi_on: false,
            clock: super::NOW,
            timers: Vec::new(),
            frames: Vec::new(),
            drawn_seq: 0,
            render_requested: false,
            sleep: None,
            log: Vec::new(),
        }
    }

    /// Sends an event to the core and answers effects until the core waits.
    pub fn send(&mut self, event: Event) {
        if matches!(event, Event::Woke | Event::Start(_)) {
            self.sleep = None;
        }
        self.queue(event);
        self.run();
    }

    /// Sends an event to the core, but does not answer the effects yet.
    pub fn queue(&mut self, event: Event) {
        let command = self.app.update(event, &mut self.model);
        self.commands.push(command);
    }

    /// Answers effects and sends events until the core waits.
    pub fn run(&mut self) {
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

    /// Returns the durations of the timers that wait.
    pub fn timers(&self) -> Vec<Duration> {
        self.timers
            .iter()
            .map(|timer| timer.operation.duration.into())
            .collect()
    }

    /// Fires all the timers that wait, as if their time passed.
    pub fn fire_timers(&mut self) {
        let longest = self.timers().into_iter().max().unwrap_or_default();
        self.clock += longest.as_secs();
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
        self.clock += shortest.as_secs();
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
            Effect::MqttDisconnect(_) => {
                self.log.push("MqttDisconnect".into());
                self.broker.disconnect();
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
            Effect::WifiOff(_) => {
                self.log.push("WifiOff".into());
                self.wifi_on = false;
                self.broker.drop_connection();
            }
            Effect::Sleep(request) => {
                self.log.push("Sleep".into());
                self.sleep = Some(request.operation);
            }
            Effect::TimeNow(mut request) => {
                self.log.push("TimeNow".into());
                request
                    .resolve(instant(self.clock))
                    .expect("the core waits for the time");
            }
            Effect::TimeNotifyAfter(request) => {
                let seconds = Duration::from(request.operation.duration).as_secs();
                self.log.push(format!("TimeNotifyAfter {seconds}s"));
                self.timers.push(request);
            }
            Effect::TimeClear(mut request) => {
                self.log.push("TimeClear".into());
                let id = request.operation.id;
                self.timers.retain(|timer| timer.operation.id != id);
                request.resolve(id).expect("the core waits for the clear");
            }
        }
    }
}
