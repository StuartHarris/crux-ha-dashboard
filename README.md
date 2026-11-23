# crux-ha-dashboard

A touchscreen dashboard for Home Assistant, written in Rust with
[Crux](https://github.com/redbadger/crux).

We develop the dashboard off the device first. Then we deploy it to a LilyGo
board as `no_std` firmware. The Crux core holds the app logic. Each target has
its own shell, which renders the view model and runs the effects.

For the parts of the system, the core, the shells and the tests, refer to
[`docs/architecture.md`](docs/architecture.md). For the MQTT topics, refer to
[`docs/mqtt.md`](docs/mqtt.md).

The dashboard shows each time as UK time (Europe/London), with British Summer
Time. The core calculates the offset from UTC, so no shell has a time zone
setting.
