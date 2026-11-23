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
//! The core clears a timer that it no longer uses. The shell then removes
//! the timer, so the timer does not fire.
//!
//! The timers of the MQTT session are separate. They are not in the log,
//! and [`Shell::advance`] and [`Shell::advance_to_next`] do not fire them.
//! A test of the session fires them with [`Shell::advance_session`]. The
//! shell finds a timer of the session by its duration. Refer to
//! [`session_timers`].
//!
//! The shell logs the stream with the names of the MQTT operations: an
//! `Open` is `MqttConnect`, and each packet in a `Write` is
//! `MqttPublish <topic> <payload>`, `MqttSubscribe` (once for each write)
//! or `MqttDisconnect`. A `Close` is `StreamClose`.
//!
//! The draw timer is short, and the steps of a test are far apart in time.
//! Thus, by default, [`Shell::run`] moves the clock to the next deadline
//! while the core waits for a draw. A test of the draw timer sets
//! [`Shell::auto_draw`] to `false`.
//!
//! The shell answers `FrontLight` at once and keeps the level in
//! [`Shell::light`]. A test of a light that does not answer sets
//! [`Shell::hold_light`].
//!
//! The shell answers each radio request at once with the fake radio
//! ([`Shell::radio`]). It logs `Off` as `WifiOff`, and each other radio
//! request with its name, for example `RadioJoin Cached keep`. A test that
//! needs a request that waits sets [`Shell::hold_radio`]. The shell then
//! keeps each radio request, and obeys the stop rule of the radio
//! capability: a new request stops each request that waits.
//! [`Shell::answer_radio`] answers the requests that wait.
//!
//! The shell keeps each `Log` request in [`Shell::logs`], not in
//! [`Shell::log`], so a line of the log does not change the effects that a
//! test checks. It answers the request at once with the time on its clock
//! in milliseconds, so the core keeps each line with that time. It answers
//! each `LogClock` request at once with the same time, and does not log it.
//! It counts the `LogClock` requests in [`Shell::clocks`]. A test of a late
//! answer sets [`Shell::hold_clock`], and then answers the requests that
//! wait with [`Shell::answer_clock`].
//!
//! At each `Sleep` request, the shell keeps the durations of the timers
//! that wait when the core then waits ([`Shell::timers_at_sleep`]). A timer
//! of the core cannot wake the chip, and light sleep stops it, so the shell
//! panics if a timer waits. Each test that reaches a `Sleep` request thus
//! checks this rule.

use alloc::{format, string::String, vec::Vec};
use core::time::Duration;

use crux_core::{App, Request};
use crux_time::{TimerId, operation::NotifyAfter};

use super::broker::{Broker, publish_of};
use crate::{
    Cmd, Dashboard, Effect, Event, Model, ViewModel,
    capabilities::{
        light::FrontLight,
        log::{Area, Log, LogClock, Uptime},
        power::{Sleep, Wake},
        radio::{
            Address, Join, Joined, Lease, Leave, Off, Restart, Scan, Scanned, StopScan, Target,
        },
        stream::Close,
    },
    fake::FakeRadio,
    mqtt::codec::server::Packet,
};

pub use crate::fake::session_timers;

/// A timer that waits.
struct Timer {
    /// The time on the clock of the shell at which the timer fires.
    due: Duration,
    request: Request<NotifyAfter>,
    /// The timer is a timer of the MQTT session.
    session: bool,
}

/// The message if the answer to a radio request fails.
const ANSWER: &str = "a radio request has one answer";

/// A radio request that waits for its answer. Refer to
/// [`Shell::hold_radio`].
enum Held {
    Join(Request<Join>),
    Address(Request<Address>),
    Leave(Request<Leave>),
    Scan(Request<Scan>),
    StopScan(Request<StopScan>),
    Off(Request<Off>),
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
    reason = "each flag is a separate rule of the test shell"
)]
pub struct Shell {
    app: Dashboard,
    pub model: Model,
    commands: Vec<Cmd>,
    pub broker: Broker,
    /// The fake radio, which answers the radio requests.
    pub radio: FakeRadio,
    /// If `true`, the shell keeps each radio request until
    /// [`Shell::answer_radio`]. A new request stops each request that waits,
    /// as the stop rule of the radio capability tells.
    pub hold_radio: bool,
    /// The radio requests that wait, from the oldest.
    held: Vec<Held>,
    /// The reason of each `Restart` notification, in order.
    pub restarts: Vec<String>,
    /// The area of each `Restart` notification, in order.
    pub restart_areas: Vec<Area>,
    /// Each `Log` request as `{level} {area}: {message}`, in order. For
    /// example `warn wifi: the scan failed, so stay on ...`.
    pub logs: Vec<String>,
    /// The number of `LogClock` requests.
    pub clocks: usize,
    /// If `true`, the shell keeps each `LogClock` request until
    /// [`Shell::answer_clock`].
    pub hold_clock: bool,
    /// If `true`, the shell sets the level of each `FrontLight` request, but
    /// keeps the request until [`Shell::answer_lights`], so the core waits
    /// for the light.
    pub hold_light: bool,
    /// The `FrontLight` requests that the shell did not answer.
    held_lights: Vec<Request<FrontLight>>,
    /// The `LogClock` requests that wait, from the oldest.
    held_clocks: Vec<Request<LogClock>>,
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
    /// For each `Sleep` request, the durations of the timers that wait when
    /// the core waits after the request, the timers of the session
    /// included.
    pub timers_at_sleep: Vec<Vec<Duration>>,
    /// The core sent a `Sleep` request in the current run.
    sleep_sent: bool,
    sleep_request: Option<Request<Sleep>>,
    /// A short name for each effect, in order.
    pub log: Vec<String>,
    /// The duration of each timer that the core cleared while the timer
    /// waited, in order. The timers of the session are not in this list.
    pub cleared: Vec<Duration>,
    /// The `Close` requests, in order.
    pub closes: Vec<Close>,
    /// The packets of each `Write` request, in order.
    pub writes: Vec<Vec<Packet>>,
    /// The number of bytes of each `Write` request, in order.
    pub write_bytes: Vec<usize>,
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
            radio: FakeRadio::default(),
            hold_radio: false,
            held: Vec::new(),
            restarts: Vec::new(),
            restart_areas: Vec::new(),
            logs: Vec::new(),
            clocks: 0,
            hold_clock: false,
            hold_light: false,
            held_lights: Vec::new(),
            held_clocks: Vec::new(),
            timers: Vec::new(),
            now: Duration::ZERO,
            light: 0,
            auto_draw: true,
            frames: Vec::new(),
            drawn_seq: 0,
            render_requested: false,
            sleep: None,
            timers_at_sleep: Vec::new(),
            sleep_sent: false,
            sleep_request: None,
            log: Vec::new(),
            cleared: Vec::new(),
            closes: Vec::new(),
            writes: Vec::new(),
            write_bytes: Vec::new(),
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

    /// Answers the `Sleep` request that waits with `Wake::Timer`, as the
    /// shell does at the check of the power source.
    pub fn check(&mut self) {
        self.wake(Wake::Timer);
    }

    /// Answers the `Sleep` request that waits with `Wake::Other`, as the
    /// shell does after a wake with no cause.
    pub fn stray(&mut self) {
        self.wake(Wake::Other);
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
        while self.model.refresh.draw_waits() {
            let Some(index) = self.next_due(limit, false) else {
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
                // The answer of the broker to a write arrives after the
                // answer to the write.
                if self.broker.deliver_answers() {
                    continue;
                }
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
        if core::mem::take(&mut self.sleep_sent) {
            let timers = self
                .timers
                .iter()
                .map(|timer| Duration::from(timer.request.operation.duration))
                .collect::<Vec<_>>();
            assert!(
                timers.is_empty(),
                "timers of the core wait at a Sleep request: {timers:?}"
            );
            self.timers_at_sleep.push(timers);
        }
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
        self.advance_with(duration, false)
    }

    /// Moves the clock forward by `duration` as [`Shell::advance`] does, but
    /// fires the timers of the MQTT session too.
    pub fn advance_session(&mut self, duration: Duration) -> Vec<Event> {
        self.advance_with(duration, true)
    }

    fn advance_with(&mut self, duration: Duration, session: bool) -> Vec<Event> {
        self.settle();
        let target = self.now + duration;
        let mut fired = Vec::new();
        while let Some(index) = self.next_due(target, session) {
            self.fire(index, target, &mut fired);
        }
        self.set_now(target);
        fired
    }

    /// Moves the clock to the next deadline of a timer that is not a timer
    /// of the session, and fires the timers that are due then. Returns the
    /// events that they sent. If no timer waits, returns no events.
    pub fn advance_to_next(&mut self) -> Vec<Event> {
        let next = self
            .timers
            .iter()
            .filter(|timer| !timer.session)
            .map(|timer| timer.due)
            .min();
        match next {
            Some(due) => self.advance(due.saturating_sub(self.now)),
            None => Vec::new(),
        }
    }

    /// Returns the place of the timer with the earliest deadline at or
    /// before `target`. Of two timers with the same deadline, the timer that
    /// the core started first comes first. A timer of the session counts
    /// only if `session` is `true`.
    fn next_due(&self, target: Duration, session: bool) -> Option<usize> {
        self.timers
            .iter()
            .enumerate()
            .filter(|(_, timer)| timer.due <= target && (session || !timer.session))
            .min_by_key(|(index, timer)| (timer.due, *index))
            .map(|(index, _)| index)
    }

    /// Fires the timer at `index` and answers the effects until the core
    /// waits. A draw that the core then waits for fires only if it is due at
    /// or before `limit`. Adds the events of the fired timers to `fired`.
    fn fire(&mut self, index: usize, limit: Duration, fired: &mut Vec<Event>) {
        let mut timer = self.timers.remove(index);
        // A timer of the session that [`Shell::advance`] skipped fires now.
        self.set_now(timer.due.max(self.now));
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

    /// Returns the number of timers of the MQTT session that wait. A timer
    /// that the core cleared does not wait.
    pub fn session_timer_count(&self) -> usize {
        self.timers.iter().filter(|timer| timer.session).count()
    }

    /// Returns the durations of the timers that wait and are not timers of
    /// the MQTT session, in the order in which the core started them.
    pub fn waiting(&self) -> Vec<Duration> {
        self.timers
            .iter()
            .filter(|timer| !timer.session)
            .map(|timer| Duration::from(timer.request.operation.duration))
            .collect()
    }

    /// Returns the log and clears it, so that a test can check the effects
    /// of the next step.
    pub fn take_log(&mut self) -> Vec<String> {
        core::mem::take(&mut self.log)
    }

    /// Returns the durations of the timers that the core cleared, and
    /// clears the list. Refer to [`Shell::cleared`].
    pub fn take_cleared(&mut self) -> Vec<Duration> {
        core::mem::take(&mut self.cleared)
    }

    /// Returns the time on the clock of the shell in milliseconds, as the
    /// shell answers `Log` and `LogClock`.
    fn uptime(&self) -> Uptime {
        Uptime(u64::try_from(self.now.as_millis()).expect("a short test"))
    }

    /// Keeps the line of a `Log` request in [`Shell::logs`], and answers it
    /// at once with the time on the clock.
    fn log_line(&mut self, mut request: Request<Log>) {
        let log = &request.operation;
        self.logs.push(format!("{} {log}", log.level));
        request
            .resolve(self.uptime())
            .expect("the core waits for the time of the line");
    }

    /// Answers a `LogClock` request at once, or keeps it if
    /// [`Shell::hold_clock`] is `true`.
    fn clock(&mut self, mut request: Request<LogClock>) {
        self.clocks += 1;
        if self.hold_clock {
            self.held_clocks.push(request);
        } else {
            request
                .resolve(self.uptime())
                .expect("the core waits for the time");
        }
    }

    /// Answers the `LogClock` request at `index` in the requests that wait,
    /// with the time on the clock, and answers the effects until the core
    /// waits. Refer to [`Shell::hold_clock`].
    pub fn answer_clock(&mut self, index: usize) {
        let mut request = self.held_clocks.remove(index);
        request
            .resolve(self.uptime())
            .expect("the core waits for the time");
        self.run();
    }

    /// Answers the `FrontLight` requests that wait, in order, and answers
    /// the effects until the core waits. Refer to [`Shell::hold_light`].
    pub fn answer_lights(&mut self) {
        for mut request in core::mem::take(&mut self.held_lights) {
            request.resolve(()).expect("the core waits for the light");
        }
        self.run();
    }

    /// Returns the number of `LogClock` requests that wait.
    pub const fn held_clocks(&self) -> usize {
        self.held_clocks.len()
    }

    /// Returns the lines of the log of the core and clears them. Refer to
    /// [`Shell::logs`].
    pub fn take_logs(&mut self) -> Vec<String> {
        core::mem::take(&mut self.logs)
    }

    /// Keeps the area and the reason of a `Restart` notification.
    fn restart(&mut self, restart: Restart) {
        self.log.push("RadioRestart".into());
        self.restart_areas.push(restart.area);
        self.restarts.push(restart.reason);
    }

    fn handle(&mut self, effect: Effect) {
        match effect {
            Effect::Render(_) => {
                self.log.push("Render".into());
                self.render_requested = true;
            }
            Effect::StreamOpen(request) => {
                self.log.push("MqttConnect".into());
                self.broker.open(request);
            }
            Effect::StreamWrite(request) => {
                self.write_bytes.push(request.operation.bytes.len());
                let packets = self.broker.write(request);
                self.log_packets(&packets);
                self.writes.push(packets);
            }
            Effect::StreamClose(request) => {
                self.log.push("StreamClose".into());
                self.closes.push(request.operation.clone());
                self.broker.close(request);
            }
            Effect::RadioJoin(request) => {
                let join = request.operation;
                let target = match join.target {
                    Target::Cached(_) => "Cached",
                    Target::Strongest => "Strongest",
                };
                let keep = if join.keep { " keep" } else { "" };
                self.log.push(format!("RadioJoin {target}{keep}"));
                self.radio_request(Held::Join(request));
            }
            Effect::RadioAddress(request) => {
                self.log.push("RadioAddress".into());
                self.radio_request(Held::Address(request));
            }
            Effect::RadioLeave(request) => {
                self.log.push("RadioLeave".into());
                self.radio_request(Held::Leave(request));
            }
            Effect::RadioScan(request) => {
                self.log.push("RadioScan".into());
                self.radio_request(Held::Scan(request));
            }
            Effect::RadioStopScan(request) => {
                self.log.push("RadioStopScan".into());
                self.radio_request(Held::StopScan(request));
            }
            Effect::RadioOff(request) => {
                self.log.push("WifiOff".into());
                self.radio_request(Held::Off(request));
            }
            Effect::RadioRestart(request) => self.restart(request.operation),
            Effect::Log(request) => self.log_line(request),
            Effect::LogClock(request) => self.clock(request),
            Effect::Sleep(request) => {
                self.log.push("Sleep".into());
                self.sleep_sent = true;
                self.sleep = Some(request.operation.clone());
                self.sleep_request = Some(request);
            }
            Effect::FrontLight(mut request) => {
                let level = request.operation.level;
                self.log.push(format!("FrontLight {level}"));
                self.light = level;
                if self.hold_light {
                    self.held_lights.push(request);
                } else {
                    request.resolve(()).expect("the core waits for the light");
                }
            }
            Effect::TimeNotifyAfter(request) => {
                let duration = Duration::from(request.operation.duration);
                let session = session_timers().contains(&duration);
                if !session {
                    self.log
                        .push(format!("TimeNotifyAfter {}ms", duration.as_millis()));
                }
                let due = self.now + duration;
                self.timers.push(Timer {
                    due,
                    request,
                    session,
                });
            }
            Effect::TimeClear(mut request) => {
                let id = request.operation.id;
                let session = self
                    .timers
                    .iter()
                    .any(|timer| timer.session && timer.request.operation.id == id);
                if !session {
                    self.log.push("TimeClear".into());
                    self.record_clear(id);
                }
                self.timers.retain(|timer| timer.request.operation.id != id);
                request.resolve(id).expect("the core waits for the clear");
            }
        }
    }

    /// Adds the duration of the timer `id` to [`Shell::cleared`], if the
    /// timer waits.
    fn record_clear(&mut self, id: TimerId) {
        let waits = self
            .timers
            .iter()
            .find(|timer| timer.request.operation.id == id);
        if let Some(timer) = waits {
            self.cleared
                .push(Duration::from(timer.request.operation.duration));
        }
    }

    /// Answers a radio request at once, or keeps it if
    /// [`Shell::hold_radio`] is `true`. A new request first stops each
    /// request that waits, as the stop rule of the radio capability tells.
    fn radio_request(&mut self, request: Held) {
        if !self.hold_radio {
            self.answer(request);
            return;
        }
        for held in core::mem::take(&mut self.held) {
            match held {
                Held::Join(mut join) => join.resolve(Joined::Stopped).expect(ANSWER),
                Held::Address(mut address) => address.resolve(Lease::Stopped).expect(ANSWER),
                Held::Scan(mut scan) => scan.resolve(Scanned::Stopped).expect(ANSWER),
                Held::Leave(mut leave) => leave.resolve(()).expect(ANSWER),
                Held::StopScan(mut stop_scan) => stop_scan.resolve(()).expect(ANSWER),
                Held::Off(mut off) => off.resolve(()).expect(ANSWER),
            }
        }
        self.held.push(request);
    }

    /// Answers the radio requests that wait, in order, with the fake radio,
    /// and answers the effects until the core waits.
    pub fn answer_radio(&mut self) {
        for held in core::mem::take(&mut self.held) {
            self.answer(held);
        }
        self.run();
    }

    /// Returns the number of radio requests that wait for their answers.
    pub const fn held_radio(&self) -> usize {
        self.held.len()
    }

    /// Answers a radio request with the fake radio. After `Off`, the
    /// connection to the broker is gone.
    fn answer(&mut self, request: Held) {
        match request {
            Held::Join(mut request) => {
                let joined = self.radio.join(&request.operation);
                request.resolve(joined).expect(ANSWER);
            }
            Held::Address(mut request) => {
                let lease = self.radio.address(&request.operation);
                request.resolve(lease).expect(ANSWER);
            }
            Held::Leave(mut request) => {
                self.radio.leave(&request.operation);
                request.resolve(()).expect(ANSWER);
            }
            Held::Scan(mut request) => {
                let scanned = self.radio.scan(&request.operation);
                request.resolve(scanned).expect(ANSWER);
            }
            Held::StopScan(mut request) => {
                self.radio.stop_scan(&request.operation);
                request.resolve(()).expect(ANSWER);
            }
            Held::Off(mut request) => {
                self.radio.off(&request.operation);
                self.broker.drop_connection();
                request.resolve(()).expect(ANSWER);
            }
        }
    }

    /// Logs the packets of a `Write` with the names of the MQTT operations.
    fn log_packets(&mut self, packets: &[Packet]) {
        let mut subscribed = false;
        for packet in packets {
            match packet {
                Packet::Publish { .. } => {
                    let publish = publish_of(packet).expect("a publish");
                    let payload = String::from_utf8_lossy(&publish.payload);
                    self.log
                        .push(format!("MqttPublish {} {payload}", publish.topic));
                }
                Packet::Subscribe { .. } if !subscribed => {
                    subscribed = true;
                    self.log.push("MqttSubscribe".into());
                }
                Packet::Disconnect => self.log.push("MqttDisconnect".into()),
                Packet::Connect { .. }
                | Packet::Subscribe { .. }
                | Packet::PubAck(_)
                | Packet::PingReq => {}
            }
        }
    }
}
