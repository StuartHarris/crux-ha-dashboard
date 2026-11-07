# crux-ha-dashboard

A touchscreen dashboard for Home Assistant, written in Rust with
[Crux](https://github.com/redbadger/crux).

We develop the dashboard off the device first. Then we deploy it to a LilyGo
board as `no_std` firmware. The Crux core holds the app logic. Each target has
its own shell, which renders the view model and runs the effects.
