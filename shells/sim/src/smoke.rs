//! Connects to the broker, waits for the retained entity states, draws the
//! page to a PNG file and stops.
//!
//! The broker sends the retained states after the SUBACK packet, so the
//! smoke test stops when no bytes arrive for [`QUIET`] after the SUBACK
//! packet.
//!
//! The core does a dry run in this mode, so it publishes no service call.

use std::{
    path::Path,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use shared::{Connection, Event, Settings, capabilities::stream::StreamItem};

use crate::{
    inspector::Direction,
    panel::Panel,
    settings::Options,
    shell::{Notifier, Shell, Signal, reading},
    stream,
};

/// The time with no bytes from the broker, after the SUBACK packet, after
/// which the shell has the retained states.
const QUIET: Duration = Duration::from_millis(300);
/// The longest time to wait.
const LIMIT: Duration = Duration::from_secs(20);

/// Runs the smoke test and writes the page to `out`.
///
/// # Errors
///
/// Returns a message if the shell got no states before the time limit, or if
/// the file cannot be written. If the time limit expires, the shell writes
/// the page first.
pub fn run(options: Options, out: &Path) -> Result<(), String> {
    let runtime = tokio::runtime::Runtime::new().map_err(|error| error.to_string())?;
    let (sender, signals) = mpsc::channel();
    let notifier = Notifier::new(sender, Arc::new(|| {}));
    let mut shell = Shell::new(runtime.handle().clone(), notifier, stream::tcp_opener());
    let settings = Settings {
        read_only: true,
        ..options.settings
    };
    shell.start(settings, options.power, options.path);
    shell.send(Event::BatteryChanged(Some(options.level)));
    shell.send(Event::PowerRead(reading(options.power)));

    let started = Instant::now();
    // The time of the last bytes from the broker, or of the SUBACK packet if
    // no bytes arrived after it.
    let mut last_bytes = Instant::now();
    let mut quiet = false;
    let complete = loop {
        if let Ok(signal) = signals.recv_timeout(Duration::from_millis(50)) {
            if matches!(
                signal,
                Signal::Stream {
                    item: StreamItem::Received(_),
                    ..
                }
            ) {
                last_bytes = Instant::now();
            }
            shell.handle(signal);
        }
        if !quiet && shell.suback_count() > 0 {
            quiet = true;
            last_bytes = last_bytes.max(Instant::now());
        }
        let online = shell.view().connection == Connection::Online;
        if online && quiet && last_bytes.elapsed() >= QUIET {
            break true;
        }
        if started.elapsed() >= LIMIT {
            break false;
        }
    };

    // The core does a dry run, so it must not ask for a service call.
    let calls = shell
        .inspector()
        .mqtt
        .iter()
        .filter(|entry| entry.direction == Direction::Out && entry.topic.ends_with("/cmd"))
        .count();

    let messages = shell.inspector().messages;
    let view = shell.view();
    let mut panel = Panel::new();
    panel.draw(&view);
    panel.save_png(out)?;
    eprintln!(
        "smoke: {messages} messages, page {} ({}), wrote {}",
        view.page_label,
        view.title,
        out.display()
    );
    let header = view
        .index()
        .into_iter()
        .flat_map(|index| [&index.weather, &index.all_lights])
        .flatten();
    for tile in header.chain(view.tiles()) {
        eprintln!(
            "smoke:   {}: {:?} {} {}",
            tile.label, tile.state, tile.text, tile.detail
        );
    }
    for room in view.index().iter().flat_map(|index| &index.rooms) {
        eprintln!(
            "smoke:   {}: {:?} {} (any on: {})",
            room.title,
            room.lights,
            room.temperature.as_deref().unwrap_or("-"),
            room.any_on
        );
    }
    // Stop with no DISCONNECT packet. The broker publishes the last will.
    drop(shell);
    runtime.shutdown_background();
    if calls > 0 {
        return Err(format!("smoke: the core published {calls} service calls"));
    }
    eprintln!("smoke: no service call published");
    if complete {
        Ok(())
    } else {
        Err(format!(
            "smoke: no complete state within {} seconds",
            LIMIT.as_secs()
        ))
    }
}
