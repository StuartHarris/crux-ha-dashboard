//! The hardware checks for the first start of a board.
//!
//! The steps run in sequence, and each step writes its result to the log:
//!
//! 1. The heap: the size of the internal heap and of the PSRAM heap.
//! 2. The I2C bus: the addresses that answer.
//! 3. The panel: a clear, then 16 grey bars with a full refresh.
//! 4. A partial refresh: a black rectangle with the fast waveform.
//! 5. A page of the dashboard, from the core, with a full refresh.
//! 6. Touch: for 30 seconds, the raw and the panel point of each tap.
//! 7. Wi-Fi: a join and an address from DHCP.
//!
//! The MQTT checks use the `dashboard` binary with `read_only = true`.

#![no_std]
#![no_main]
#![allow(clippy::future_not_send)]

extern crate alloc;

use dashboard_firmware::{
    board, network,
    panel::{Area, GT911_ADDR_LOW, PCA9535_ADDR, Panel, TPS65185_ADDR},
    panel_pins,
};
use embassy_executor::Spawner;
use embassy_net::StackResources;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use embedded_graphics::{
    pixelcolor::Gray4,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
};
use esp_backtrace as _;
use esp_radio::wifi::WifiController;
use logic::{
    shell::{Job, Shell},
    touch::{Sample, Tapper, TouchMap},
};
use shared::{Event, PowerSource};
use static_cell::StaticCell;
use ui::Framebuffer;

esp_bootloader_esp_idf::esp_app_desc!();

/// The build-time configuration, which `build.rs` reads from `cfg.toml`.
mod config {
    include!(concat!(env!("OUT_DIR"), "/config.rs"));
}

/// The memory of the network stack.
static RESOURCES: StaticCell<StackResources<2>> = StaticCell::new();

/// The I2C devices of the board: the IO expander, the RTC, the fuel gauge,
/// the touch controller, the PMIC of the panel and the charger.
const EXPECTED: [(u8, &str); 6] = [
    (PCA9535_ADDR, "PCA9535 IO expander"),
    (0x51, "PCF8563 RTC"),
    (0x55, "BQ27220 fuel gauge"),
    (GT911_ADDR_LOW, "GT911 touch"),
    (TPS65185_ADDR, "TPS65185 panel PMIC"),
    (0x6B, "BQ25896 charger"),
];

/// The time of the touch step.
const TOUCH_STEP: Duration = Duration::from_secs(30);
/// The pause between two steps, to read the log and look at the panel.
const PAUSE: Duration = Duration::from_secs(3);

#[esp_hal::main]
async fn main(spawner: Spawner) -> ! {
    let peripherals = board::init();
    board::start(
        peripherals.PSRAM,
        peripherals.TIMG0,
        peripherals.FROM_CPU_INTR0,
    );

    log::info!("step 1: heap: {}", esp_alloc::HEAP.stats());

    let mut panel = match Panel::new(
        panel_pins!(peripherals),
        peripherals.I2C0,
        peripherals.DMA_CH0,
        peripherals.LCD_CAM,
        peripherals.RMT,
    ) {
        Ok(panel) => panel,
        Err(error) => {
            log::error!("the panel did not start: {error:?}");
            idle().await
        }
    };

    // The reset of the GT911 sets its address, and the PMIC answers only when
    // the panel power is on.
    let resolution = panel.init_touch().await;
    if let Err(error) = panel.power_on() {
        log::error!("step 2: panel power on failed: {error:?}");
    }
    scan(&mut panel);
    Timer::after(PAUSE).await;

    log::info!("step 3: clear and 16 grey bars");
    let mut frame = Framebuffer::new();
    for level in 0..16u8 {
        let bar = Rectangle::new(Point::new(i32::from(level) * 60, 0), Size::new(60, 540));
        let Ok(()) = bar
            .into_styled(PrimitiveStyle::with_fill(Gray4::new(level)))
            .draw(&mut frame);
    }
    timed("full refresh", panel.full(&frame)).await;
    Timer::after(PAUSE).await;

    log::info!("step 4: a black rectangle with a partial refresh");
    let shown = frame.clone();
    let area = Rectangle::new(Point::new(380, 200), Size::new(200, 140));
    let Ok(()) = area
        .into_styled(PrimitiveStyle::with_fill(Gray4::BLACK))
        .draw(&mut frame);
    let partial = Area {
        x: 380,
        y: 200,
        width: 200,
        height: 140,
    };
    timed("partial refresh", panel.partial(&shown, &frame, partial)).await;
    Timer::after(PAUSE).await;

    log::info!("step 5: a page of the dashboard");
    let mut shell = Shell::new(|| crux_time::Instant::new(0, 0));
    // A dry run, so the check publishes no service call.
    let dry_run = logic::config::Config {
        read_only: true,
        ..config::CONFIG
    };
    let start = logic::shell::start(&dry_run, PowerSource::Stand, None, None);
    let view = shell
        .send(Event::Start(start))
        .into_iter()
        .find_map(|job| match job {
            Job::Render(view) => Some(*view),
            _ => None,
        })
        .unwrap_or_else(|| shell.view());
    log::info!("the page is {} ({})", view.title, view.path);
    let Ok(()) = ui::draw(&view, &mut frame);
    timed("full refresh", panel.full(&frame)).await;
    if let Err(error) = panel.power_off() {
        log::error!("panel power off failed: {error:?}");
    }
    Timer::after(PAUSE).await;

    log::info!(
        "step 6: touch for {} s. Tap the corners and the tiles.",
        TOUCH_STEP.as_secs()
    );
    match resolution {
        Ok(resolution) => {
            log::info!("touch resolution {resolution:?}");
            touch(&mut panel, TouchMap::new(resolution), &view).await;
        }
        Err(error) => log::error!("the GT911 does not answer: {error:?}"),
    }

    log::info!("step 7: Wi-Fi join");
    let mut controller = WifiController::new(
        peripherals.WIFI,
        network::controller_config(&config::CONFIG),
    )
    .expect("start the Wi-Fi driver");
    let rng = esp_hal::rng::Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    let (stack, runner) = embassy_net::new(
        esp_radio::wifi::Interface::station(),
        embassy_net::Config::dhcpv4(embassy_net::DhcpConfig::default()),
        RESOURCES.init(StackResources::new()),
        seed,
    );
    spawner.spawn(net_task(runner).expect("spawn the network task"));
    match with_timeout(Duration::from_secs(20), controller.connect_async()).await {
        Ok(Ok(info)) => log::info!("joined: {info:?}"),
        Ok(Err(error)) => log::error!("join failed: {error:?}"),
        Err(_) => log::error!("join timed out"),
    }
    if with_timeout(Duration::from_secs(20), stack.wait_config_up())
        .await
        .is_ok()
    {
        if let Some(config) = stack.config_v4() {
            log::info!("address {}", config.address);
        }
    } else {
        log::error!("no address from DHCP");
    }

    log::info!("bring-up done. Next: `just flash` for the dashboard.");
    idle().await
}

/// Writes the I2C addresses that answer, and the devices that are missing.
fn scan(panel: &mut Panel<'_>) {
    let mut found = [0u8; 16];
    let count = panel.scan(&mut found);
    let found = &found[..count];
    log::info!("step 2: I2C addresses: {found:02X?}");
    for (address, name) in EXPECTED {
        if found.contains(&address) {
            log::info!("  found 0x{address:02X} {name}");
        } else {
            log::warn!("  missing 0x{address:02X} {name}");
        }
    }
}

/// Runs a refresh and writes its time to the log.
async fn timed(name: &str, refresh: impl Future<Output = dashboard_firmware::panel::Result<()>>) {
    let started = Instant::now();
    match refresh.await {
        Ok(()) => log::info!(
            "{name} in {} ms",
            Instant::now().duration_since(started).as_millis()
        ),
        Err(error) => log::error!("{name} failed: {error:?}"),
    }
}

/// Writes each tap, with the event that it gives on the page.
async fn touch(panel: &mut Panel<'_>, map: TouchMap, view: &shared::ViewModel) {
    let mut tapper = Tapper::default();
    let end = Instant::now() + TOUCH_STEP;
    while Instant::now() < end {
        match panel.touch_sample() {
            Ok(sample) => {
                if sample == Sample::Released {
                    log::info!("release");
                }
                let point = sample.map(|raw| map.to_panel(raw));
                if let (Sample::Pressed(raw), Some(tap)) =
                    (sample, tapper.sample(Instant::now().as_millis(), point))
                {
                    let control = ui::hit_test(view, tap);
                    log::info!(
                        "tap raw {raw:?} panel ({}, {}) -> {control:?}",
                        tap.x,
                        tap.y
                    );
                }
            }
            Err(error) => log::warn!("touch read failed: {error:?}"),
        }
        Timer::after(Duration::from_millis(30)).await;
    }
}

async fn idle() -> ! {
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, esp_radio::wifi::Interface>) {
    runner.run().await;
}
