//! Property tests for the adapter's pure logic: request URL construction, endpoint validation, held-input tracking and firmware state interpretation. Behaviour that needs a device is covered by the property tests in `tests/properties.rs`.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use kvm_core::{Capability, DeviceProfile, MouseMode};
use proptest::{collection::vec, prelude::*};
use serde_json::{Value, json};
use url::Url;

use super::{Config, Held, InFlight, Inputs, Kvmd, hid_capability, mouse_mode};

fn config(endpoint: Url) -> Config {
    Config {
        endpoint,
        profile: DeviceProfile::PiKvm,
        trusted_ca_pem: None,
        insecure_tls: false,
        timeout: Duration::from_secs(5),
    }
}

/// An optional value that cannot occur in any fixed message, so containment proves a leak.
fn secret() -> impl Strategy<Value = Option<String>> {
    proptest::option::of("SECRET-[a-z0-9]{1,10}")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // Breaks if the prefix is joined without a separating slash, because the last prefix segment is then replaced by `api/`.
    #[test]
    fn api_urls_extend_the_endpoint_prefix(
        segments in vec("[a-z0-9]{1,8}", 0..4),
        trailing_slash in any::<bool>(),
        tail in vec("[a-z_]{1,8}", 1..4),
        port in 1024u16..,
    ) {
        let prefix = if segments.is_empty() {
            "/".to_owned()
        } else {
            format!("/{}/", segments.join("/"))
        };
        let written = if trailing_slash {
            prefix.clone()
        } else {
            prefix.trim_end_matches('/').to_owned()
        };
        let endpoint = Url::parse(&format!("https://device.example:{port}{written}")).unwrap();
        let device = Kvmd::new(config(endpoint)).unwrap();
        let path = tail.join("/");
        let url = device.url(&path).unwrap();
        prop_assert_eq!(url.scheme(), "https");
        prop_assert_eq!(url.host_str(), Some("device.example"));
        prop_assert_eq!(url.port(), Some(port));
        prop_assert_eq!(url.path(), format!("{prefix}api/{path}"));
        prop_assert_eq!(url.query(), None);
        prop_assert_eq!(url.fragment(), None);
    }

    // Breaks if any of the four rejection conditions is dropped, or if a rejection echoes the offending part.
    #[test]
    fn endpoints_are_accepted_only_without_credentials_query_or_fragment(
        https in any::<bool>(),
        user in secret(),
        password in secret(),
        query in secret(),
        fragment in secret(),
    ) {
        let mut endpoint = Url::parse("https://device.example/proxy/").unwrap();
        if !https {
            endpoint.set_scheme("http").unwrap();
        }
        if let Some(user) = &user {
            endpoint.set_username(user).unwrap();
        }
        if let Some(password) = &password {
            endpoint.set_password(Some(password)).unwrap();
        }
        endpoint.set_query(query.as_deref());
        endpoint.set_fragment(fragment.as_deref());
        let acceptable = https
            && user.is_none()
            && password.is_none()
            && query.is_none()
            && fragment.is_none();
        match Kvmd::new(config(endpoint)) {
            Ok(_) => prop_assert!(acceptable),
            Err(error) => {
                prop_assert!(!acceptable);
                let rendered = format!("{error} {error:?}");
                for part in [&user, &password, &query, &fragment].into_iter().flatten() {
                    prop_assert!(!rendered.contains(part.as_str()), "{rendered}");
                }
            }
        }
    }
}

/// One recorded outcome: a key or button, its name, whether it was a press, and how the request ended.
#[derive(Debug, Clone)]
struct Record {
    key: bool,
    name: String,
    pressed: bool,
    outcome: Outcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Confirmed,
    Unconfirmed,
    Abandoned,
}

fn record() -> impl Strategy<Value = Record> {
    (
        any::<bool>(),
        "[a-c]",
        any::<bool>(),
        proptest::sample::select(vec![
            Outcome::Confirmed,
            Outcome::Unconfirmed,
            Outcome::Abandoned,
        ]),
    )
        .prop_map(|(key, name, pressed, outcome)| Record {
            key,
            name,
            pressed,
            outcome,
        })
}

proptest! {
    // The held sets follow each input's last operation whether or not it was confirmed, the two kinds never mix, and any unconfirmed or abandoned request marks its input and the session indeterminate permanently. Breaks if a release leaves the name held, an abandoned request is forgotten, or the indeterminate flag is ever cleared.
    #[test]
    fn held_inputs_follow_their_last_operation(records in vec(record(), 0..32)) {
        let mut inputs = Inputs::default();
        let mut last: BTreeMap<(bool, String), bool> = BTreeMap::new();
        let mut unconfirmed: BTreeSet<(bool, String)> = BTreeSet::new();
        for step in records {
            let kind = if step.key { Held::Key } else { Held::Button };
            let in_flight = InFlight {
                inputs: &mut inputs,
                kind,
                name: &step.name,
                pressed: step.pressed,
                finished: false,
            };
            match step.outcome {
                Outcome::Confirmed => in_flight.finish(true),
                Outcome::Unconfirmed => in_flight.finish(false),
                Outcome::Abandoned => drop(in_flight),
            }
            last.insert((step.key, step.name.clone()), step.pressed);
            if step.outcome != Outcome::Confirmed {
                unconfirmed.insert((step.key, step.name));
            }
            let names = |key: bool, set: &BTreeMap<(bool, String), bool>| -> BTreeSet<String> {
                set.iter()
                    .filter(|((kind, _), pressed)| *kind == key && **pressed)
                    .map(|((_, name), _)| name.clone())
                    .collect()
            };
            prop_assert_eq!(&inputs.keys, &names(true, &last));
            prop_assert_eq!(&inputs.buttons, &names(false, &last));
            let uncertain = |key: bool| -> BTreeSet<String> {
                unconfirmed
                    .iter()
                    .filter(|(kind, _)| *kind == key)
                    .map(|(_, name)| name.clone())
                    .collect()
            };
            prop_assert_eq!(&inputs.uncertain_keys, &uncertain(true));
            prop_assert_eq!(&inputs.uncertain_buttons, &uncertain(false));
            prop_assert_eq!(inputs.indeterminate, !unconfirmed.is_empty());
        }
    }
}

/// A JSON flag as firmware might report it: absent, a boolean, or a value of the wrong type.
fn flag() -> impl Strategy<Value = Option<Value>> {
    prop_oneof![
        Just(None),
        Just(Some(json!(true))),
        Just(Some(json!(false))),
        Just(Some(json!("true"))),
        Just(Some(json!(1))),
        Just(Some(Value::Null)),
    ]
}

proptest! {
    // Breaks if the mode is read from a non-boolean value or from a flag outside the mouse object.
    #[test]
    fn mouse_mode_reads_only_a_boolean_in_the_mouse_object(
        absolute in flag(),
        stray in flag(),
    ) {
        let mut mouse = json!({});
        if let Some(value) = &absolute {
            mouse["absolute"] = value.clone();
        }
        let mut state = json!({"enabled": true, "mouse": mouse});
        if let Some(value) = stray {
            state["absolute"] = value;
        }
        let expected = match absolute {
            Some(Value::Bool(true)) => MouseMode::Absolute,
            Some(Value::Bool(false)) => MouseMode::Relative,
            _ => MouseMode::Unknown,
        };
        prop_assert_eq!(mouse_mode(&state), expected);
    }

    // Breaks if a wrongly typed flag counts as true, or if the other device's state leaks into the answer.
    #[test]
    fn hid_capability_requires_boolean_flags_on_the_named_device(
        enabled in flag(),
        online in flag(),
        other_online in flag(),
        mouse in any::<bool>(),
    ) {
        let (device, other) = if mouse { ("mouse", "keyboard") } else { ("keyboard", "mouse") };
        let mut state = json!({});
        if let Some(value) = &enabled {
            state["enabled"] = value.clone();
        }
        if let Some(value) = &online {
            state[device] = json!({"online": value});
        }
        if let Some(value) = other_online {
            state[other] = json!({"online": value});
        }
        let expected = match (enabled, online) {
            (Some(Value::Bool(false)), _) => Capability::Unsupported,
            (Some(Value::Bool(true)), Some(Value::Bool(true))) => Capability::Supported,
            _ => Capability::Unknown,
        };
        prop_assert_eq!(hid_capability(&state, device), expected);
    }
}
