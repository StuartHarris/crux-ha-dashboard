//! A test shell. It runs the core and answers each effect as a real shell
//! does, with the fake broker for MQTT.
//!
//! The shell has a fake clock, which starts at 0. Timers do not expire by
//! themselves. A test moves the clock with [`Shell::advance`] or
//! [`Shell::advance_to_next`]. The timers that are due then fire in the
//! order of their deadlines. Both functions return the events that the
//! fired timers sent to the core, so a test sees what each timer did. The
//! shell does not find a timer by its kind or by its duration.
//!
//! A stale timer (one that the core no longer uses) stays until its
//! deadline, as on a real device. When it fires, the core ignores its event.
//!
//! The draw timer is short, and the steps of a test are far apart in time.
//! Thus, by default, [`Shell::run`] moves the clock to the next deadline
//! while the core waits for a draw. A test of the draw timer sets
//! [`Shell::auto_draw`] to `false`.
//!
//! The shell answers `FrontLight` at once and keeps the level in
//! [`Shell::light`].

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
};

/// A timer that waits.
struct Timer {
    /// The time on the clock of the shell at which the timer fires.
    due: Duration,
    request: Request<NotifyAfter>,
}

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
    /// The timers that wait, in the order in which the core started them.
    timers: Vec<Timer>,
    /// The time on the clock of the shell.
    now: Duration,
    /// The level of the front light, in percent.
    pub light: u8,
    /// If `true`, [`Shell::run`] moves the clock to the next deadline while
    /// the core waits for a draw.
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
            now: Duration::ZERO,
            light: 0,
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
    /// [`Shell::auto_draw`] is `true` and the core waits for a draw, it then
    /// fires the timers in the order of their deadlines until the core drew.
    pub fn run(&mut self) {
        self.settle();
        let mut fired = Vec::new();
        self.auto_draw_until(Duration::MAX, &mut fired);
    }

    /// If [`Shell::auto_draw`] is `true`, fires the timers that are due at or
    /// before `limit`, in the order of their deadlines, while the core waits
    /// for a draw. Adds the events of the fired timers to `fired`.
    fn auto_draw_until(&mut self, limit: Duration, fired: &mut Vec<Event>) {
        if !self.auto_draw {
            return;
        }
        while self.model.refresh.draw_pending {
            let Some(index) = self.next_due(limit) else {
                break;
            };
            self.fire(index, limit, fired);
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

    /// Returns the time on the clock of the shell.
    pub const fn now(&self) -> Duration {
        self.now
    }

    /// Moves the clock forward by `duration`. Each timer that is due fires,
    /// in the order of the deadlines, and the shell answers the effects
    /// until the core waits. Returns the events that the fired timers sent,
    /// in order, also the events of the draw timers.
    pub fn advance(&mut self, duration: Duration) -> Vec<Event> {
        self.settle();
        let target = self.now + duration;
        let mut fired = Vec::new();
        while let Some(index) = self.next_due(target) {
            self.fire(index, target, &mut fired);
        }
        self.set_now(target);
        fired
    }

    /// Moves the clock to the next deadline, and fires the timers that are
    /// due then. Returns the events that they sent. If no timer waits,
    /// returns no events.
    pub fn advance_to_next(&mut self) -> Vec<Event> {
        match self.timers.iter().map(|timer| timer.due).min() {
            Some(due) => self.advance(due.saturating_sub(self.now)),
            None => Vec::new(),
        }
    }

    /// Returns the place of the timer with the earliest deadline at or
    /// before `target`. Of two timers with the same deadline, the timer that
    /// the core started first comes first.
    fn next_due(&self, target: Duration) -> Option<usize> {
        self.timers
            .iter()
            .enumerate()
            .filter(|(_, timer)| timer.due <= target)
            .min_by_key(|(index, timer)| (timer.due, *index))
            .map(|(index, _)| index)
    }

    /// Fires the timer at `index` and answers the effects until the core
    /// waits. A draw that the core then waits for fires only if it is due at
    /// or before `limit`. Adds the events of the fired timers to `fired`.
    fn fire(&mut self, index: usize, limit: Duration, fired: &mut Vec<Event>) {
        let mut timer = self.timers.remove(index);
        self.set_now(timer.due);
        let id = timer.request.operation.id;
        timer
            .request
            .resolve(id)
            .expect("the core waits for the timer");
        // The core waited before the timer fired, so each new event comes
        // from this timer.
        let events: Vec<Event> = self
            .commands
            .iter_mut()
            .flat_map(|command| command.events().collect::<Vec<_>>())
            .collect();
        for event in &events {
            let command = self.app.update(event.clone(), &mut self.model);
            self.commands.push(command);
        }
        fired.extend(events);
        self.settle();
        self.auto_draw_until(limit, fired);
    }

    /// Sets the clock to `now`. The clock of the broker moves forward by the
    /// same number of whole seconds.
    ///
    /// # Panics
    ///
    /// Panics if `now` is before the time on the clock. The clock never
    /// goes back.
    fn set_now(&mut self, now: Duration) {
        assert!(
            now >= self.now,
            "the clock goes back from {:?} to {now:?}",
            self.now
        );
        self.broker.time += now.as_secs() - self.now.as_secs();
        self.now = now;
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
                self.broker.subscribe(request);
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
            Effect::WifiQuiet(_) => self.log.push("WifiQuiet".into()),
            Effect::Sleep(request) => {
                self.log.push("Sleep".into());
                self.sleep = Some(request.operation.clone());
                self.sleep_request = Some(request);
            }
            Effect::FrontLight(mut request) => {
                let level = request.operation.level;
                self.log.push(format!("FrontLight {level}"));
                self.light = level;
                request.resolve(()).expect("the core waits for the light");
            }
            Effect::TimeNotifyAfter(request) => {
                let duration = Duration::from(request.operation.duration);
                self.log
                    .push(format!("TimeNotifyAfter {}ms", duration.as_millis()));
                let due = self.now + duration;
                self.timers.push(Timer { due, request });
            }
            Effect::TimeClear(mut request) => {
                self.log.push("TimeClear".into());
                let id = request.operation.id;
                self.timers.retain(|timer| timer.request.operation.id != id);
                request.resolve(id).expect("the core waits for the clear");
            }
        }
    }
}
