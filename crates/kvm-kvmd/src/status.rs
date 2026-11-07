//! Typed status read from the KVMD state routes.
//!
//! Response shapes follow PiKVM `kvmd` revision fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef and GLKVM `glkvm` revision 3e8dd23c4bd638a4650433664cc3b0f3b4d29395: the [PiKVM](https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/kvmd/info/__init__.py) and [GLKVM](https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/info/__init__.py) info managers, the [PiKVM](https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/plugins/hid/otg/__init__.py) and [GLKVM](https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/plugins/hid/otg/__init__.py) OTG HID plugins, the [PiKVM](https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/kvmd/streamer/__init__.py) and [GLKVM](https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/streamer.py) streamer managers with uStreamer's [state handler](https://github.com/pikvm/ustreamer/blob/11fd828ab00c01a39bd0843ef442ef0b6d3cac41/src/ustreamer/http/server.c), the [PiKVM](https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/plugins/atx/gpio.py) and [GLKVM](https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/plugins/atx/gpio.py) ATX plugins, the [PiKVM](https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/kvmd/switch/state.py) and [GLKVM](https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/switch/state.py) switch state, the [PiKVM](https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/kvmd/ugpio.py) and [GLKVM](https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/ugpio.py) user GPIO, and the [PiKVM](https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/plugins/msd/otg/__init__.py) and [GLKVM](https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/plugins/msd/otg/__init__.py) OTG media plugins. These are source revisions, not firmware releases, and no physical device has been read.
//!
//! Profile differences in those sources: only PiKVM reports per-button ATX activity (`acts`), only GLKVM reports a partition-backed media drive (`drive_partition`) and the `h265`, `zero_delay` and `venc_mode` streamer features, and GLKVM hard-codes the switch's `synced` flag and, in its system information, the streamer version and the platform type, so those are reported as unknown for that profile. A field present in a response is read whatever the profile.

use kvm_core::{
    AtxStatus, DeviceProfile, DeviceStatus, Error, GpioInput, GpioOutput, GpioStatus, HidOutputs,
    HidStatus, JigglerStatus, KernelInfo, KeyboardLeds, KeyboardStatus, Limits, MediaDrive,
    MediaImage, MediaStatus, MouseMode, MouseStatus, PlatformInfo, Reported, Resolution, Result,
    StoragePartition, StreamerFeatures, StreamerStatus, SwitchPort, SwitchStatus, SystemInfo,
    VideoSource,
};
use serde_json::Value;

use super::{Kvmd, RequestFailure, mouse_mode};

/// State routes read for a status, in the order `parse` takes them.
const ROUTES: [&str; 7] = ["info", "hid", "streamer", "atx", "switch", "gpio", "msd"];

/// The outcome of reading one state route.
#[derive(Debug, Default)]
pub(crate) enum Read {
    State(Value),
    /// The route does not exist on the device (HTTP 404).
    Absent,
    /// The session may not read the route.
    #[default]
    Denied,
    /// The route answered but its state could not be read: an HTTP error status, `ok: false`, a missing result, malformed JSON or an oversized body.
    Failed(Error),
}

impl Kvmd {
    /// Reads every state route with GET requests only, so reading status sends no input or control request. A connection failure (no connection or TLS session could be established) fails the whole status at once. A route that timed out after connecting, or whose body broke off, failed on its own, like one answering an error status; how such outcomes combine is described at [`parse`].
    pub(crate) async fn read_status(&self) -> Result<DeviceStatus> {
        let active = self.client.read().await;
        let mut reads: [Read; 7] = Default::default();
        for (read, path) in reads.iter_mut().zip(ROUTES) {
            *read = match self.state_with(&active.client, path).await {
                Ok(state) => Read::State(state),
                Err(RequestFailure {
                    error,
                    connection: true,
                }) => return Err(error),
                Err(RequestFailure {
                    error: Error::Http(404),
                    ..
                }) => Read::Absent,
                Err(RequestFailure {
                    error: Error::Authentication,
                    ..
                }) => Read::Denied,
                Err(RequestFailure { error, .. }) => Read::Failed(error),
            };
        }
        parse(self.profile, reads)
    }
}

/// Builds a typed status from the route reads, in `ROUTES` order.
///
/// Both inspected sources always register the info, HID and streamer routes, so a 404 on any of them means the endpoint is not a KVMD API and fails with that HTTP error. A status in which no route produced state has observed nothing, so it fails: with an authentication error when a route was denied, otherwise with the info route's error (for example the HTTP 502 of a proxy whose daemon is stopped, or the invalid JSON of a page that is not the API).
///
/// A denied route is unknown, which keeps the subsystems a partly permitted session may read. A session whose credentials became invalid between requests therefore looks the same as one denied some routes; `check_auth` or a new login tells them apart. Otherwise each failed or denied route is unknown, and an absent route is unsupported only when the info route was read, which establishes that the endpoint is a KVMD API; without that it is unknown.
pub(crate) fn parse(profile: DeviceProfile, reads: [Read; 7]) -> Result<DeviceStatus> {
    if reads[..3].iter().any(|read| matches!(read, Read::Absent)) {
        return Err(Error::Http(404));
    }
    let observed = reads.iter().any(|read| matches!(read, Read::State(_)));
    if !observed && reads.iter().any(|read| matches!(read, Read::Denied)) {
        return Err(Error::Authentication);
    }
    let api_confirmed = matches!(reads[0], Read::State(_));
    let [info, hid, streamer, atx, switch, gpio, msd] = reads;
    // With nothing observed and nothing denied, the info route, which is neither absent (checked above) nor read, failed, and its error is the status's.
    let info = match info {
        Read::Failed(error) if !observed => return Err(error),
        info => info,
    };
    Ok(DeviceStatus {
        info: section(info, api_confirmed, |state| {
            Reported::Supported(system_info(profile, state))
        }),
        hid: section(hid, api_confirmed, |state| enabled(state, hid_status)),
        streamer: section(streamer, api_confirmed, |state| {
            Reported::Supported(streamer_status(profile, state))
        }),
        atx: section(atx, api_confirmed, |state| {
            enabled(state, |state| atx_status(profile, state))
        }),
        switch: section(switch, api_confirmed, |state| switch_status(profile, state)),
        gpio: section(gpio, api_confirmed, gpio_status),
        media: section(msd, api_confirmed, |state| {
            enabled(state, |state| media_status(profile, state))
        }),
    })
}

/// A subsystem's status: unknown when its route was denied, failed or is not an object, and when absent unsupported if `api_confirmed` (the info route was read), otherwise unknown.
fn section<T>(
    read: Read,
    api_confirmed: bool,
    parse: impl FnOnce(&Value) -> Reported<T>,
) -> Reported<T> {
    match read {
        Read::State(state) if state.is_object() => parse(&state),
        Read::Absent if api_confirmed => Reported::Unsupported,
        Read::State(_) | Read::Denied | Read::Failed(_) | Read::Absent => Reported::Unknown,
    }
}

/// A subsystem that reports `enabled`. A disabled plugin reports placeholder values (all LEDs off, for example), so only an explicitly enabled subsystem is parsed; one that omits the flag is unknown, as in capability discovery.
fn enabled<T>(state: &Value, parse: impl FnOnce(&Value) -> T) -> Reported<T> {
    match state.get("enabled").and_then(Value::as_bool) {
        Some(true) => Reported::Supported(parse(state)),
        Some(false) => Reported::Unsupported,
        None => Reported::Unknown,
    }
}

fn extract<T>(found: Option<&Value>, read: impl FnOnce(&Value) -> Option<T>) -> Reported<T> {
    found
        .and_then(read)
        .map_or(Reported::Unknown, Reported::Supported)
}

/// A field that only some profiles' inspected sources report: when absent it is unsupported for a profile whose source never reports it.
fn profile_field<T>(
    state: &Value,
    pointer: &str,
    reported_by_profile: bool,
    read: impl FnOnce(&Value) -> Option<T>,
) -> Reported<T> {
    match state.pointer(pointer) {
        None if !reported_by_profile => Reported::Unsupported,
        found => extract(found, read),
    }
}

fn boolean(state: &Value, pointer: &str) -> Reported<bool> {
    extract(state.pointer(pointer), Value::as_bool)
}

fn text(state: &Value, pointer: &str) -> Reported<String> {
    extract(state.pointer(pointer), |value| {
        value.as_str().map(str::to_owned)
    })
}

/// Text for which the device reports an empty string when it could not determine the value, such as a version it failed to read.
fn determined_text(state: &Value, pointer: &str) -> Reported<String> {
    match text(state, pointer) {
        Reported::Supported(value) if value.is_empty() => Reported::Unknown,
        other => other,
    }
}

fn count32(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|count| u32::try_from(count).ok())
}

fn number32(state: &Value, pointer: &str) -> Reported<u32> {
    extract(state.pointer(pointer), count32)
}

fn number64(state: &Value, pointer: &str) -> Reported<u64> {
    extract(state.pointer(pointer), Value::as_u64)
}

fn nested<T>(state: &Value, pointer: &str, parse: impl FnOnce(&Value) -> T) -> Reported<T> {
    match state.pointer(pointer) {
        Some(inner) if inner.is_object() => Reported::Supported(parse(inner)),
        _ => Reported::Unknown,
    }
}

/// Entries of an object keyed by name, such as GPIO channels or stored images, in name order.
fn entries<T>(
    state: &Value,
    pointer: &str,
    parse: impl Fn(String, &Value) -> T,
) -> Reported<Vec<T>> {
    match state.pointer(pointer).and_then(Value::as_object) {
        Some(entries) => Reported::Supported(
            entries
                .iter()
                .map(|(name, entry)| parse(name.clone(), entry))
                .collect(),
        ),
        None => Reported::Unknown,
    }
}

fn system_info(profile: DeviceProfile, state: &Value) -> SystemInfo {
    // PiKVM's default legacy info response moves the platform under `hw`; GLKVM's leaves it under `system`.
    let platform = state
        .pointer("/system/platform")
        .or_else(|| state.pointer("/hw/platform"));
    SystemInfo {
        kvmd_version: determined_text(state, "/system/kvmd/version"),
        streamer_app: determined_text(state, "/system/streamer/app"),
        streamer_version: match profile {
            DeviceProfile::PiKvm => determined_text(state, "/system/streamer/version"),
            // GLKVM's system information returns a fixed version string instead of asking the streamer.
            DeviceProfile::GlKvm => Reported::Unknown,
        },
        kernel: nested(state, "/system/kernel", |kernel| KernelInfo {
            system: text(kernel, "/system"),
            release: text(kernel, "/release"),
            version: text(kernel, "/version"),
            machine: text(kernel, "/machine"),
        }),
        platform: match platform {
            Some(platform) if platform.is_object() => Reported::Supported(PlatformInfo {
                kind: match profile {
                    DeviceProfile::PiKvm => text(platform, "/type"),
                    // GLKVM's system information reports the fixed type `rpi` whatever the hardware.
                    DeviceProfile::GlKvm => Reported::Unknown,
                },
                base: text(platform, "/base"),
                serial: text(platform, "/serial"),
                model: text(platform, "/model"),
                video: text(platform, "/video"),
                board: text(platform, "/board"),
            }),
            _ => Reported::Unknown,
        },
        host: determined_text(state, "/meta/server/host"),
    }
}

fn hid_outputs(device: &Value) -> Reported<HidOutputs> {
    let available = device
        .pointer("/outputs/available")
        .and_then(Value::as_array)
        .and_then(|outputs| {
            outputs
                .iter()
                .map(|output| output.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        });
    let active = match device.pointer("/outputs/active") {
        Some(Value::String(active)) if active.is_empty() => Some(None),
        Some(Value::String(active)) => Some(Some(active.clone())),
        _ => None,
    };
    match (available, active) {
        (Some(available), Some(active)) => Reported::Supported(HidOutputs { available, active }),
        _ => Reported::Unknown,
    }
}

fn hid_status(state: &Value) -> HidStatus {
    HidStatus {
        online: boolean(state, "/online"),
        busy: boolean(state, "/busy"),
        // PiKVM's OTG plugin reports `null` here, which is unknown rather than disconnected.
        connected: boolean(state, "/connected"),
        keyboard: nested(state, "/keyboard", |keyboard| {
            let online = boolean(keyboard, "/online");
            KeyboardStatus {
                // The LEDs start as all off and are only updated by the attached host while the keyboard is online, so offline they are not an observation.
                leds: if online == Reported::Supported(true) {
                    nested(keyboard, "/leds", |leds| KeyboardLeds {
                        caps: boolean(leds, "/caps"),
                        scroll: boolean(leds, "/scroll"),
                        num: boolean(leds, "/num"),
                    })
                } else {
                    Reported::Unknown
                },
                online,
                outputs: hid_outputs(keyboard),
            }
        }),
        mouse: nested(state, "/mouse", |mouse| MouseStatus {
            online: boolean(mouse, "/online"),
            // PiKVM's MCU HID reports `absolute: true` while the whole HID is offline, so the mode is an observation only while the HID reports itself online. The OTG plugins always report themselves online and report the configured gadget mode whatever the attached host does, so their mode is kept. Capability discovery reads the mode on its own, so this does not change the absolute-move check.
            mode: if boolean(state, "/online") == Reported::Supported(true) {
                mouse_mode(state)
            } else {
                MouseMode::Unknown
            },
            outputs: hid_outputs(mouse),
        }),
        jiggler: nested(state, "/jiggler", |jiggler| JigglerStatus {
            enabled: boolean(jiggler, "/enabled"),
            active: boolean(jiggler, "/active"),
            interval_seconds: number64(jiggler, "/interval"),
        }),
    }
}

/// A streamer parameter that is absent because the device reports its feature unavailable is unsupported.
fn parameter<T>(feature: &Reported<bool>, found: Reported<T>) -> Reported<T> {
    match (feature, found) {
        (Reported::Supported(false), Reported::Unknown) => Reported::Unsupported,
        (_, found) => found,
    }
}

fn limits(state: &Value, pointer: &str) -> Reported<Limits> {
    let range = state.pointer(pointer);
    let bound = |name: &str| range.and_then(|range| range.get(name)).and_then(count32);
    match (bound("min"), bound("max")) {
        (Some(min), Some(max)) => Reported::Supported(Limits { min, max }),
        _ => Reported::Unknown,
    }
}

fn streamer_status(profile: DeviceProfile, state: &Value) -> StreamerStatus {
    let gl = profile == DeviceProfile::GlKvm;
    let feature = |name: &str, reported_by_profile: bool| {
        profile_field(
            state,
            &format!("/features/{name}"),
            reported_by_profile,
            Value::as_bool,
        )
    };
    let features = StreamerFeatures {
        quality: feature("quality", true),
        resolution: feature("resolution", true),
        h264: feature("h264", true),
        h265: feature("h265", gl),
        zero_delay: feature("zero_delay", gl),
        venc_mode: feature("venc_mode", gl),
    };
    // uStreamer's own state, which the device reports as `null` while the streamer is not running.
    let running = state
        .get("streamer")
        .filter(|streamer| streamer.is_object());
    StreamerStatus {
        desired_fps: number32(state, "/params/desired_fps"),
        desired_fps_limits: limits(state, "/limits/desired_fps"),
        quality: parameter(&features.quality, number32(state, "/params/quality")),
        resolution: parameter(&features.resolution, text(state, "/params/resolution")),
        available_resolutions: parameter(
            &features.resolution,
            extract(state.pointer("/limits/available_resolutions"), |list| {
                list.as_array()?
                    .iter()
                    .map(|entry| entry.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            }),
        ),
        h264_bitrate: parameter(&features.h264, number32(state, "/params/h264_bitrate")),
        h264_gop: parameter(&features.h264, number32(state, "/params/h264_gop")),
        encoder: running.map_or(Reported::Unknown, |streamer| {
            text(streamer, "/encoder/type")
        }),
        source: running.map_or(Reported::Unknown, |streamer| {
            nested(streamer, "/source", |source| {
                let online = boolean(source, "/online");
                VideoSource {
                    // Without a signal uStreamer still reports a resolution, which is then not the source's.
                    resolution: if online == Reported::Supported(true) {
                        let width = source.pointer("/resolution/width").and_then(count32);
                        let height = source.pointer("/resolution/height").and_then(count32);
                        match (width, height) {
                            (Some(width), Some(height)) => {
                                Reported::Supported(Resolution { width, height })
                            }
                            _ => Reported::Unknown,
                        }
                    } else {
                        Reported::Unknown
                    },
                    online,
                    captured_fps: number32(source, "/captured_fps"),
                }
            })
        }),
        clients: running.map_or(Reported::Unknown, |streamer| {
            number32(streamer, "/stream/clients")
        }),
        features,
    }
}

fn atx_status(profile: DeviceProfile, state: &Value) -> AtxStatus {
    let pikvm = profile == DeviceProfile::PiKvm;
    AtxStatus {
        busy: boolean(state, "/busy"),
        power_led: boolean(state, "/leds/power"),
        hdd_led: boolean(state, "/leds/hdd"),
        power_busy: profile_field(state, "/acts/power", pikvm, Value::as_bool),
        reset_busy: profile_field(state, "/acts/reset", pikvm, Value::as_bool),
    }
}

/// A switch whose port list is empty is unsupported and one without a port list is unknown, as in capability discovery: PiKVM reports an empty list, with its initial `active_port: -1` and `synced: true`, when no switch is attached or its units are not ready.
fn switch_status(profile: DeviceProfile, state: &Value) -> Reported<SwitchStatus> {
    // As in capability discovery, a response without a port list says nothing about whether a switch exists.
    match state.pointer("/model/ports").and_then(Value::as_array) {
        None => return Reported::Unknown,
        Some(ports) if ports.is_empty() => return Reported::Unsupported,
        Some(_) => {}
    }
    Reported::Supported(SwitchStatus {
        ports: extract(state.pointer("/model/ports"), |ports| {
            Some(
                (0_u32..)
                    .zip(ports.as_array()?)
                    .map(|(port, entry)| SwitchPort {
                        port,
                        unit: number32(entry, "/unit"),
                        channel: number32(entry, "/channel"),
                        id: text(entry, "/id"),
                        name: text(entry, "/name"),
                    })
                    .collect::<Vec<_>>(),
            )
        }),
        // The device reports a negative port when none is selected.
        active_port: extract(state.pointer("/summary/active_port"), |port| {
            let port = port.as_i64()?;
            if port < 0 {
                Some(None)
            } else {
                u32::try_from(port).ok().map(Some)
            }
        }),
        synced: match profile {
            DeviceProfile::PiKvm => boolean(state, "/summary/synced"),
            // GLKVM's switch state always reports `true`, which is not an observation of synchronisation.
            DeviceProfile::GlKvm => Reported::Unknown,
        },
    })
}

/// GPIO is supported when the device lists a channel, unsupported when it lists no output and no input, and unknown when it does not report its channels. Capability discovery judges the GPIO control, which only outputs accept, so a device with inputs and no outputs has an unsupported GPIO capability but a supported GPIO status that lists its inputs.
fn gpio_status(state: &Value) -> Reported<GpioStatus> {
    let channels = |pointer: &str| {
        state
            .pointer(pointer)
            .and_then(Value::as_object)
            .map(serde_json::Map::len)
    };
    match (channels("/state/inputs"), channels("/state/outputs")) {
        (Some(inputs), Some(outputs)) if inputs > 0 || outputs > 0 => {}
        (Some(inputs), None) if inputs > 0 => {}
        (None, Some(outputs)) if outputs > 0 => {}
        (None | Some(0), Some(0)) => return Reported::Unsupported,
        _ => return Reported::Unknown,
    }
    Reported::Supported(gpio_channels(state))
}

fn gpio_channels(state: &Value) -> GpioStatus {
    // The device reports `state: false` for an offline or busy channel without reading it.
    let read_state = |channel: &Value, readable: bool| {
        if readable {
            boolean(channel, "/state")
        } else {
            Reported::Unknown
        }
    };
    GpioStatus {
        inputs: entries(state, "/state/inputs", |channel, input| {
            let online = boolean(input, "/online");
            GpioInput {
                channel,
                state: read_state(input, online == Reported::Supported(true)),
                online,
            }
        }),
        outputs: entries(state, "/state/outputs", |channel, output| {
            let online = boolean(output, "/online");
            let busy = boolean(output, "/busy");
            GpioOutput {
                state: read_state(
                    output,
                    online == Reported::Supported(true) && busy == Reported::Supported(false),
                ),
                switchable: extract(
                    state
                        .pointer("/model/scheme/outputs")
                        .and_then(|outputs| outputs.get(&channel))
                        .and_then(|scheme| scheme.get("switch")),
                    Value::as_bool,
                ),
                channel,
                online,
                busy,
            }
        }),
    }
}

fn media_image(name: String, image: &Value) -> MediaImage {
    MediaImage {
        name,
        size: number64(image, "/size"),
        complete: boolean(image, "/complete"),
        removable: boolean(image, "/removable"),
    }
}

/// A virtual drive; the device reports `null` while the drive is unavailable. A remote image's URL is not read, since it can embed credentials.
fn media_drive(drive: Option<&Value>) -> Reported<MediaDrive> {
    let Some(drive) = drive.filter(|drive| drive.is_object()) else {
        return Reported::Unknown;
    };
    Reported::Supported(MediaDrive {
        connected: boolean(drive, "/connected"),
        cdrom: boolean(drive, "/cdrom"),
        rw: boolean(drive, "/rw"),
        image: match drive.get("image") {
            Some(Value::Null) => Reported::Supported(None),
            Some(image) => extract(image.get("name"), |name| {
                Some(Some(media_image(name.as_str()?.to_owned(), image)))
            }),
            None => Reported::Unknown,
        },
    })
}

fn media_status(profile: DeviceProfile, state: &Value) -> MediaStatus {
    MediaStatus {
        online: boolean(state, "/online"),
        busy: boolean(state, "/busy"),
        drive: media_drive(state.get("drive")),
        partition_drive: match state.get("drive_partition") {
            None if profile == DeviceProfile::PiKvm => Reported::Unsupported,
            found => media_drive(found),
        },
        images: entries(state, "/storage/images", media_image),
        partitions: entries(state, "/storage/parts", |name, part| StoragePartition {
            name,
            size: number64(part, "/size"),
            free: number64(part, "/free"),
            writable: boolean(part, "/writable"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvm_core::{Capability, MouseMode};
    use proptest::{collection::vec, prelude::*};
    use serde_json::json;

    /// Synthetic responses shaped after each profile's inspected source, shared with the HTTPS contract tests.
    fn fixture(profile: DeviceProfile) -> Value {
        serde_json::from_str(match profile {
            DeviceProfile::PiKvm => include_str!("../tests/fixtures/pikvm-status.json"),
            DeviceProfile::GlKvm => include_str!("../tests/fixtures/glkvm-status.json"),
        })
        .unwrap()
    }

    fn reads(fixture: &Value) -> [Read; 7] {
        ROUTES.map(|route| Read::State(fixture[route].clone()))
    }

    fn parsed(profile: DeviceProfile) -> DeviceStatus {
        parse(profile, reads(&fixture(profile))).unwrap()
    }

    fn supported<T: std::fmt::Debug>(reported: &Reported<T>) -> &T {
        reported
            .value()
            .unwrap_or_else(|| panic!("expected a reported value, found {reported:?}"))
    }

    fn some(value: &str) -> Reported<String> {
        Reported::Supported(value.to_owned())
    }

    #[test]
    fn pikvm_status_is_typed() {
        let status = parsed(DeviceProfile::PiKvm);
        let info = supported(&status.info);
        assert_eq!(info.kvmd_version, some("0.0.0-synthetic"));
        assert_eq!(info.streamer_app, some("ustreamer"));
        assert_eq!(supported(&info.kernel).machine, some("aarch64"));
        // PiKVM's legacy info response carries the platform under `hw`.
        assert_eq!(supported(&info.platform).model, some("synthetic"));
        assert_eq!(supported(&info.platform).kind, some("rpi"));
        assert_eq!(info.host, some("pikvm.example"));

        let hid = supported(&status.hid);
        assert_eq!(hid.connected, Reported::Unknown);
        let keyboard = supported(&hid.keyboard);
        assert_eq!(supported(&keyboard.leds).num, Reported::Supported(true));
        assert_eq!(supported(&keyboard.outputs).active, None);
        let mouse = supported(&hid.mouse);
        assert_eq!(mouse.mode, MouseMode::Absolute);
        assert_eq!(
            *supported(&mouse.outputs),
            HidOutputs {
                available: vec!["usb".into(), "usb_rel".into()],
                active: Some("usb".into()),
            }
        );
        assert_eq!(
            supported(&hid.jiggler).interval_seconds,
            Reported::Supported(60)
        );

        let streamer = supported(&status.streamer);
        assert_eq!(streamer.features.resolution, Reported::Supported(false));
        assert_eq!(streamer.resolution, Reported::Unsupported);
        assert_eq!(streamer.available_resolutions, Reported::Unsupported);
        assert_eq!(streamer.features.h265, Reported::Unsupported);
        assert_eq!(streamer.quality, Reported::Supported(80));
        assert_eq!(
            streamer.desired_fps_limits,
            Reported::Supported(Limits { min: 0, max: 70 })
        );
        assert_eq!(streamer.encoder, some("M2M-IMAGE"));
        let source = supported(&streamer.source);
        assert_eq!(
            source.resolution,
            Reported::Supported(Resolution {
                width: 1920,
                height: 1080
            })
        );
        assert_eq!(source.captured_fps, Reported::Supported(39));
        assert_eq!(streamer.clients, Reported::Supported(1));

        let atx = supported(&status.atx);
        assert_eq!(atx.power_led, Reported::Supported(true));
        assert_eq!(atx.hdd_led, Reported::Supported(false));
        assert_eq!(atx.power_busy, Reported::Supported(false));

        let switch = supported(&status.switch);
        let ports = supported(&switch.ports);
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].name, some("Server"));
        assert_eq!(ports[1].port, 1);
        assert_eq!(ports[1].channel, Reported::Supported(1));
        assert_eq!(ports[1].id, some("2"));
        assert_eq!(switch.active_port, Reported::Supported(Some(1)));
        assert_eq!(switch.synced, Reported::Supported(true));

        let gpio = supported(&status.gpio);
        let inputs = supported(&gpio.inputs);
        assert_eq!(inputs[0].channel, "led");
        assert_eq!(inputs[0].state, Reported::Supported(true));
        let outputs = supported(&gpio.outputs);
        assert_eq!(outputs[0].channel, "button");
        assert_eq!(outputs[0].busy, Reported::Supported(true));
        assert_eq!(outputs[0].state, Reported::Unknown);
        assert_eq!(outputs[0].switchable, Reported::Supported(false));
        assert_eq!(outputs[1].channel, "relay");
        assert_eq!(outputs[1].state, Reported::Supported(true));
        assert_eq!(outputs[1].switchable, Reported::Supported(true));

        let media = supported(&status.media);
        let drive = supported(&media.drive);
        assert_eq!(drive.connected, Reported::Supported(true));
        assert_eq!(drive.cdrom, Reported::Supported(true));
        let image = supported(&drive.image).as_ref().unwrap();
        assert_eq!(image.name, "remote.iso");
        assert_eq!(image.size, Reported::Supported(2048));
        assert_eq!(media.partition_drive, Reported::Unsupported);
        let images = supported(&media.images);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].name, "disk.iso");
        assert_eq!(images[0].complete, Reported::Supported(true));
        let partitions = supported(&media.partitions);
        assert_eq!(partitions[0].name, "");
        assert_eq!(partitions[0].free, Reported::Supported(4_294_967_296));
    }

    #[test]
    fn glkvm_status_is_typed() {
        let status = parsed(DeviceProfile::GlKvm);
        let info = supported(&status.info);
        // GLKVM reports a fixed streamer version rather than the running streamer's.
        assert_eq!(info.streamer_version, Reported::Unknown);
        assert_eq!(info.streamer_app, some("ustreamer"));
        // GLKVM's info response carries the platform under `system`, with `null` for fields it could not read.
        let platform = supported(&info.platform);
        assert_eq!(platform.serial, some("1111111111111111"));
        assert_eq!(platform.model, Reported::Unknown);
        // GLKVM reports the fixed platform type `rpi` whatever the hardware.
        assert_eq!(platform.kind, Reported::Unknown);
        assert_eq!(info.host, some("glkvm.example"));

        let hid = supported(&status.hid);
        assert_eq!(hid.connected, Reported::Supported(true));
        let mouse = supported(&hid.mouse);
        assert_eq!(mouse.online, Reported::Supported(false));
        // An OTG HID reports itself online and its configured gadget mode whatever the attached host does, so the mode is kept.
        assert_eq!(mouse.mode, MouseMode::Relative);
        assert_eq!(supported(&hid.jiggler).active, Reported::Supported(true));

        let streamer = supported(&status.streamer);
        assert_eq!(streamer.features.h265, Reported::Supported(true));
        assert_eq!(streamer.features.venc_mode, Reported::Supported(true));
        assert_eq!(streamer.resolution, some("1920x1080"));
        assert_eq!(
            streamer.available_resolutions,
            Reported::Supported(vec!["1920x1080".into(), "1280x720".into()])
        );
        // The streamer is not running, so its capture state is unobserved.
        assert_eq!(streamer.encoder, Reported::Unknown);
        assert_eq!(streamer.source, Reported::Unknown);
        assert_eq!(streamer.clients, Reported::Unknown);

        let atx = supported(&status.atx);
        assert_eq!(atx.busy, Reported::Supported(true));
        assert_eq!(atx.hdd_led, Reported::Supported(true));
        assert_eq!(atx.power_busy, Reported::Unsupported);
        assert_eq!(atx.reset_busy, Reported::Unsupported);

        let switch = supported(&status.switch);
        let ports = supported(&switch.ports);
        assert_eq!(ports.len(), 4);
        assert_eq!(ports[1].name, some("Lab"));
        assert_eq!(ports[3].id, some("1.4"));
        assert_eq!(switch.active_port, Reported::Supported(None));
        assert_eq!(switch.synced, Reported::Unknown);

        // No channel is configured, which capability discovery also reports as unsupported.
        assert_eq!(status.gpio, Reported::Unsupported);

        let media = supported(&status.media);
        assert_eq!(supported(&media.drive).image, Reported::Supported(None));
        let partition = supported(&media.partition_drive);
        assert_eq!(partition.rw, Reported::Supported(true));
        assert_eq!(supported(&partition.image).as_ref().unwrap().name, "sda1");
        assert_eq!(
            supported(&media.images)[0].complete,
            Reported::Supported(false)
        );
    }

    #[test]
    fn absent_disabled_and_unreadable_subsystems_are_distinguished() {
        let status = parse(
            DeviceProfile::PiKvm,
            [
                Read::State(json!("not an object")),
                Read::State(json!({"enabled": true})),
                Read::Denied,
                Read::State(
                    json!({"enabled": false, "busy": false, "acts": {"power": false, "reset": false}, "leds": {"power": false, "hdd": false}}),
                ),
                Read::Absent,
                Read::State(json!({})),
                Read::State(
                    json!({"enabled": false, "online": false, "busy": false, "storage": null, "drive": null}),
                ),
            ],
        )
        .unwrap();
        assert_eq!(status.info, Reported::Unknown);
        let hid = supported(&status.hid);
        assert_eq!(hid.online, Reported::Unknown);
        assert_eq!(hid.keyboard, Reported::Unknown);
        assert_eq!(hid.mouse, Reported::Unknown);
        assert_eq!(status.streamer, Reported::Unknown);
        // A disabled ATX plugin reports its LEDs off; that is a placeholder, not an observation.
        assert_eq!(status.atx, Reported::Unsupported);
        assert_eq!(status.switch, Reported::Unsupported);
        // A GPIO response that lists no channels says nothing either way, as in capability discovery.
        assert_eq!(status.gpio, Reported::Unknown);
        assert_eq!(status.media, Reported::Unsupported);

        let status = parse(
            DeviceProfile::GlKvm,
            ROUTES.map(|route| match route {
                "info" | "hid" | "streamer" => Read::State(json!({})),
                "atx" => Read::State(json!({"leds": {"power": true}})),
                "msd" => Read::State(json!({"online": true})),
                _ => Read::Absent,
            }),
        )
        .unwrap();
        // A subsystem that does not say whether it is enabled is unknown, as in capability discovery.
        assert_eq!(status.atx, Reported::Unknown);
        assert_eq!(status.media, Reported::Unknown);
        assert_eq!(status.hid, Reported::Unknown);
        // The info route was read, so absent routes are subsystems the device does not have.
        assert_eq!(status.switch, Reported::Unsupported);
        assert_eq!(status.gpio, Reported::Unsupported);

        // Without the info route an absent route is not evidence of a missing subsystem.
        let status = parse(
            DeviceProfile::PiKvm,
            ROUTES.map(|route| match route {
                "info" => Read::Failed(Error::Http(500)),
                "hid" | "streamer" => Read::State(json!({})),
                _ => Read::Absent,
            }),
        )
        .unwrap();
        assert_eq!(status.info, Reported::Unknown);
        assert_eq!(status.atx, Reported::Unknown);
        assert_eq!(status.switch, Reported::Unknown);
        assert_eq!(status.media, Reported::Unknown);
    }

    #[test]
    fn a_status_that_observed_nothing_fails() {
        // A proxy whose daemon is stopped answers every route with HTTP 502.
        assert!(matches!(
            parse(
                DeviceProfile::PiKvm,
                ROUTES.map(|_| Read::Failed(Error::Http(502)))
            ),
            Err(Error::Http(502))
        ));
        // A page that is not the API answers with something that is not JSON; the first route's error is reported.
        let mut observed: [Read; 7] =
            std::array::from_fn(|_| Read::Failed(Error::InvalidResponse("not JSON".into())));
        observed[0] = Read::Failed(Error::Http(500));
        assert!(matches!(
            parse(DeviceProfile::GlKvm, observed),
            Err(Error::Http(500))
        ));
        // Info failing with every other route absent is not a KVMD API either.
        let observed = ROUTES.map(|route| match route {
            "info" => Read::Failed(Error::Http(500)),
            _ => Read::Absent,
        });
        assert!(matches!(
            parse(DeviceProfile::PiKvm, observed),
            Err(Error::Http(404))
        ));
        // Both sources register the HID and streamer routes, so a 404 on either is not a KVMD API.
        for index in [1, 2] {
            let mut observed = reads(&fixture(DeviceProfile::PiKvm));
            observed[index] = Read::Absent;
            assert!(matches!(
                parse(DeviceProfile::PiKvm, observed),
                Err(Error::Http(404))
            ));
        }
    }

    #[test]
    fn a_failed_route_is_unknown_without_discarding_the_others() {
        for (index, route) in ROUTES.iter().enumerate() {
            let responses = fixture(DeviceProfile::PiKvm);
            let mut reads = reads(&responses);
            reads[index] = Read::Failed(Error::Http(500));
            let status = parse(DeviceProfile::PiKvm, reads).unwrap();
            let capabilities = [
                status.info.capability(),
                status.hid.capability(),
                status.streamer.capability(),
                status.atx.capability(),
                status.switch.capability(),
                status.gpio.capability(),
                status.media.capability(),
            ];
            for (other, capability) in capabilities.iter().enumerate() {
                let expected = if other == index {
                    Capability::Unknown
                } else {
                    Capability::Supported
                };
                assert_eq!(
                    *capability, expected,
                    "{route} failed, {} read",
                    ROUTES[other]
                );
            }
        }
        // With nothing read, a denial takes precedence over failures: the session has observed nothing.
        let mut failed: [Read; 7] = std::array::from_fn(|_| Read::Failed(Error::Http(502)));
        failed[2] = Read::Denied;
        assert!(matches!(
            parse(DeviceProfile::GlKvm, failed),
            Err(Error::Authentication)
        ));
    }

    // Breaks if the parser starts reading a key that the generated responses never contain, which would leave that branch reachable only through its unknown case.
    #[test]
    fn fuzz_keys_cover_every_key_the_parser_reads() {
        let source = include_str!("status.rs");
        let parser = &source[..source.find("#[cfg(test)]").unwrap()];
        let keys: Vec<&str> = KEYS.split(' ').collect();
        let mut missing = Vec::new();
        // The route list names URL paths, not object keys; every other literal in the parser is a key or a JSON pointer of keys.
        for line in parser.lines().filter(|line| {
            let line = line.trim_start();
            !line.starts_with("//") && !line.starts_with("const ROUTES")
        }) {
            // String literals are the odd-numbered pieces between quotes; the parser's literals contain no escaped quotes.
            for literal in line.split('"').skip(1).step_by(2) {
                for key in literal.split('/') {
                    let is_key = !key.is_empty()
                        && key
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit());
                    if is_key && !keys.contains(&key) {
                        missing.push(key.to_owned());
                    }
                }
            }
        }
        // `mouse_mode`, shared with capability discovery, reads `/mouse/absolute`.
        assert!(keys.contains(&"absolute"));
        assert!(
            missing.is_empty(),
            "keys the parser reads but KEYS omits: {missing:?}"
        );
    }

    #[test]
    fn subsystem_support_agrees_with_capability_discovery() {
        for profile in [DeviceProfile::PiKvm, DeviceProfile::GlKvm] {
            // Discovery reads a switch as supported only when it lists ports.
            assert_eq!(switch_status(profile, &json!({})), Reported::Unknown);
            assert_eq!(
                switch_status(profile, &json!({"model": {"ports": {}}})),
                Reported::Unknown
            );
            assert!(matches!(
                switch_status(profile, &json!({"model": {"ports": [{}]}})),
                Reported::Supported(_)
            ));
        }
        assert_eq!(gpio_status(&json!({})), Reported::Unknown);
        assert_eq!(
            gpio_status(&json!({"state": {"inputs": {}, "outputs": {}}})),
            Reported::Unsupported
        );
        assert_eq!(
            gpio_status(&json!({"state": {"outputs": {}}})),
            Reported::Unsupported
        );
        assert_eq!(
            gpio_status(&json!({"state": {"inputs": {}}})),
            Reported::Unknown
        );
        let outputs = gpio_status(&json!({"state": {"outputs": {"relay": {}}}}));
        assert_eq!(supported(&outputs).inputs, Reported::Unknown);
        assert_eq!(supported(&outputs).outputs.value().unwrap().len(), 1);
        // Inputs alone are readable state, although the GPIO control (outputs only) is unsupported.
        let inputs = gpio_status(&json!({"state": {"inputs": {"led": {}}, "outputs": {}}}));
        assert_eq!(supported(&inputs).inputs.value().unwrap().len(), 1);
        assert_eq!(supported(&inputs).outputs, Reported::Supported(Vec::new()));
    }

    #[test]
    fn mouse_mode_is_reported_only_while_the_hid_is_online() {
        // (HID online, mouse online, absolute, expected mode)
        for (hid_online, mouse_online, absolute, expected) in [
            (json!(true), json!(true), json!(true), MouseMode::Absolute),
            (json!(true), json!(true), json!(false), MouseMode::Relative),
            // An OTG HID with no host attached: the gadget mode is still configured.
            (json!(true), json!(false), json!(false), MouseMode::Relative),
            (json!(true), json!(false), json!(true), MouseMode::Absolute),
            // PiKVM's MCU HID while offline reports absolute as a placeholder.
            (json!(false), json!(false), json!(true), MouseMode::Unknown),
            (Value::Null, json!(true), json!(false), MouseMode::Unknown),
        ] {
            let hid = hid_status(&json!({
                "online": hid_online,
                "mouse": {"online": mouse_online, "absolute": absolute},
            }));
            assert_eq!(supported(&hid.mouse).mode, expected);
        }
    }

    #[test]
    fn an_empty_switch_inventory_is_unsupported() {
        // PiKVM's switch state before any unit is ready, which is also what a device without a switch serves.
        let responses = json!({"model": {"firmware": {"version": 7}, "units": [], "ports": [], "limits": {}}, "summary": {"active_port": -1, "active_id": "", "synced": true}});
        for profile in [DeviceProfile::PiKvm, DeviceProfile::GlKvm] {
            assert_eq!(switch_status(profile, &responses), Reported::Unsupported);
        }
        let mut responses = fixture(DeviceProfile::PiKvm);
        responses["switch"]["model"]["ports"] = json!([]);
        let status = parse(DeviceProfile::PiKvm, reads(&responses)).unwrap();
        assert_eq!(status.switch, Reported::Unsupported);
    }

    #[test]
    fn placeholder_values_of_offline_devices_are_unknown() {
        let mut responses = fixture(DeviceProfile::PiKvm);
        responses["hid"]["keyboard"]["online"] = json!(false);
        responses["streamer"]["streamer"]["source"]["online"] = json!(false);
        responses["streamer"]["streamer"]["source"]["captured_fps"] = json!(0);
        let status = parse(DeviceProfile::PiKvm, reads(&responses)).unwrap();
        let keyboard = supported(&supported(&status.hid).keyboard);
        assert_eq!(keyboard.online, Reported::Supported(false));
        assert_eq!(keyboard.leds, Reported::Unknown);
        let source = supported(&supported(&status.streamer).source);
        assert_eq!(source.online, Reported::Supported(false));
        assert_eq!(source.resolution, Reported::Unknown);
        // A source without a signal genuinely captures no frames.
        assert_eq!(source.captured_fps, Reported::Supported(0));
    }

    #[test]
    fn a_status_with_nothing_readable_is_an_authentication_failure() {
        assert!(matches!(
            parse(DeviceProfile::PiKvm, Default::default()),
            Err(Error::Authentication)
        ));
        let mut mixed: [Read; 7] = Default::default();
        mixed[3] = Read::Absent;
        mixed[4] = Read::Failed(Error::Http(500));
        assert!(matches!(
            parse(DeviceProfile::GlKvm, mixed),
            Err(Error::Authentication)
        ));
        let mut partial: [Read; 7] = Default::default();
        partial[1] = Read::State(json!({"enabled": true}));
        let status = parse(DeviceProfile::PiKvm, partial).unwrap();
        assert_eq!(status.hid.capability(), Capability::Supported);
        assert_eq!(status.info, Reported::Unknown);
        // Both sources always serve the info route, so its absence means the endpoint is not a KVMD API.
        let mut info_absent = reads(&fixture(DeviceProfile::GlKvm));
        info_absent[0] = Read::Absent;
        assert!(matches!(
            parse(DeviceProfile::GlKvm, info_absent),
            Err(Error::Http(404))
        ));
        assert!(matches!(
            parse(DeviceProfile::PiKvm, ROUTES.map(|_| Read::Absent)),
            Err(Error::Http(404))
        ));
    }

    #[test]
    fn credential_bearing_device_values_are_not_carried() {
        for profile in [DeviceProfile::PiKvm, DeviceProfile::GlKvm] {
            let status = parsed(profile);
            assert!(!format!("{status:?}").contains("SECRET"));
            assert!(!serde_json::to_string(&status).unwrap().contains("SECRET"));
        }
    }

    /// Every object key the parser reads, at any depth, so generated responses can reach its supported leaves rather than only its unknown cases. `fuzz_keys_cover_every_key_the_parser_reads` checks the list against the parser's source.
    const KEYS: &str = "enabled online busy connected state leds power hdd caps scroll num acts reset model ports unit channel id name summary active_port synced inputs outputs scheme switch drive drive_partition image cdrom rw size complete removable free writable storage images parts system kvmd version streamer app kernel release machine platform type base serial video board hw meta server host keyboard mouse absolute available active jiggler interval features quality resolution h264 h265 zero_delay venc_mode params desired_fps h264_bitrate h264_gop limits available_resolutions min max encoder source width height captured_fps stream clients";

    fn json_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            any::<u64>().prop_map(Value::from),
            "[a-z_]{0,8}".prop_map(Value::from),
        ];
        leaf.prop_recursive(4, 48, 6, |inner| {
            prop_oneof![
                vec(inner.clone(), 0..4).prop_map(Value::Array),
                proptest::collection::btree_map(
                    proptest::sample::select(
                        KEYS.split(' ').map(str::to_owned).collect::<Vec<_>>(),
                    ),
                    inner,
                    0..6,
                )
                .prop_map(|entries| Value::Object(entries.into_iter().collect())),
            ]
        })
    }

    fn profile() -> impl Strategy<Value = DeviceProfile> {
        proptest::sample::select(vec![DeviceProfile::PiKvm, DeviceProfile::GlKvm])
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        // Breaks if an unexpected response shape panics instead of becoming unknown, or if a parsed status fails to survive serialisation.
        #[test]
        fn arbitrary_responses_parse_and_round_trip(
            responses in vec(json_value(), 7),
            profile in profile(),
        ) {
            let reads: [Read; 7] = std::array::from_fn(|index| Read::State(responses[index].clone()));
            let status = parse(profile, reads).unwrap();
            let encoded = serde_json::to_value(&status).unwrap();
            let decoded: DeviceStatus = serde_json::from_value(encoded).unwrap();
            prop_assert_eq!(decoded, status.clone());
            for (route, response) in ROUTES.iter().zip(&responses) {
                if !response.is_object() {
                    let capability = match *route {
                        "info" => status.info.capability(),
                        "hid" => status.hid.capability(),
                        "streamer" => status.streamer.capability(),
                        "atx" => status.atx.capability(),
                        "switch" => status.switch.capability(),
                        "gpio" => status.gpio.capability(),
                        _ => status.media.capability(),
                    };
                    prop_assert_eq!(capability, Capability::Unknown, "{}", route);
                }
            }
        }

        // Breaks if a disabled ATX plugin's placeholder LEDs are reported, or an enabled plugin's LEDs are not carried unchanged.
        #[test]
        fn atx_leds_are_reported_only_when_enabled(
            enabled in any::<bool>(),
            power in any::<bool>(),
            hdd in any::<bool>(),
            profile in profile(),
        ) {
            let state = json!({"enabled": enabled, "busy": false, "leds": {"power": power, "hdd": hdd}});
            let atx = section(Read::State(state), true, |state| enabled_atx(profile, state));
            if enabled {
                let atx = atx.value().unwrap();
                prop_assert_eq!(&atx.power_led, &Reported::Supported(power));
                prop_assert_eq!(&atx.hdd_led, &Reported::Supported(hdd));
                let acts = if profile == DeviceProfile::PiKvm { Reported::Unknown } else { Reported::Unsupported };
                prop_assert_eq!(&atx.power_busy, &acts);
            } else {
                prop_assert_eq!(atx, Reported::Unsupported);
            }
        }

        // Breaks if a GPIO output's placeholder state is reported while it is offline or busy.
        #[test]
        fn gpio_output_state_is_reported_only_when_readable(
            online in proptest::option::of(any::<bool>()),
            busy in proptest::option::of(any::<bool>()),
            value in any::<bool>(),
            switchable in proptest::option::of(any::<bool>()),
        ) {
            let mut output = json!({"state": value});
            if let Some(online) = online {
                output["online"] = online.into();
            }
            if let Some(busy) = busy {
                output["busy"] = busy.into();
            }
            let mut scheme = json!({});
            if let Some(switchable) = switchable {
                scheme["switch"] = switchable.into();
            }
            let gpio = gpio_channels(&json!({
                "model": {"scheme": {"outputs": {"a/b~c": scheme}}},
                "state": {"outputs": {"a/b~c": output}},
            }));
            let outputs = gpio.outputs.value().unwrap();
            prop_assert_eq!(outputs.len(), 1);
            prop_assert_eq!(&outputs[0].channel, "a/b~c");
            let expected = if online == Some(true) && busy == Some(false) {
                Reported::Supported(value)
            } else {
                Reported::Unknown
            };
            prop_assert_eq!(&outputs[0].state, &expected);
            prop_assert_eq!(&outputs[0].switchable, &switchable.map_or(Reported::Unknown, Reported::Supported));
        }

        // Breaks if "no port selected" is confused with a port number, or GLKVM's constant `synced` flag is reported as an observation.
        #[test]
        fn switch_selection_distinguishes_no_port(
            active in any::<i64>(),
            synced in any::<bool>(),
            profile in profile(),
        ) {
            let switch = switch_status(profile, &json!({"model": {"ports": [{}]}, "summary": {"active_port": active, "synced": synced}}));
            let Reported::Supported(switch) = switch else {
                panic!("a switch with a port is supported");
            };
            let expected = if active < 0 {
                Reported::Supported(None)
            } else {
                u32::try_from(active).map_or(Reported::Unknown, |port| Reported::Supported(Some(port)))
            };
            prop_assert_eq!(switch.active_port, expected);
            let ports = switch.ports.value().unwrap();
            prop_assert_eq!(ports.len(), 1);
            prop_assert_eq!(ports[0].port, 0);
            prop_assert_eq!(&ports[0].unit, &Reported::Unknown);
            let synced = if profile == DeviceProfile::PiKvm { Reported::Supported(synced) } else { Reported::Unknown };
            prop_assert_eq!(switch.synced, synced);
        }

        // Breaks if a value the device reports that can embed a credential (a remote image URL, a stream client key, configured extras) reaches the status.
        #[test]
        fn embedded_credentials_never_reach_the_status(
            secret in "SECRET-[a-z0-9]{1,10}",
            profile in profile(),
        ) {
            let mut responses = fixture(profile);
            responses["msd"]["drive"]["image"]["url"] = format!("https://user:{secret}@images.example/").into();
            responses["info"]["extras"] = json!({"synthetic": {"token": secret}});
            responses["streamer"]["streamer"] = json!({"stream": {"clients": 1, "clients_stat": {"1": {"key": secret}}}});
            let status = parse(profile, reads(&responses)).unwrap();
            let debug = format!("{status:?}");
            let encoded = serde_json::to_string(&status).unwrap();
            prop_assert!(!debug.contains(&secret));
            prop_assert!(!encoded.contains(&secret));
        }
    }

    fn enabled_atx(profile: DeviceProfile, state: &Value) -> Reported<AtxStatus> {
        enabled(state, |state| atx_status(profile, state))
    }
}
