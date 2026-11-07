//! The desktop simulator of the dashboard.
//!
//! It runs the core and draws the panel with the same `ui` code as the
//! firmware. It has three modes:
//!
//! - A window, connected to the MQTT broker. A click is a touch.
//! - `--png <FILE>`: draws a page with fixed entity states to a PNG file,
//!   with no window and no broker.
//! - `--smoke <FILE>`: connects to the broker, waits for the entity states,
//!   draws the page to a PNG file and stops.
//!
//! Run `sim --help` for the options.

mod mqtt;
mod panel;
mod png;
mod settings;
mod shell;
mod smoke;
mod window;

use std::{fs, io, path::Path, process::ExitCode};

use settings::{Mode, USAGE};

fn main() -> ExitCode {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let options = match settings::parse(
        std::env::args().skip(1),
        |name| std::env::var(name).ok(),
        home.as_deref(),
        read_password,
    ) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    let result = match options.mode.clone() {
        Mode::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Mode::Png(out) => png::run(options.path, &out),
        Mode::Smoke(out) => smoke::run(options, &out),
        Mode::Window => window::run(options),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// Reads the password file. Warns if other users can read the file.
fn read_password(path: &Path) -> io::Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            eprintln!(
                "warning: other users can read {}. Run: chmod 600 {}",
                path.display(),
                path.display()
            );
        }
    }
    fs::read_to_string(path)
}
