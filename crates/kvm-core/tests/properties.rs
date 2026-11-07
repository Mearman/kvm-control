//! Property tests for the contract types that cross process boundaries: their serialised forms and the error messages that callers and logs receive.

use kvm_core::{
    AtxStatus, AuthScheme, CallerIdentity, Capabilities, Capability, Control, Credentials,
    DeviceId, DeviceIdentity, DeviceProfile, DeviceStatus, Endpoint, EndpointName, Error,
    GpioInput, GpioOutput, GpioStatus, HidOutputs, HidStatus, JigglerStatus, KernelInfo,
    KeyboardLeds, KeyboardStatus, Limits, LoginState, MediaDrive, MediaImage, MediaStatus,
    MouseMode, MouseStatus, PlatformInfo, PowerAction, Reported, Resolution, Secret, ServerTrust,
    StoragePartition, StreamerFeatures, StreamerStatus, SwitchPort, SwitchStatus, SystemInfo,
    VideoSource, escape_json_for_terminal, is_terminal_unsafe,
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

fn profile() -> impl Strategy<Value = DeviceProfile> {
    proptest::sample::select(vec![DeviceProfile::PiKvm, DeviceProfile::GlKvm])
}

fn endpoint() -> BoxedStrategy<Endpoint> {
    (
        option::of(any::<String>()),
        "[a-z]{1,12}",
        option::of(1_u16..),
        "(/[a-z0-9]{1,8}){0,3}/?",
        profile(),
        proptest::sample::select(vec![
            AuthScheme::Session,
            AuthScheme::Header,
            AuthScheme::Basic,
        ]),
        prop_oneof![
            option::of(any::<String>())
                .prop_map(|additional_ca_pem| ServerTrust::Verified { additional_ca_pem }),
            Just(ServerTrust::Disabled),
        ],
        any::<(u64, u32)>(),
    )
        .prop_map(
            |(label, host, port, path, profile, auth, trust, (secs, nanos))| Endpoint {
                label,
                address: url::Url::parse(&match port {
                    Some(port) => format!("https://{host}.invalid:{port}{path}"),
                    None => format!("https://{host}.invalid{path}"),
                })
                .unwrap(),
                profile,
                auth,
                trust,
                timeout: std::time::Duration::new(secs, nanos % 1_000_000_000),
            },
        )
        .boxed()
}

fn login_state() -> BoxedStrategy<LoginState> {
    prop_oneof![
        Just(LoginState::Authenticated),
        option::of(any::<u64>()).prop_map(|expires_in| LoginState::ApprovalPending { expires_in }),
        Just(LoginState::Unverified),
    ]
    .boxed()
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

    // Breaks if an endpoint field is renamed or skipped on one side of the round trip, so stored configuration would reconnect somewhere else or with another profile, scheme or trust policy.
    #[test]
    fn endpoints_survive_a_round_trip_through_json(endpoint in endpoint()) {
        let encoded = serde_json::to_value(&endpoint).unwrap();
        let decoded: Endpoint = serde_json::from_value(encoded.clone()).unwrap();
        prop_assert_eq!(serde_json::to_value(&decoded).unwrap(), encoded);
        prop_assert_eq!(decoded, endpoint);
    }

    // Breaks if a login state decodes as another state or loses whether its approval expiry was reported.
    #[test]
    fn login_states_survive_a_round_trip_through_json(state in login_state()) {
        let encoded = serde_json::to_value(state).unwrap();
        prop_assert_eq!(serde_json::from_value::<LoginState>(encoded).unwrap(), state);
    }

    // Breaks if identity takes its label, address or profile from anywhere but the endpoint, or its serial number or host name from anywhere but the report, keeps a value the device did not report, or does not survive serialisation.
    #[test]
    fn identity_separates_configuration_from_device_claims(
        endpoint in endpoint(),
        verified in any::<bool>(),
        serial in any::<String>(),
        host in any::<String>(),
    ) {
        let info = |platform: Reported<PlatformInfo>, host: Reported<String>| {
            Reported::Supported(SystemInfo {
                kvmd_version: Reported::Unknown,
                streamer_app: Reported::Unknown,
                streamer_version: Reported::Unknown,
                kernel: Reported::Unknown,
                platform,
                host,
            })
        };
        let platform = |serial: Reported<String>| {
            Reported::Supported(PlatformInfo {
                kind: Reported::Unknown,
                base: Reported::Unknown,
                serial,
                model: Reported::Supported("not an identifier".into()),
                video: Reported::Unknown,
                board: Reported::Unknown,
            })
        };
        let cases = [
            (
                info(platform(Reported::Supported(serial.clone())), Reported::Supported(host.clone())),
                Reported::Supported(serial.clone()),
                Reported::Supported(host.clone()),
            ),
            (info(platform(Reported::Unknown), Reported::Unsupported), Reported::Unknown, Reported::Unsupported),
            (info(platform(Reported::Unsupported), Reported::Unknown), Reported::Unsupported, Reported::Unknown),
            (info(Reported::Unknown, Reported::Supported(host.clone())), Reported::Unknown, Reported::Supported(host.clone())),
            (info(Reported::Unsupported, Reported::Unknown), Reported::Unsupported, Reported::Unknown),
            (Reported::Unknown, Reported::Unknown, Reported::Unknown),
            (Reported::Unsupported, Reported::Unsupported, Reported::Unsupported),
        ];
        for (reported, expected_serial, expected_host) in cases {
            let identity = DeviceIdentity::observe(&endpoint, verified, &reported);
            prop_assert_eq!(&identity.label, &endpoint.label);
            prop_assert_eq!(&identity.address, &endpoint.address);
            prop_assert_eq!(identity.profile, endpoint.profile);
            prop_assert_eq!(identity.address_verified, verified);
            prop_assert_eq!(&identity.serial, &expected_serial);
            prop_assert_eq!(&identity.host, &expected_host);
            let encoded = serde_json::to_value(&identity).unwrap();
            prop_assert_eq!(serde_json::from_value::<DeviceIdentity>(encoded).unwrap(), identity.clone());
            let id = identity.id();
            prop_assert_eq!(&id, &endpoint.device_id());
            let encoded = serde_json::to_value(&id).unwrap();
            prop_assert_eq!(serde_json::from_value::<DeviceId>(encoded).unwrap(), id);
        }
    }

    // Each field of an identity is varied alone. Breaks if the identifier changes with anything the device reports or with the address of a labelled endpoint, stays the same when the label, the profile or an unlabelled address changes, or lets a label spelt like an address collide with that address.
    #[test]
    fn device_ids_depend_only_on_the_configured_name_and_profile(
        endpoint in endpoint(),
        other_label in any::<String>(),
        other_host in "[a-z]{1,12}",
        status in device_status(),
    ) {
        let identity = DeviceIdentity::observe(&endpoint, true, &status.info);
        let id = identity.id();
        let expected_name = match &endpoint.label {
            Some(label) => EndpointName::Label(label.clone()),
            None => EndpointName::Address(endpoint.address.clone()),
        };
        prop_assert_eq!(&id.endpoint, &expected_name);
        prop_assert_eq!(id.profile, endpoint.profile);

        // What the device reports, and whether the address was verified, never move the identifier.
        let claims = DeviceIdentity {
            serial: Reported::Supported("another serial".into()),
            host: Reported::Unknown,
            address_verified: false,
            ..identity.clone()
        };
        prop_assert_eq!(claims.id(), id.clone());
        let unread = DeviceIdentity::observe(&endpoint, false, &Reported::Unknown);
        prop_assert_eq!(unread.id(), id.clone());

        let moved = DeviceIdentity {
            address: url::Url::parse(&format!("https://{other_host}.example.invalid/moved/")).unwrap(),
            ..identity.clone()
        };
        prop_assert_eq!(moved.id() == id, identity.label.is_some());

        let relabelled = DeviceIdentity {
            label: Some(other_label.clone()),
            ..identity.clone()
        };
        prop_assert_eq!(relabelled.id() == id, identity.label.as_deref() == Some(other_label.as_str()));

        let reprofiled = DeviceIdentity {
            profile: match identity.profile {
                DeviceProfile::PiKvm => DeviceProfile::GlKvm,
                DeviceProfile::GlKvm => DeviceProfile::PiKvm,
            },
            ..identity.clone()
        };
        prop_assert_ne!(reprofiled.id(), id);

        let unlabelled = DeviceIdentity { label: None, ..identity.clone() };
        let spelt_as_address = DeviceIdentity {
            label: Some(identity.address.to_string()),
            ..identity.clone()
        };
        prop_assert_ne!(unlabelled.id(), spelt_as_address.id());
    }

    // Breaks if a secret reaches debug output, which is what logs and panic messages print.
    #[test]
    fn secrets_are_redacted_from_debug_output(
        username in "[a-z]{1,8}",
        password in "pw-[A-Za-z0-9]{8,32}",
        code in proptest::option::of("[0-9]{6}"),
    ) {
        let secret = Secret::from(password.as_str());
        prop_assert_eq!(secret.expose(), password.as_str());
        prop_assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        let mut credentials = Credentials::new(username.clone(), password.clone());
        if let Some(code) = &code {
            credentials = credentials.with_totp(code.as_str());
        }
        let printed = format!("{credentials:?}");
        prop_assert!(!printed.contains(&password), "{}", printed);
        if let Some(code) = &code {
            prop_assert!(!printed.contains(code.as_str()), "{}", printed);
        }
        prop_assert!(printed.contains(&username), "{}", printed);
        prop_assert_eq!(&credentials.caller, &CallerIdentity { username });
        prop_assert_eq!(credentials.totp.is_some(), code.is_some());
    }
}
