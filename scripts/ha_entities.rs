#!/usr/bin/env -S cargo +nightly -Zscript
---
[package]
edition = "2024"

[dependencies]
shared = { path = "../shared" }
---
//! Print the Home Assistant entity ids and attributes of the dashboard.
//!
//! The lists come from `PAGES` in `shared/src/pages.rs`. Run `just ha-entities --help`
//! for the usage.

use shared::pages::{dashboard_attributes, dashboard_entity_ids};

const USAGE: &str = "\
Print the Home Assistant entities that the dashboard shows.

Usage: just ha-entities [--statestream | --seed | --help]

  (no option)    the entity ids, one on each line
  --statestream  the YAML lines for `include: entities:` of `mqtt_statestream`
  --seed         the YAML for `attributes:` and `entities:` of `dashboard_statestream_seed`
  --help         this text

`mqtt_statestream` must have `publish_timestamps: true`, because the dashboard reads
`last_updated`. The seed automation must also publish `last_updated` of each entity,
as plain ISO 8601 text.";

fn main() {
    let ids = dashboard_entity_ids();
    match std::env::args().nth(1).as_deref() {
        None => ids.iter().for_each(|id| println!("{id}")),
        Some("--statestream") => {
            eprintln!("note: `mqtt_statestream` must have `publish_timestamps: true`.");
            ids.iter().for_each(|id| println!("      - {id}"));
        }
        Some("--seed") => {
            eprintln!(
                "note: the automation must also publish `last_updated` as plain ISO 8601 text."
            );
            println!("    attributes:");
            dashboard_attributes()
                .iter()
                .for_each(|name| println!("    - {name}"));
            println!("    entities:");
            ids.iter().for_each(|id| println!("    - {id}"));
        }
        Some("--help" | "-h") => println!("{USAGE}"),
        Some(other) => {
            eprintln!("unknown option: {other}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}
