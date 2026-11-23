//! The light on the wire: the payloads of Home Assistant, the state
//! payload, and the light publisher of the device publish queue. Refer to
//! `docs/mqtt.md`.

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::{Command, Level, Percent, Sync, TapLevel};
use crate::{
    Model,
    features::device::{Item, Next},
    mqtt::{PacketId, Publish, Qos},
};

/// A command of Home Assistant on `set/light`, as JSON. The core ignores
/// the other keys, for example `transition` and `flash`.
#[derive(Deserialize)]
struct CommandPayload<'a> {
    /// `ON` or `OFF`.
    state: &'a str,
    /// The brightness in percent, because the light entity has the
    /// brightness scale 100.
    brightness: Option<u32>,
}

/// Returns the command of the JSON `payload` of `set/light`, or `None` if
/// the payload is not valid.
///
/// The payload is `{"state":"ON"}`, `{"state":"ON","brightness":40}` or
/// `{"state":"OFF"}`. A brightness of 0 is OFF. A brightness more than 100
/// is not valid. The core ignores a brightness in an OFF command.
#[must_use]
pub fn command(payload: &[u8]) -> Option<Command> {
    let command: CommandPayload<'_> = serde_json::from_slice(payload).ok()?;
    match command.state {
        "OFF" => Some(Command::Off),
        "ON" => match command.brightness {
            None => Some(Command::On(None)),
            Some(0) => Some(Command::Off),
            Some(brightness) => {
                let level = Level::new(u8::try_from(brightness).ok()?)?;
                Some(Command::On(Some(level)))
            }
        },
        _ => None,
    }
}

/// Returns the tap level of the `payload` of a number of Home Assistant, or
/// `None` if the payload is not valid.
///
/// Home Assistant sends a whole number as an integer, for example `40`. The
/// core also accepts `40.0`. It rejects a fraction, a negative value and a
/// value more than 100. An empty payload is [`TapLevel::Default`]: a user
/// cleared the retained message.
#[must_use]
pub fn tap_level(payload: &[u8]) -> Option<TapLevel> {
    let text = core::str::from_utf8(payload).ok()?.trim();
    if text.is_empty() {
        return Some(TapLevel::Default);
    }
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !fraction.bytes().all(|byte| byte == b'0') {
        return None;
    }
    let percent = whole.parse::<u8>().ok().and_then(Percent::new)?;
    Some(TapLevel::Set(percent))
}

/// The JSON state of the light.
#[derive(Serialize)]
struct StatePayload {
    /// `ON` or `OFF`.
    state: &'static str,
    /// The level in percent. The OFF state has no brightness, because Home
    /// Assistant ignores the brightness 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    brightness: Option<u8>,
}

/// Returns the state payload of the light at `level` (`None` is OFF), for
/// example `{"state":"ON","brightness":40}` or `{"state":"OFF"}`.
#[must_use]
pub fn state_json(level: Option<Level>) -> Vec<u8> {
    let payload = StatePayload {
        state: if level.is_some() { "ON" } else { "OFF" },
        brightness: level.map(Level::get),
    };
    // The fields are a number and text, and the writer is a `Vec`, so the
    // serialization cannot fail.
    serde_json::to_vec(&payload).expect("a payload of a number and text serializes")
}

/// Returns the state message of the light if it is due: on a new session,
/// after a command of Home Assistant, or if the light is not the state that
/// the broker acknowledged. While a state message waits for its PUBACK, and
/// during a dry run, returns nothing. Refer to [`Sync`].
pub fn next(model: &mut Model) -> Next {
    let now = model.light.lit();
    let Some(settings) = model
        .startup
        .settings()
        .filter(|settings| !settings.read_only)
    else {
        return Next::Nothing;
    };
    let Some(live) = model.connection.live_mut() else {
        return Next::Nothing;
    };
    match live.light {
        Sync::InFlight { .. } => return Next::Nothing,
        Sync::Sent(sent) if sent == now => return Next::Nothing,
        Sync::Due | Sync::Sent(_) => {}
    }
    let publish = Publish {
        topic: settings.light_topic(),
        payload: state_json(now),
        qos: Qos::AtLeastOnce,
        retain: true,
    };
    Next::Offer(publish, Item::Light(now))
}

/// The session took the state message of the light at `sent` with the
/// packet id `packet`.
pub const fn taken(sent: Option<Level>, packet: PacketId, model: &mut Model) {
    if let Some(live) = model.connection.live_mut() {
        live.light = Sync::InFlight {
            packet,
            sent,
            again: false,
        };
    }
}

/// The session can never take the state message of the light. The message
/// is short, so this does not occur. If it occurs, the core does not offer
/// the message again.
pub const fn never(sent: Option<Level>, model: &mut Model) {
    if let Some(live) = model.connection.live_mut() {
        live.light = Sync::Sent(sent);
    }
}

/// The broker acknowledged the publish with the packet id `id`. Returns
/// `false` if the id is not the state message of the light.
pub fn acked(id: PacketId, model: &mut Model) -> bool {
    let Some(live) = model.connection.live_mut() else {
        return false;
    };
    match live.light {
        Sync::InFlight {
            packet,
            sent,
            again,
        } if packet == id => {
            live.light = if again { Sync::Due } else { Sync::Sent(sent) };
            true
        }
        Sync::Due | Sync::InFlight { .. } | Sync::Sent(_) => false,
    }
}

/// Returns the number of state messages of the light that wait for a
/// PUBACK: 0 or 1.
pub fn in_flight(model: &mut Model) -> usize {
    model.connection.live_mut().map_or(0, |live| {
        usize::from(matches!(live.light, Sync::InFlight { .. }))
    })
}

#[cfg(test)]
mod tests {
    //! Tests of the payloads of the light.

    use alloc::string::String;

    use super::{command, state_json, tap_level};
    use crate::features::light::{Command, Level, Percent, TapLevel};

    fn level(percent: u8) -> Level {
        Level::new(percent).unwrap()
    }

    fn set(percent: u8) -> TapLevel {
        TapLevel::Set(Percent::new(percent).unwrap())
    }

    #[test]
    fn a_command_parses_on_off_and_a_brightness() {
        assert_eq!(command(br#"{"state":"ON"}"#), Some(Command::On(None)));
        assert_eq!(
            command(br#"{"state":"ON","brightness":40}"#),
            Some(Command::On(Some(level(40))))
        );
        assert_eq!(
            command(br#"{"state":"ON","brightness":100}"#),
            Some(Command::On(Some(level(100))))
        );
        assert_eq!(command(br#"{"state":"OFF"}"#), Some(Command::Off));
        assert_eq!(
            command(br#"{"state":"OFF","brightness":40}"#),
            Some(Command::Off)
        );
    }

    #[test]
    fn a_command_ignores_the_keys_that_the_core_does_not_use() {
        assert_eq!(
            command(br#"{"state":"ON","brightness":40,"transition":2,"flash":"short"}"#),
            Some(Command::On(Some(level(40))))
        );
    }

    #[test]
    fn a_brightness_of_0_is_off() {
        assert_eq!(
            command(br#"{"state":"ON","brightness":0}"#),
            Some(Command::Off)
        );
    }

    #[test]
    fn a_command_that_is_not_valid_gives_none() {
        for payload in [
            &br#"{"state":"ON","brightness":101}"#[..],
            br#"{"state":"ON","brightness":-1}"#,
            br#"{"state":"ON","brightness":40.5}"#,
            br#"{"brightness":40}"#,
            br#"{"state":"on"}"#,
            br#"{"state":"TOGGLE"}"#,
            b"ON",
            b"",
            b"not json",
        ] {
            assert_eq!(
                command(payload),
                None,
                "{}",
                String::from_utf8_lossy(payload)
            );
        }
    }

    #[test]
    fn a_tap_level_parses_a_whole_number() {
        assert_eq!(tap_level(b"40"), Some(set(40)));
        assert_eq!(tap_level(b"40.0"), Some(set(40)));
        assert_eq!(tap_level(b"0"), Some(set(0)));
        assert_eq!(tap_level(b"100"), Some(set(100)));
    }

    #[test]
    fn an_empty_tap_level_is_the_default() {
        assert_eq!(tap_level(b""), Some(TapLevel::Default));
    }

    #[test]
    fn a_tap_level_that_is_not_valid_gives_none() {
        for payload in [
            &b"40.5"[..],
            b"-1",
            b"101",
            b"256",
            b"abc",
            b".5",
            b"4e1",
            b"\xff",
        ] {
            assert_eq!(
                tap_level(payload),
                None,
                "{}",
                String::from_utf8_lossy(payload)
            );
        }
    }

    #[test]
    fn the_state_payload_has_the_brightness_only_when_on() {
        assert_eq!(
            state_json(Some(level(40))),
            br#"{"state":"ON","brightness":40}"#
        );
        assert_eq!(state_json(None), br#"{"state":"OFF"}"#);
    }
}
