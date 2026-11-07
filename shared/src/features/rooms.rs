//! The rooms feature: the current page, page changes, and taps on tiles and
//! scenes.

use serde::Serialize;

use crate::{
    Cmd, Control, Model,
    capabilities::mqtt::{Publish, Qos},
    features::{connection, power, refresh::redraw},
    route::Route,
};

/// The JSON payload of a service call for Home Assistant.
#[derive(Serialize)]
struct ServiceCall<'a> {
    service: &'a str,
    entity_id: &'a str,
}

/// Acts on a tap on `control`, or on no control. Refer to
/// [`Event::Tapped`](crate::Event::Tapped).
pub fn tapped(control: Option<Control>, model: &mut Model) -> Cmd {
    if model.power.asleep {
        return power::woke(model);
    }
    match control {
        None => Cmd::done(),
        Some(Control::PreviousPage) => turn_page(model, false),
        Some(Control::Home) => navigate(Route::Home, model),
        Some(Control::NextPage) => turn_page(model, true),
        Some(Control::Tile(index)) => tile_tapped(index, model),
        Some(Control::Scene(index)) => scene_tapped(index, model),
    }
}

/// Shows the page at `route`. The core ignores a route that has no page,
/// and it ignores the event while the device sleeps.
pub fn navigate(route: Route, model: &mut Model) -> Cmd {
    if model.power.asleep || !model.pages.iter().any(|page| page.is_at(&route)) {
        return Cmd::done();
    }
    let idle = power::restart_idle_timer(model);
    if route == model.route {
        return idle;
    }
    model.route = route;
    redraw(model, true).and(idle)
}

/// Shows the next or the previous page. The pages wrap around. The core
/// ignores the event while the device sleeps.
pub fn turn_page(model: &mut Model, forward: bool) -> Cmd {
    let count = model.pages.len();
    if model.power.asleep || count == 0 {
        return Cmd::done();
    }
    let index = model.page_index();
    let next = if forward {
        (index + 1) % count
    } else {
        (index + count - 1) % count
    };
    model.route = model.pages[next].route();
    redraw(model, true).and(power::restart_idle_timer(model))
}

/// Toggles the light or the switch of the tile. A tap on another kind of
/// tile only starts the idle time again. The device is awake.
fn tile_tapped(index: usize, model: &mut Model) -> Cmd {
    let idle = power::restart_idle_timer(model);
    let Some(tile) = model.current_page().and_then(|page| page.tiles.get(index)) else {
        return idle;
    };
    let Some(service) = tile.kind.tap_service() else {
        return idle;
    };
    call_service(model, service, tile.entity_id).and(idle)
}

/// Turns on the scene. The device is awake.
fn scene_tapped(index: usize, model: &mut Model) -> Cmd {
    let idle = power::restart_idle_timer(model);
    let Some(scene) = model.current_page().and_then(|page| page.scenes.get(index)) else {
        return idle;
    };
    call_service(model, "scene.turn_on", scene.entity_id).and(idle)
}

/// Starts or stops a dry run. The core keeps the value until the next
/// `Start`, which gives the value of the shell again.
///
/// Before `Start`, the core keeps the request. At `Start`, the dashboard does
/// a dry run if the settings or the request ask for one. The core draws the
/// change only while the device is awake. The next wake shows it.
pub fn set_read_only(read_only: bool, model: &mut Model) -> Cmd {
    let Some(settings) = &mut model.settings else {
        model.read_only_requested = read_only;
        return Cmd::done();
    };
    if settings.read_only == read_only {
        return Cmd::done();
    }
    settings.read_only = read_only;
    if model.power.asleep {
        Cmd::done()
    } else {
        redraw(model, false)
    }
}

/// Publishes a service call to Home Assistant.
///
/// In a dry run, the core sends nothing and keeps nothing in the outbox.
/// The panel shows no sign of the tap, because the state of the entity does
/// not change. The view model tells the shell that the dashboard does a dry
/// run, so the shell can show it.
fn call_service(model: &mut Model, service: &str, entity_id: &str) -> Cmd {
    let Some(settings) = &model.settings else {
        return Cmd::done();
    };
    if settings.read_only {
        return Cmd::done();
    }
    let Ok(payload) = serde_json::to_vec(&ServiceCall { service, entity_id }) else {
        return Cmd::done();
    };
    let publish = Publish {
        topic: settings.command_topic(),
        payload,
        qos: Qos::AtLeastOnce,
        retain: false,
    };
    connection::send(publish, model)
}
