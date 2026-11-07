use alloc::string::String;

use crux_core::{
    App, Command,
    macros::effect,
    render::{RenderOperation, render},
};
use facet::Facet;
use serde::{Deserialize, Serialize};

/// The events that the shells send to the core.
#[derive(Facet, Serialize, Deserialize, Clone, Debug)]
#[repr(C)]
pub enum Event {
    /// The shell has started.
    Start,
}

#[effect]
#[derive(Debug)]
pub enum Effect {
    Render(RenderOperation),
}

#[derive(Default)]
pub struct Model {
    started: bool,
}

/// The data that a shell needs to draw the screen.
#[derive(Facet, Serialize, Deserialize, Clone, Default, Debug, PartialEq, Eq)]
pub struct ViewModel {
    pub title: String,
    pub status: String,
}

#[derive(Default)]
pub struct Dashboard;

impl App for Dashboard {
    type Event = Event;
    type Model = Model;
    type ViewModel = ViewModel;
    type Effect = Effect;

    fn update(&self, event: Event, model: &mut Model) -> Command<Effect, Event> {
        match event {
            Event::Start => model.started = true,
        }

        render()
    }

    fn view(&self, model: &Model) -> ViewModel {
        ViewModel {
            title: "Home".into(),
            status: if model.started {
                "Started"
            } else {
                "Not started"
            }
            .into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_renders() {
        let app = Dashboard;
        let mut model = Model::default();

        app.update(Event::Start, &mut model).expect_only_render();

        assert_eq!(app.view(&model).status, "Started");
    }
}
