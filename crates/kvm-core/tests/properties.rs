//! Property tests for the contract types that cross process boundaries: their serialised forms and the error messages that callers and logs receive.

use kvm_core::{
    AtxStatus, Capabilities, Capability, Control, DeviceProfile, DeviceStatus, Error, GpioInput,
    GpioOutput, GpioStatus, HidOutputs, HidStatus, JigglerStatus, KernelInfo, KeyboardLeds,
    KeyboardStatus, Limits, MediaDrive, MediaImage, MediaStatus, MouseMode, MouseStatus,
    PlatformInfo, PowerAction, Reported, Resolution, StoragePartition, StreamerFeatures,
    StreamerStatus, SwitchPort, SwitchStatus, SystemInfo, VideoSource, escape_json_for_terminal,
    is_terminal_unsafe,
};
use proptest::{collection::vec, option, prelude::*};
use serde_json::json;

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

fn reported<T: Clone + std::fmt::Debug + 'static>(
    inner: impl Strategy<Value = T> + 'static,
) -> BoxedStrategy<Reported<T>> {
    prop_oneof![
        inner.prop_map(Reported::Supported),
        Just(Reported::Unsupported),
        Just(Reported::Unknown),
    ]
    .boxed()
}

fn flag() -> BoxedStrategy<Reported<bool>> {
    reported(any::<bool>())
}

fn text() -> BoxedStrategy<Reported<String>> {
    reported(any::<String>())
}

fn number() -> BoxedStrategy<Reported<u32>> {
    reported(any::<u32>())
}

fn system_info() -> BoxedStrategy<SystemInfo> {
    let kernel =
        (text(), text(), text(), text()).prop_map(|(system, release, version, machine)| {
            KernelInfo {
                system,
                release,
                version,
                machine,
            }
        });
    let platform = (text(), text(), text(), text(), text(), text()).prop_map(
        |(kind, base, serial, model, video, board)| PlatformInfo {
            kind,
            base,
            serial,
            model,
            video,
            board,
        },
    );
    (
        text(),
        text(),
        text(),
        reported(kernel),
        reported(platform),
        text(),
    )
        .prop_map(
            |(kvmd_version, streamer_app, streamer_version, kernel, platform, host)| SystemInfo {
                kvmd_version,
                streamer_app,
                streamer_version,
                kernel,
                platform,
                host,
            },
        )
        .boxed()
}

fn hid_outputs() -> BoxedStrategy<Reported<HidOutputs>> {
    reported(
        (vec(any::<String>(), 0..3), option::of(any::<String>()))
            .prop_map(|(available, active)| HidOutputs { available, active }),
    )
}

fn hid_status() -> BoxedStrategy<HidStatus> {
    let leds =
        (flag(), flag(), flag()).prop_map(|(caps, scroll, num)| KeyboardLeds { caps, scroll, num });
    let keyboard = (flag(), reported(leds), hid_outputs()).prop_map(|(online, leds, outputs)| {
        KeyboardStatus {
            online,
            leds,
            outputs,
        }
    });
    let mouse =
        (flag(), mouse_mode(), hid_outputs()).prop_map(|(online, mode, outputs)| MouseStatus {
            online,
            mode,
            outputs,
        });
    let jiggler =
        (flag(), flag(), reported(any::<u64>())).prop_map(|(enabled, active, interval_seconds)| {
            JigglerStatus {
                enabled,
                active,
                interval_seconds,
            }
        });
    (
        flag(),
        flag(),
        flag(),
        reported(keyboard),
        reported(mouse),
        reported(jiggler),
    )
        .prop_map(
            |(online, busy, connected, keyboard, mouse, jiggler)| HidStatus {
                online,
                busy,
                connected,
                keyboard,
                mouse,
                jiggler,
            },
        )
        .boxed()
}

fn streamer_status() -> BoxedStrategy<StreamerStatus> {
    let features = (flag(), flag(), flag(), flag(), flag(), flag()).prop_map(
        |(quality, resolution, h264, h265, zero_delay, venc_mode)| StreamerFeatures {
            quality,
            resolution,
            h264,
            h265,
            zero_delay,
            venc_mode,
        },
    );
    let limits = (any::<u32>(), any::<u32>()).prop_map(|(min, max)| Limits { min, max });
    let resolution =
        (any::<u32>(), any::<u32>()).prop_map(|(width, height)| Resolution { width, height });
    let source =
        (flag(), reported(resolution), number()).prop_map(|(online, resolution, captured_fps)| {
            VideoSource {
                online,
                resolution,
                captured_fps,
            }
        });
    (
        features,
        number(),
        reported(limits),
        number(),
        text(),
        reported(vec(any::<String>(), 0..3)),
        number(),
        number(),
        text(),
        reported(source),
        number(),
    )
        .prop_map(
            |(
                features,
                desired_fps,
                desired_fps_limits,
                quality,
                resolution,
                available_resolutions,
                h264_bitrate,
                h264_gop,
                encoder,
                source,
                clients,
            )| StreamerStatus {
                features,
                desired_fps,
                desired_fps_limits,
                quality,
                resolution,
                available_resolutions,
                h264_bitrate,
                h264_gop,
                encoder,
                source,
                clients,
            },
        )
        .boxed()
}

fn atx_status() -> BoxedStrategy<AtxStatus> {
    (flag(), flag(), flag(), flag(), flag())
        .prop_map(
            |(busy, power_led, hdd_led, power_busy, reset_busy)| AtxStatus {
                busy,
                power_led,
                hdd_led,
                power_busy,
                reset_busy,
            },
        )
        .boxed()
}

fn switch_status() -> BoxedStrategy<SwitchStatus> {
    let port = (any::<u32>(), number(), number(), text(), text()).prop_map(
        |(port, unit, channel, id, name)| SwitchPort {
            port,
            unit,
            channel,
            id,
            name,
        },
    );
    (
        reported(vec(port, 0..3)),
        reported(option::of(any::<u32>())),
        flag(),
    )
        .prop_map(|(ports, active_port, synced)| SwitchStatus {
            ports,
            active_port,
            synced,
        })
        .boxed()
}

fn gpio_status() -> BoxedStrategy<GpioStatus> {
    let input = (any::<String>(), flag(), flag()).prop_map(|(channel, online, state)| GpioInput {
        channel,
        online,
        state,
    });
    let output = (any::<String>(), flag(), flag(), flag(), flag()).prop_map(
        |(channel, online, busy, state, switchable)| GpioOutput {
            channel,
            online,
            busy,
            state,
            switchable,
        },
    );
    (reported(vec(input, 0..3)), reported(vec(output, 0..3)))
        .prop_map(|(inputs, outputs)| GpioStatus { inputs, outputs })
        .boxed()
}

fn media_image() -> BoxedStrategy<MediaImage> {
    (any::<String>(), reported(any::<u64>()), flag(), flag())
        .prop_map(|(name, size, complete, removable)| MediaImage {
            name,
            size,
            complete,
            removable,
        })
        .boxed()
}

fn media_drive() -> BoxedStrategy<MediaDrive> {
    (flag(), flag(), flag(), reported(option::of(media_image())))
        .prop_map(|(connected, cdrom, rw, image)| MediaDrive {
            connected,
            cdrom,
            rw,
            image,
        })
        .boxed()
}

fn media_status() -> BoxedStrategy<MediaStatus> {
    let partition = (
        any::<String>(),
        reported(any::<u64>()),
        reported(any::<u64>()),
        flag(),
    )
        .prop_map(|(name, size, free, writable)| StoragePartition {
            name,
            size,
            free,
            writable,
        });
    (
        flag(),
        flag(),
        reported(media_drive()),
        reported(media_drive()),
        reported(vec(media_image(), 0..3)),
        reported(vec(partition, 0..3)),
    )
        .prop_map(
            |(online, busy, drive, partition_drive, images, partitions)| MediaStatus {
                online,
                busy,
                drive,
                partition_drive,
                images,
                partitions,
            },
        )
        .boxed()
}

fn device_status() -> BoxedStrategy<DeviceStatus> {
    (
        reported(system_info()),
        reported(hid_status()),
        reported(streamer_status()),
        reported(atx_status()),
        reported(switch_status()),
        reported(gpio_status()),
        reported(media_status()),
    )
        .prop_map(
            |(info, hid, streamer, atx, switch, gpio, media)| DeviceStatus {
                info,
                hid,
                streamer,
                atx,
                switch,
                gpio,
                media,
            },
        )
        .boxed()
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

    // Breaks if a status field is renamed or skipped on one side of the round trip, or if a support state is spelt differently on encoding and decoding.
    #[test]
    fn statuses_survive_a_round_trip_through_json(status in device_status()) {
        let encoded = serde_json::to_value(&status).unwrap();
        let decoded: DeviceStatus = serde_json::from_value(encoded.clone()).unwrap();
        prop_assert_eq!(serde_json::to_value(&decoded).unwrap(), encoded);
        prop_assert_eq!(decoded, status);
    }

    // Breaks if the machine-readable spelling of a reported value changes, or if its capability disagrees with whether it carries a value.
    #[test]
    fn reported_values_have_a_stable_tagged_spelling(value in reported(any::<u32>())) {
        let expected = match &value {
            Reported::Supported(inner) => json!({"support": "Supported", "value": inner}),
            Reported::Unsupported => json!({"support": "Unsupported"}),
            Reported::Unknown => json!({"support": "Unknown"}),
        };
        prop_assert_eq!(serde_json::to_value(&value).unwrap(), expected);
        let capability = value.capability();
        prop_assert_eq!(value.value().is_some(), capability == Capability::Supported);
        prop_assert_eq!(serde_json::to_value(capability).unwrap(), serde_json::to_value(&value).unwrap()["support"].clone());
    }

    // Breaks if escaping changes the decoded JSON, or leaves a character that can drive a terminal (C1 controls such as U+009B, DEL, bidirectional controls) in the output.
    #[test]
    fn terminal_escaping_preserves_json_and_removes_unsafe_characters(
        text in proptest::collection::vec(
            prop_oneof![
                any::<char>(),
                proptest::sample::select(vec!['\u{1b}', '\u{7f}', '\u{9b}', '\u{202e}', '\u{2066}', '\n', '\t']),
            ],
            0..24,
        )
        .prop_map(String::from_iter),
    ) {
        let value = json!({"name": text, "list": [text.clone()]});
        for serialised in [serde_json::to_string(&value).unwrap(), serde_json::to_string_pretty(&value).unwrap()] {
            let escaped = escape_json_for_terminal(&serialised);
            prop_assert!(!escaped.chars().any(|c| is_terminal_unsafe(c) && !matches!(c, '\n' | '\t' | '\r')), "{}", escaped);
            prop_assert_eq!(serde_json::from_str::<serde_json::Value>(&escaped).unwrap(), value.clone());
        }
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
