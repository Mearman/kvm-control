//! Pins the serialised field names and tags of `DeviceStatus`, which `kvm-cli status` and the MCP `status` tool emit as machine-readable output. A round trip cannot catch a rename made on both sides; this comparison against literal JSON can.

use kvm_core::{
    AtxStatus, DeviceStatus, GpioInput, GpioOutput, GpioStatus, HidOutputs, HidStatus,
    JigglerStatus, KernelInfo, KeyboardLeds, KeyboardStatus, Limits, MediaDrive, MediaImage,
    MediaStatus, MouseMode, MouseStatus, PlatformInfo, Reported, Resolution, StoragePartition,
    StreamerFeatures, StreamerStatus, SwitchPort, SwitchStatus, SystemInfo, VideoSource,
};
use serde_json::{Value, json};

fn yes<T>(value: T) -> Reported<T> {
    Reported::Supported(value)
}

fn text(value: &str) -> Reported<String> {
    yes(value.to_owned())
}

/// The JSON form of a supported value.
fn sup(value: Value) -> Value {
    json!({"support": "Supported", "value": value})
}

fn unsupported() -> Value {
    json!({"support": "Unsupported"})
}

fn unknown() -> Value {
    json!({"support": "Unknown"})
}

fn image() -> MediaImage {
    MediaImage {
        name: "disk.iso".into(),
        size: yes(2048),
        complete: yes(true),
        removable: Reported::Unknown,
    }
}

fn image_json() -> Value {
    json!({"name": "disk.iso", "size": sup(json!(2048)), "complete": sup(json!(true)), "removable": unknown()})
}

fn status() -> DeviceStatus {
    let outputs = HidOutputs {
        available: vec!["usb".into()],
        active: None,
    };
    DeviceStatus {
        info: yes(SystemInfo {
            kvmd_version: text("4.0"),
            streamer_app: text("ustreamer"),
            streamer_version: Reported::Unknown,
            kernel: yes(KernelInfo {
                system: text("Linux"),
                release: text("6.1"),
                version: text("#1"),
                machine: text("aarch64"),
            }),
            platform: yes(PlatformInfo {
                kind: text("rpi"),
                base: text("board"),
                serial: text("0001"),
                model: text("v4"),
                video: text("hdmi"),
                board: Reported::Unsupported,
            }),
            host: text("kvm"),
        }),
        hid: yes(HidStatus {
            online: yes(true),
            busy: yes(false),
            connected: Reported::Unknown,
            keyboard: yes(KeyboardStatus {
                online: yes(true),
                leds: yes(KeyboardLeds {
                    caps: yes(true),
                    scroll: yes(false),
                    num: yes(false),
                }),
                outputs: yes(outputs.clone()),
            }),
            mouse: yes(MouseStatus {
                online: yes(true),
                mode: MouseMode::Absolute,
                outputs: yes(HidOutputs {
                    active: Some("usb".into()),
                    ..outputs
                }),
            }),
            jiggler: yes(JigglerStatus {
                enabled: yes(false),
                active: yes(false),
                interval_seconds: yes(60),
            }),
        }),
        streamer: yes(StreamerStatus {
            features: StreamerFeatures {
                quality: yes(true),
                resolution: yes(false),
                h264: yes(true),
                h265: Reported::Unsupported,
                zero_delay: Reported::Unsupported,
                venc_mode: Reported::Unsupported,
            },
            desired_fps: yes(30),
            desired_fps_limits: yes(Limits { min: 0, max: 60 }),
            quality: yes(80),
            resolution: Reported::Unsupported,
            available_resolutions: Reported::Unsupported,
            h264_bitrate: yes(5000),
            h264_gop: yes(30),
            encoder: text("CPU"),
            source: yes(VideoSource {
                online: yes(true),
                resolution: yes(Resolution {
                    width: 1920,
                    height: 1080,
                }),
                captured_fps: yes(29),
            }),
            clients: yes(1),
        }),
        atx: yes(AtxStatus {
            busy: yes(false),
            power_led: yes(true),
            hdd_led: yes(false),
            power_busy: yes(false),
            reset_busy: yes(false),
        }),
        switch: yes(SwitchStatus {
            ports: yes(vec![SwitchPort {
                port: 0,
                unit: yes(0),
                channel: yes(0),
                id: text("1"),
                name: text("Server"),
            }]),
            active_port: yes(None),
            synced: yes(true),
        }),
        gpio: yes(GpioStatus {
            inputs: yes(vec![GpioInput {
                channel: "led".into(),
                online: yes(true),
                state: yes(true),
            }]),
            outputs: yes(vec![GpioOutput {
                channel: "relay".into(),
                online: yes(true),
                busy: yes(false),
                state: yes(false),
                switchable: yes(true),
            }]),
        }),
        media: yes(MediaStatus {
            online: yes(true),
            busy: yes(false),
            drive: yes(MediaDrive {
                connected: yes(true),
                cdrom: yes(true),
                rw: yes(false),
                image: yes(Some(image())),
            }),
            partition_drive: Reported::Unsupported,
            images: yes(vec![image()]),
            partitions: yes(vec![StoragePartition {
                name: String::new(),
                size: yes(4096),
                free: yes(1024),
                writable: yes(true),
            }]),
        }),
    }
}

fn expected() -> Value {
    let outputs = json!({"available": ["usb"], "active": null});
    json!({
        "info": sup(json!({
            "kvmd_version": sup(json!("4.0")),
            "streamer_app": sup(json!("ustreamer")),
            "streamer_version": unknown(),
            "kernel": sup(json!({
                "system": sup(json!("Linux")),
                "release": sup(json!("6.1")),
                "version": sup(json!("#1")),
                "machine": sup(json!("aarch64")),
            })),
            "platform": sup(json!({
                "kind": sup(json!("rpi")),
                "base": sup(json!("board")),
                "serial": sup(json!("0001")),
                "model": sup(json!("v4")),
                "video": sup(json!("hdmi")),
                "board": unsupported(),
            })),
            "host": sup(json!("kvm")),
        })),
        "hid": sup(json!({
            "online": sup(json!(true)),
            "busy": sup(json!(false)),
            "connected": unknown(),
            "keyboard": sup(json!({
                "online": sup(json!(true)),
                "leds": sup(json!({
                    "caps": sup(json!(true)),
                    "scroll": sup(json!(false)),
                    "num": sup(json!(false)),
                })),
                "outputs": sup(outputs),
            })),
            "mouse": sup(json!({
                "online": sup(json!(true)),
                "mode": "Absolute",
                "outputs": sup(json!({"available": ["usb"], "active": "usb"})),
            })),
            "jiggler": sup(json!({
                "enabled": sup(json!(false)),
                "active": sup(json!(false)),
                "interval_seconds": sup(json!(60)),
            })),
        })),
        "streamer": sup(json!({
            "features": {
                "quality": sup(json!(true)),
                "resolution": sup(json!(false)),
                "h264": sup(json!(true)),
                "h265": unsupported(),
                "zero_delay": unsupported(),
                "venc_mode": unsupported(),
            },
            "desired_fps": sup(json!(30)),
            "desired_fps_limits": sup(json!({"min": 0, "max": 60})),
            "quality": sup(json!(80)),
            "resolution": unsupported(),
            "available_resolutions": unsupported(),
            "h264_bitrate": sup(json!(5000)),
            "h264_gop": sup(json!(30)),
            "encoder": sup(json!("CPU")),
            "source": sup(json!({
                "online": sup(json!(true)),
                "resolution": sup(json!({"width": 1920, "height": 1080})),
                "captured_fps": sup(json!(29)),
            })),
            "clients": sup(json!(1)),
        })),
        "atx": sup(json!({
            "busy": sup(json!(false)),
            "power_led": sup(json!(true)),
            "hdd_led": sup(json!(false)),
            "power_busy": sup(json!(false)),
            "reset_busy": sup(json!(false)),
        })),
        "switch": sup(json!({
            "ports": sup(json!([{
                "port": 0,
                "unit": sup(json!(0)),
                "channel": sup(json!(0)),
                "id": sup(json!("1")),
                "name": sup(json!("Server")),
            }])),
            "active_port": sup(Value::Null),
            "synced": sup(json!(true)),
        })),
        "gpio": sup(json!({
            "inputs": sup(json!([{"channel": "led", "online": sup(json!(true)), "state": sup(json!(true))}])),
            "outputs": sup(json!([{
                "channel": "relay",
                "online": sup(json!(true)),
                "busy": sup(json!(false)),
                "state": sup(json!(false)),
                "switchable": sup(json!(true)),
            }])),
        })),
        "media": sup(json!({
            "online": sup(json!(true)),
            "busy": sup(json!(false)),
            "drive": sup(json!({
                "connected": sup(json!(true)),
                "cdrom": sup(json!(true)),
                "rw": sup(json!(false)),
                "image": sup(image_json()),
            })),
            "partition_drive": unsupported(),
            "images": sup(json!([image_json()])),
            "partitions": sup(json!([{
                "name": "",
                "size": sup(json!(4096)),
                "free": sup(json!(1024)),
                "writable": sup(json!(true)),
            }])),
        })),
    })
}

// Breaks if any status field is renamed, a tag is respelt, or an untagged field (an identifier, a mouse mode, a range, a resolution) gains or loses its tag.
#[test]
fn status_serialises_to_the_documented_schema() {
    assert_eq!(serde_json::to_value(status()).unwrap(), expected());
    let decoded: DeviceStatus = serde_json::from_value(expected()).unwrap();
    assert_eq!(decoded, status());
}
