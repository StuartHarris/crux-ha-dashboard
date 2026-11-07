//! The timer task: it waits for the timers of the core and sends
//! `Signal::Timer` when one expires. `logic::timers` keeps the timers.

use core::time::Duration;

use crux_time::TimerId;
use embassy_futures::select::{Either, select};
use embassy_time::{Instant, Timer};
use logic::timers::Timers;

use crate::{SIGNALS, TIMERS};

/// Work for the timer task.
pub enum Request {
    /// Start a timer.
    Start {
        id: TimerId,
        token: u32,
        after: Duration,
    },
    /// Stop the timer with this token.
    Cancel { token: u32 },
}

#[embassy_executor::task]
pub async fn task() {
    let mut timers = Timers::default();
    loop {
        let request = match timers.next_due() {
            Some(due) => match select(TIMERS.pop(), Timer::at(Instant::from_millis(due))).await {
                Either::First(request) => Some(request),
                Either::Second(()) => None,
            },
            None => Some(TIMERS.pop().await),
        };
        let now = Instant::now().as_millis();
        match request {
            Some(Request::Start { id, token, after }) => {
                let after = u64::try_from(after.as_millis()).unwrap_or(u64::MAX);
                timers.start(id, token, now.saturating_add(after));
            }
            Some(Request::Cancel { token }) => timers.cancel(token),
            None => {}
        }
        for signal in timers.expire(now) {
            SIGNALS.push(signal);
        }
    }
}
