//! Property tests for the contract types that cross process boundaries: their serialised forms and the error messages that callers and logs receive.

use kvm_core::{
    Capabilities, Capability, Control, DeviceProfile, DeviceStatus, Error, MouseMode, PowerAction,
};
use proptest::{collection::vec, prelude::*};
use serde_json::Value;

fn capability() -> impl Strategy<Value = Capability> {
    proptest::sample::select(vec![
        Capability::Supported,
        Capability::Unsupported,
        Capability::Unknown,
    ])
}

fn mouse_mode() -> impl Strategy<Value = MouseMode> {
    proptest::sample::select(vec![
        MouseMode::Absolute,
        MouseMode::Relative,
        MouseMode::Unknown,
    ])
}

fn json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<String>().prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            vec(inner.clone(), 0..4).prop_map(Value::Array),
            proptest::collection::hash_map(any::<String>(), inner, 0..4)
                .prop_map(|entries| Value::Object(entries.into_iter().collect())),
        ]
    })
}

fn control() -> impl Strategy<Value = Control> {
    prop_oneof![
        proptest::sample::select(vec![
            PowerAction::On,
            PowerAction::Off,
            PowerAction::OffHard,
            PowerAction::ResetHard,
        ])
        .prop_map(Control::Power),
        any::<u32>().prop_map(|port| Control::Switch { port }),
        (any::<String>(), any::<bool>())
            .prop_map(|(channel, state)| Control::Gpio { channel, state }),
        any::<bool>().prop_map(|connected| Control::MediaConnect { connected }),
        any::<String>().prop_map(|image| Control::MediaSelect { image }),
    ]
}

proptest! {
    // Breaks if a field is renamed or skipped on one side of the round trip, or if two capability states share a spelling.
    #[test]
    fn capabilities_survive_a_round_trip_through_json(
        states in vec(capability(), 9),
        mode in mouse_mode(),
    ) {
        let capabilities = Capabilities {
            status: states[0],
            snapshot: states[1],
            keyboard: states[2],
            mouse: states[3],
            mouse_mode: mode,
            live_video: states[4],
            power: states[5],
            switch: states[6],
            gpio: states[7],
            media: states[8],
        };
        let encoded = serde_json::to_value(&capabilities).unwrap();
        let decoded: Capabilities = serde_json::from_value(encoded.clone()).unwrap();
        prop_assert_eq!(decoded.status, capabilities.status);
        prop_assert_eq!(decoded.snapshot, capabilities.snapshot);
        prop_assert_eq!(decoded.keyboard, capabilities.keyboard);
        prop_assert_eq!(decoded.mouse, capabilities.mouse);
        prop_assert_eq!(decoded.mouse_mode, capabilities.mouse_mode);
        prop_assert_eq!(decoded.live_video, capabilities.live_video);
        prop_assert_eq!(decoded.power, capabilities.power);
        prop_assert_eq!(decoded.switch, capabilities.switch);
        prop_assert_eq!(decoded.gpio, capabilities.gpio);
        prop_assert_eq!(decoded.media, capabilities.media);
        prop_assert_eq!(serde_json::to_value(&decoded).unwrap(), encoded);
    }

    // Breaks if device-reported JSON is reshaped when carried in a status.
    #[test]
    fn status_carries_arbitrary_device_json_unchanged(info in json(), hid in json(), streamer in json()) {
        let status = DeviceStatus { info, hid, streamer };
        let decoded: DeviceStatus = serde_json::from_str(&serde_json::to_string(&status).unwrap()).unwrap();
        prop_assert_eq!(decoded.info, status.info);
        prop_assert_eq!(decoded.hid, status.hid);
        prop_assert_eq!(decoded.streamer, status.streamer);
    }

    // Breaks if a control operation loses a payload field or decodes as another variant.
    #[test]
    fn controls_survive_a_round_trip_through_json(operation in control()) {
        let encoded = serde_json::to_value(&operation).unwrap();
        let decoded: Control = serde_json::from_value(encoded.clone()).unwrap();
        prop_assert_eq!(serde_json::to_value(&decoded).unwrap(), encoded);
        match (&operation, &decoded) {
            (Control::Power(left), Control::Power(right)) => {
                prop_assert_eq!(format!("{left:?}"), format!("{right:?}"));
            }
            (Control::Switch { port: left }, Control::Switch { port: right }) => {
                prop_assert_eq!(left, right);
            }
            (
                Control::Gpio { channel: left_channel, state: left_state },
                Control::Gpio { channel: right_channel, state: right_state },
            ) => {
                prop_assert_eq!(left_channel, right_channel);
                prop_assert_eq!(left_state, right_state);
            }
            (Control::MediaConnect { connected: left }, Control::MediaConnect { connected: right }) => {
                prop_assert_eq!(left, right);
            }
            (Control::MediaSelect { image: left }, Control::MediaSelect { image: right }) => {
                prop_assert_eq!(left, right);
            }
            _ => panic!("{operation:?} decoded as {decoded:?}"),
        }
    }

    // Breaks if an uncertain outcome loses the instruction not to replay it, which is what keeps a retry from repeating an input or control that may already have happened. Also breaks if a variant stops carrying its detail or status.
    #[test]
    fn errors_keep_their_payload_and_the_replay_warning(detail in any::<String>(), status in any::<u16>()) {
        let details = [
            Error::Unsupported(detail.clone()),
            Error::Transport(detail.clone()),
            Error::InvalidResponse(detail.clone()),
            Error::AmbiguousInput(detail.clone()),
            Error::AmbiguousControl(detail.clone()),
        ];
        for error in &details {
            prop_assert!(error.to_string().ends_with(&detail), "{error}");
        }
        for error in &details[3..] {
            prop_assert!(error.to_string().contains("do not replay automatically"), "{error}");
        }
        prop_assert!(Error::Http(status).to_string().ends_with(&status.to_string()));
    }

    // Breaks if two profiles share a spelling or one fails to decode.
    #[test]
    fn profiles_have_distinct_stable_spellings(first in any::<bool>(), second in any::<bool>()) {
        let pick = |glkvm: bool| if glkvm { DeviceProfile::GlKvm } else { DeviceProfile::PiKvm };
        let (left, right) = (pick(first), pick(second));
        let encoded = serde_json::to_string(&left).unwrap();
        prop_assert_eq!(serde_json::from_str::<DeviceProfile>(&encoded).unwrap(), left);
        prop_assert_eq!(encoded == serde_json::to_string(&right).unwrap(), left == right);
    }
}
