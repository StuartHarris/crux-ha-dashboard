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

## Licence

The crates in this repository have the licence MIT or Apache-2.0, at your option.
Refer to [`LICENSE-MIT`](LICENSE-MIT) and [`LICENSE-APACHE`](LICENSE-APACHE).

The `dashboard-firmware` crate in `firmware/` is the exception. Its licence is
GPL-3.0-only, because its `panel` module is a copy of part of the GPL crate
`lilygo-t5s3paperpro`. The licence text is in
[`firmware/src/panel/LICENSE`](firmware/src/panel/LICENSE). For more detail, refer to
the section "The panel driver" in [`firmware/README.md`](firmware/README.md). The
`firmware-logic` crate in `firmware/logic` has no GPL code, so it has the licence MIT or
Apache-2.0.

Unless you explicitly state otherwise, any contribution that you intentionally submit
for inclusion in the work, as the Apache-2.0 licence defines it, has the licence MIT or
Apache-2.0, as above, with no additional terms or conditions. This applies to all crates
except `dashboard-firmware`.
