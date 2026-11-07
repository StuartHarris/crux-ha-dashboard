//! The rooms feature: the current page, page changes, and taps on tiles and
//! scenes.

use serde::Serialize;

use crate::{
    Cmd, Model,
    app::redraw,
    capabilities::mqtt::{Publish, Qos},
    features::{connection, power},
    route::Route,
};

/// The JSON payload of a service call for Home Assistant.
#[derive(Serialize)]
struct ServiceCall<'a> {
    service: &'a str,
    entity_id: &'a str,
}

/// Shows the page at `route`. The core ignores a route that has no page.
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

/// Shows the next or the previous page. The pages wrap around.
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
/// tile does nothing.
pub fn tile_tapped(index: usize, model: &mut Model) -> Cmd {
    if model.power.asleep {
        return Cmd::done();
    }
    let idle = power::restart_idle_timer(model);
    let Some(tile) = model.current_page().and_then(|page| page.tiles.get(index)) else {
        return idle;
    };
    let Some(service) = tile.kind.tap_service() else {
        return idle;
    };
    call_service(model, service, tile.entity_id).and(idle)
}

/// Turns on the scene.
pub fn scene_tapped(index: usize, model: &mut Model) -> Cmd {
    if model.power.asleep {
        return Cmd::done();
    }
    let idle = power::restart_idle_timer(model);
    let Some(scene) = model.current_page().and_then(|page| page.scenes.get(index)) else {
        return idle;
    };
    call_service(model, "scene.turn_on", scene.entity_id).and(idle)
}

/// Publishes a service call to Home Assistant.
fn call_service(model: &mut Model, service: &str, entity_id: &str) -> Cmd {
    let Some(settings) = &model.settings else {
        return Cmd::done();
    };
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
