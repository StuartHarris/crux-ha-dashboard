//! The rooms feature: the current page, page changes, and taps on tiles,
//! scenes and the controls of the index page.

use serde::Serialize;

use crate::{
    Cmd, Control, Model,
    capabilities::mqtt::{Publish, Qos},
    features::{
        connection::{self, Call},
        pending::{self, Confirm},
        power,
        refresh::redraw,
    },
    layout::{BRIGHTNESS_STEP_PCT, Page},
    route::Route,
    view::light_percent,
};

/// The JSON payload of a service call for Home Assistant.
///
/// The payload has `brightness_step_pct` only for a step of a dimmable
/// light. Refer to `docs/mqtt.md`.
#[derive(Serialize)]
struct ServiceCall<'a> {
    service: &'a str,
    entity_id: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    brightness_step_pct: Option<i8>,
}

impl<'a> ServiceCall<'a> {
    /// Returns a service call with no data.
    const fn new(service: &'a str, entity_id: &'static str) -> Self {
        Self {
            service,
            entity_id,
            brightness_step_pct: None,
        }
    }
}

/// Acts on a tap on `control`, or on no control. Refer to
/// [`Event::Tapped`](crate::Event::Tapped).
pub fn tapped(control: Option<Control>, model: &mut Model) -> Cmd {
    if model.power.asleep {
        // The tap wakes the device, then acts on its control. A service call
        // waits in the outbox until the connection is open.
        let wake = power::wake_up(model);
        return wake.and(tapped(control, model));
    }
    match control {
        None => Cmd::done(),
        Some(Control::PreviousPage) => turn_page(model, false),
        Some(Control::Home) => navigate(Route::Home, model),
        Some(Control::NextPage) => turn_page(model, true),
        Some(Control::Tile(index)) => tile_tapped(index, model),
        Some(Control::Scene(index)) => scene_tapped(index, model),
        Some(Control::AllLights) => all_lights_tapped(model),
        Some(Control::Room(index)) => room_tapped(index, model),
        Some(Control::Brighter(index)) => step_tapped(index, BRIGHTNESS_STEP_PCT, model),
        Some(Control::Dimmer(index)) => step_tapped(index, -BRIGHTNESS_STEP_PCT, model),
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

/// Returns the current page if it is the page of a room, or `None` on the
/// home page.
fn current_room(model: &Model) -> Option<&'static Page> {
    model.current_page().filter(|page| page.is_room())
}

/// Toggles the light or the switch of the tile. A tap on another kind of
/// tile only starts the idle time again. The device is awake.
///
/// The index page has no tiles, so the core ignores a tile on the home page.
fn tile_tapped(index: usize, model: &mut Model) -> Cmd {
    let idle = power::restart_idle_timer(model);
    let Some(tile) = current_room(model).and_then(|page| page.tiles.get(index)) else {
        return idle;
    };
    let Some(service) = tile.kind.tap_service() else {
        return idle;
    };
    let call = ServiceCall::new(service, tile.entity_id);
    call_service(model, &call, Confirm::Toggle).and(idle)
}

/// Changes the brightness of the light of the tile by `step` percent. The
/// device is awake.
///
/// The core acts only if the tile has the − and + buttons now. Refer to
/// [`Tile::has_steps`](crate::layout::Tile::has_steps). The core also
/// ignores a − step on a light that is off and a + step on a light that is
/// on at 100%, because Home Assistant does nothing for them. The core reads
/// the brightness as the tile shows it. If the core does not act, the tap
/// only starts the idle time again. It sends no call and gives no mark.
///
/// The core sends `light.turn_on` with `brightness_step_pct`. If a light is
/// off, a + step turns it on at that brightness. Home Assistant turns a light
/// off when its brightness gets to 0. The tile shows the new brightness when
/// it comes back on the state stream.
fn step_tapped(index: usize, step: i8, model: &mut Model) -> Cmd {
    let idle = power::restart_idle_timer(model);
    let Some(tile) = current_room(model).and_then(|page| page.tiles.get(index)) else {
        return idle;
    };
    let state = model.entities.state(tile.entity_id);
    let full = model.entities.get(tile.entity_id).and_then(light_percent) == Some(100);
    let no_change = match state {
        Some("off") => step < 0,
        Some("on") => step > 0 && full,
        _ => false,
    };
    if !tile.has_steps(state) || no_change {
        return idle;
    }
    let call = ServiceCall {
        brightness_step_pct: Some(step),
        ..ServiceCall::new("light.turn_on", tile.entity_id)
    };
    call_service(model, &call, Confirm::Step).and(idle)
}

/// Turns on the scene. The device is awake.
fn scene_tapped(index: usize, model: &mut Model) -> Cmd {
    let idle = power::restart_idle_timer(model);
    let Some(scene) = current_room(model).and_then(|page| page.scenes.get(index)) else {
        return idle;
    };
    let call = ServiceCall::new("scene.turn_on", scene.entity_id);
    call_service(model, &call, Confirm::Scene).and(idle)
}

/// Toggles the All lights tile of the home page: its first light or switch
/// tile. The core acts only on the home page. The device is awake.
fn all_lights_tapped(model: &mut Model) -> Cmd {
    let idle = power::restart_idle_timer(model);
    let tile = model.current_page().and_then(Page::all_lights_tile);
    let Some((tile, service)) =
        tile.and_then(|tile| tile.kind.tap_service().map(|service| (tile, service)))
    else {
        return idle;
    };
    let call = ServiceCall::new(service, tile.entity_id);
    call_service(model, &call, Confirm::Toggle).and(idle)
}

/// Shows the page of the room with `index` in the list of rooms. The core
/// acts only on the home page, because only the index page has room
/// buttons. The device is awake.
fn room_tapped(index: usize, model: &mut Model) -> Cmd {
    let on_index = model.current_page().is_some_and(Page::is_home);
    match model.rooms().nth(index) {
        Some(room) if on_index => navigate(room.route(), model),
        _ => power::restart_idle_timer(model),
    }
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

/// Publishes a service call to Home Assistant, and gives its entity a mark
/// until Home Assistant confirms it. `confirm` tells how Home Assistant
/// confirms the call. Refer to [`pending`].
///
/// In a dry run, the core sends nothing, keeps nothing in the outbox and
/// gives no mark. The panel shows no sign of the tap, because the state of
/// the entity does not change. The view model tells the shell that the
/// dashboard does a dry run, so the shell can show it.
fn call_service(model: &mut Model, call: &ServiceCall<'_>, confirm: Confirm) -> Cmd {
    let Some(settings) = &model.settings else {
        return Cmd::done();
    };
    if settings.read_only {
        return Cmd::done();
    }
    let Ok(payload) = serde_json::to_vec(call) else {
        return Cmd::done();
    };
    let publish = Publish {
        topic: settings.command_topic(),
        payload,
        qos: Qos::AtLeastOnce,
        retain: false,
    };
    pending::mark(model, call.entity_id, confirm);
    let call = Call {
        publish,
        entity_id: call.entity_id,
    };
    connection::send(call, model).and(redraw(model, false))
}
