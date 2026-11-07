//! Run with `cargo run --manifest-path examples/in-process/Cargo.toml` from the repository root.
//! Test with `cargo test --manifest-path examples/in-process/Cargo.toml`.
//! This consumer uses a synthetic device in process; it needs no hardware or real credentials.

use async_trait::async_trait;
use kvm_core::{
    AuthScheme, Authenticate, CallerIdentity, Capabilities, Capability, Confirmation, Connected,
    Connection, Connector, Control, ControlOutcome, Credentials, Device, DeviceIdentity,
    DeviceProfile, DeviceStatus, Endpoint, Error, LoginState, MouseMode, PlatformInfo, Reported,
    Result, ServerTrust, Snapshot, SwitchPort, SwitchStatus, SystemInfo, VideoStream,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct FakeDevice {
    events: Mutex<Vec<String>>,
    /// Reports the switch status unknown, as a device whose switch route failed does.
    switch_unknown: bool,
    /// Reports the switch's active port unknown, as a device whose switch summary is unreadable does.
    active_unknown: bool,
    /// Reports the switch's port list unsupported.
    no_port_list: bool,
}

struct FakeVideo(bool);

#[async_trait]
impl VideoStream for FakeVideo {
    async fn next_frame(&mut self) -> Result<Option<Snapshot>> {
        if self.0 {
            self.0 = false;
            Ok(Some(Snapshot {
                jpeg: vec![0xff, 0xd8, 0xff, 0xd9],
            }))
        } else {
            Ok(None)
        }
    }
}

#[async_trait]
impl Device for FakeDevice {
    async fn video(&self) -> Result<Box<dyn VideoStream>> {
        Ok(Box::new(FakeVideo(true)))
    }
    async fn status(&self) -> Result<DeviceStatus> {
        let port = |port| SwitchPort {
            port,
            unit: Reported::Supported(0),
            channel: Reported::Supported(port),
            id: Reported::Supported((port + 1).to_string()),
            name: Reported::Unknown,
        };
        Ok(DeviceStatus {
            info: Reported::Supported(SystemInfo {
                kvmd_version: Reported::Unknown,
                streamer_app: Reported::Unknown,
                streamer_version: Reported::Unknown,
                kernel: Reported::Unknown,
                platform: Reported::Supported(PlatformInfo {
                    kind: Reported::Unknown,
                    base: Reported::Unknown,
                    serial: Reported::Supported(SERIAL.into()),
                    model: Reported::Unknown,
                    video: Reported::Unknown,
                    board: Reported::Unknown,
                }),
                host: Reported::Unknown,
            }),
            hid: Reported::Unknown,
            streamer: Reported::Unknown,
            atx: Reported::Unsupported,
            switch: if self.switch_unknown {
                Reported::Unknown
            } else {
                Reported::Supported(SwitchStatus {
                    ports: if self.no_port_list {
                        Reported::Unsupported
                    } else {
                        Reported::Supported(vec![port(0), port(1)])
                    },
                    active_port: if self.active_unknown {
                        Reported::Unknown
                    } else {
                        Reported::Supported(Some(0))
                    },
                    synced: Reported::Supported(true),
                })
            },
            gpio: Reported::Unsupported,
            media: Reported::Unsupported,
        })
    }
    async fn capabilities(&self) -> Result<Capabilities> {
        Ok(Capabilities {
            status: Capability::Supported,
            snapshot: Capability::Supported,
            keyboard: Capability::Supported,
            mouse: Capability::Unsupported,
            mouse_mode: MouseMode::Unknown,
            live_video: Capability::Supported,
            power: Capability::Unsupported,
            switch: Capability::Supported,
            gpio: Capability::Unsupported,
            media: Capability::Unsupported,
        })
    }
    async fn snapshot(&self) -> Result<Snapshot> {
        Ok(Snapshot {
            jpeg: vec![0xff, 0xd8, 0xff, 0xd9],
        })
    }
    async fn key(&self, key: &str, pressed: bool) -> Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(format!("key {key} {pressed}"));
        Ok(())
    }
    async fn mouse_button(&self, _: &str, _: bool) -> Result<()> {
        Err(Error::Unsupported("mouse".into()))
    }
    async fn mouse_move(&self, _: i16, _: i16) -> Result<()> {
        Err(Error::Unsupported("mouse".into()))
    }
    async fn release_inputs(&self) -> Result<()> {
        self.events.lock().unwrap().push("release inputs".into());
        Ok(())
    }
    async fn control(&self, operation: Control, _: Confirmation) -> Result<ControlOutcome> {
        match operation {
            Control::Switch { port } => {
                self.events.lock().unwrap().push(format!("switch {port}"));
                Ok(ControlOutcome::Unverified)
            }
            _ => Err(Error::Unsupported("control".into())),
        }
    }
}

/// The serial number the synthetic device reports.
const SERIAL: &str = "SYNTHETIC-0001";
/// The only credentials the synthetic device accepts.
const USERNAME: &str = "operator";
const PASSWORD: &str = "synthetic-password";

/// A synthetic connection: the device behind a login. Every device call is refused with `Error::Authentication` unless the credentials in use are the device's own, as a device that requires authentication refuses them.
struct FakeConnection {
    device: FakeDevice,
    endpoint: Endpoint,
    /// The caller whose credentials the session uses.
    caller: Mutex<Option<CallerIdentity>>,
    /// Whether the credentials in use are the ones the device accepts.
    accepted: Mutex<bool>,
    /// The caller waiting for approval and how many more approval checks report it pending.
    pending: Mutex<Option<(CallerIdentity, u32)>>,
    /// How many approval checks a session login waits through; zero means no approval is asked for.
    approval_checks: u32,
}

impl FakeConnection {
    /// Fails with `Error::Authentication` unless the credentials in use are accepted.
    fn authenticated(&self) -> Result<&FakeDevice> {
        if *self.accepted.lock().unwrap() {
            Ok(&self.device)
        } else {
            Err(Error::Authentication)
        }
    }

    fn accept(&self, caller: CallerIdentity, accepted: bool) {
        *self.caller.lock().unwrap() = Some(caller);
        *self.accepted.lock().unwrap() = accepted;
    }
}

#[async_trait]
impl Device for FakeConnection {
    async fn video(&self) -> Result<Box<dyn VideoStream>> {
        self.authenticated()?.video().await
    }
    async fn status(&self) -> Result<DeviceStatus> {
        self.authenticated()?.status().await
    }
    async fn capabilities(&self) -> Result<Capabilities> {
        self.authenticated()?.capabilities().await
    }
    async fn snapshot(&self) -> Result<Snapshot> {
        self.authenticated()?.snapshot().await
    }
    async fn key(&self, key: &str, pressed: bool) -> Result<()> {
        self.authenticated()?.key(key, pressed).await
    }
    async fn mouse_button(&self, button: &str, pressed: bool) -> Result<()> {
        self.authenticated()?.mouse_button(button, pressed).await
    }
    async fn mouse_move(&self, x: i16, y: i16) -> Result<()> {
        self.authenticated()?.mouse_move(x, y).await
    }
    async fn release_inputs(&self) -> Result<()> {
        self.authenticated()?.release_inputs().await
    }
    async fn control(
        &self,
        operation: Control,
        confirmation: Confirmation,
    ) -> Result<ControlOutcome> {
        self.authenticated()?.control(operation, confirmation).await
    }
}

#[async_trait]
impl Authenticate for FakeConnection {
    async fn login(&self, credentials: Credentials) -> Result<LoginState> {
        // As in kvm-kvmd, a login that passes its local checks discards the previous session first.
        let per_request = self.endpoint.auth != AuthScheme::Session;
        if per_request && credentials.totp.is_some() {
            return Err(Error::InvalidResponse(
                "a one-time code requires session login".into(),
            ));
        }
        *self.caller.lock().unwrap() = None;
        *self.accepted.lock().unwrap() = false;
        *self.pending.lock().unwrap() = None;
        let valid =
            credentials.caller.username == USERNAME && credentials.password.expose() == PASSWORD;
        if per_request {
            // Per-request credentials are only installed; the device judges them on each request.
            self.accept(credentials.caller, valid);
            return Ok(LoginState::Unverified);
        }
        if !valid {
            return Err(Error::Authentication);
        }
        if self.approval_checks == 0 {
            self.accept(credentials.caller, true);
            return Ok(LoginState::Authenticated);
        }
        *self.pending.lock().unwrap() = Some((credentials.caller, self.approval_checks));
        Ok(LoginState::ApprovalPending {
            expires_in: Some(60),
        })
    }
    async fn complete_approval(&self) -> Result<LoginState> {
        let mut pending = self.pending.lock().unwrap();
        let Some((_, remaining)) = pending.as_mut() else {
            return Err(Error::InvalidResponse("no approval is pending".into()));
        };
        *remaining -= 1;
        if *remaining > 0 {
            return Ok(LoginState::ApprovalPending { expires_in: None });
        }
        if let Some((caller, _)) = pending.take() {
            self.accept(caller, true);
        }
        Ok(LoginState::Authenticated)
    }
    async fn check_auth(&self) -> Result<()> {
        self.authenticated().map(|_| ())
    }
    fn caller(&self) -> Option<CallerIdentity> {
        self.caller.lock().unwrap().clone()
    }
}

#[async_trait]
impl Connection for FakeConnection {
    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
    async fn identity(&self) -> Result<DeviceIdentity> {
        // Nothing in process authenticates the "server", so the address is not verified.
        Ok(DeviceIdentity::observe(
            &self.endpoint,
            false,
            &self.status().await?.info,
        ))
    }
}

/// Opens synthetic connections; `approval_checks` is passed to each one.
#[derive(Default)]
struct FakeConnector {
    approval_checks: u32,
}

#[async_trait]
impl Connector for FakeConnector {
    async fn open(&self, endpoint: Endpoint) -> Result<Arc<dyn Connection>> {
        Ok(Arc::new(FakeConnection {
            device: FakeDevice::default(),
            endpoint,
            caller: Mutex::new(None),
            accepted: Mutex::new(false),
            pending: Mutex::new(None),
            approval_checks: self.approval_checks,
        }))
    }
}

/// Why the consumer could not choose a switch port, beside the device's own errors.
#[derive(Debug)]
enum ConsumeError {
    Device(Error),
    /// The device reports that it has no switch.
    NoSwitch,
    /// The switch status, its port list or its active port could not be observed, so no port can be chosen safely.
    SwitchUnknown,
    /// The device reports a switch but no port list.
    NoPortList,
    NoOtherPort,
}

impl From<Error> for ConsumeError {
    fn from(error: Error) -> Self {
        Self::Device(error)
    }
}

impl std::fmt::Display for ConsumeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Device(error) => error.fmt(formatter),
            Self::NoSwitch => formatter.write_str("the device has no switch"),
            Self::SwitchUnknown => formatter.write_str("the switch status is unknown"),
            Self::NoPortList => formatter.write_str("the switch reports no port list"),
            Self::NoOtherPort => formatter.write_str("the switch has no other port"),
        }
    }
}

impl std::error::Error for ConsumeError {}

// A consumer depends on the public contract, not on a particular device implementation.
async fn consume(device: &dyn Device) -> std::result::Result<ControlOutcome, ConsumeError> {
    let capabilities = device.capabilities().await?;
    // Status is read whatever its capability says: each subsystem reports its own support, and the switch subsystem reports which ports exist, so the consumer picks one the device has rather than guessing.
    let status = device.status().await?;
    let switch = match &status.switch {
        Reported::Supported(switch) => switch,
        Reported::Unsupported => return Err(ConsumeError::NoSwitch),
        Reported::Unknown => return Err(ConsumeError::SwitchUnknown),
    };
    // An unobserved active port is not "no port selected": choosing a target then could pick the active port.
    let active = match switch.active_port {
        Reported::Supported(active) => active,
        Reported::Unsupported | Reported::Unknown => return Err(ConsumeError::SwitchUnknown),
    };
    let ports = match &switch.ports {
        Reported::Supported(ports) => ports,
        Reported::Unsupported => return Err(ConsumeError::NoPortList),
        Reported::Unknown => return Err(ConsumeError::SwitchUnknown),
    };
    let target = ports
        .iter()
        .find(|port| Some(port.port) != active)
        .ok_or(ConsumeError::NoOtherPort)?
        .port;
    println!("Switch ports: {:?}, active {active:?}", switch.ports);
    if capabilities.snapshot == Capability::Supported {
        println!("Snapshot: {} bytes", device.snapshot().await?.jpeg.len());
    }
    if capabilities.live_video == Capability::Supported {
        let mut video = device.video().await?;
        if let Some(frame) = video.next_frame().await? {
            println!("Video frame: {} bytes", frame.jpeg.len());
        }
    }
    if capabilities.keyboard == Capability::Supported {
        let result = device.key("KeyA", true).await;
        // Release even when the press has an ambiguous outcome.
        let release = device.release_inputs().await;
        result?;
        release?;
    }
    Ok(device
        .control(Control::Switch { port: target }, Confirmation::new())
        .await?)
}

/// The endpoint the example connects to; it carries no secret, so it could come from a configuration file.
fn endpoint(auth: AuthScheme) -> Endpoint {
    Endpoint {
        label: Some("bench-kvm".into()),
        address: "https://kvm.invalid/".parse().unwrap(),
        profile: DeviceProfile::GlKvm,
        auth,
        trust: ServerTrust::Verified {
            additional_ca_pem: None,
        },
        timeout: Duration::from_secs(5),
    }
}

/// Why the consumer could not connect, beside the device's own errors.
#[derive(Debug)]
enum ConnectError {
    Device(Error),
    /// The device holds the login for approval without saying when the request expires, so there is no bound on waiting.
    NoApprovalExpiry,
    /// The approval request expired before it was granted.
    ApprovalExpired,
}

impl From<Error> for ConnectError {
    fn from(error: Error) -> Self {
        Self::Device(error)
    }
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Device(error) => error.fmt(formatter),
            Self::NoApprovalExpiry => formatter.write_str("device approval has no expiry"),
            Self::ApprovalExpired => formatter.write_str("device approval expired"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Connects through any connector, waits out a pending approval by checking every `poll` until the device's expiry, then reads the device's identity. A consumer written against the contract alone, as this one is, works with any implementation.
async fn connect(
    connector: &dyn Connector,
    endpoint: Endpoint,
    credentials: Credentials,
    poll: Duration,
) -> std::result::Result<(Arc<dyn Connection>, DeviceIdentity), ConnectError> {
    let connection = match connector.connect(endpoint, credentials).await? {
        Connected::Authenticated(connection) => connection,
        Connected::ApprovalPending {
            connection,
            expires_in,
        } => {
            println!("Approve the login on the device");
            let expiry = expires_in.ok_or(ConnectError::NoApprovalExpiry)?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(expiry);
            loop {
                tokio::time::sleep_until((tokio::time::Instant::now() + poll).min(deadline)).await;
                if tokio::time::Instant::now() >= deadline {
                    return Err(ConnectError::ApprovalExpired);
                }
                if connection.complete_approval().await? == LoginState::Authenticated {
                    break connection;
                }
            }
        }
    };
    let identity = connection.identity().await?;
    Ok((connection, identity))
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let connector = FakeConnector { approval_checks: 2 };
    let (connection, identity) = connect(
        &connector,
        endpoint(AuthScheme::Session),
        Credentials::new(USERNAME, PASSWORD),
        Duration::from_secs(1),
    )
    .await?;
    println!(
        "Connected to {:?} as {:?}",
        identity.id(),
        connection.caller()
    );
    let outcome = consume(connection.as_ref()).await?;
    println!("Switch outcome: {outcome:?}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvm_core::EndpointName;

    /// The interval between approval checks in tests: short, so the synthetic approval is not waited for.
    const TEST_POLL: Duration = Duration::from_millis(1);

    #[tokio::test]
    async fn consumer_uses_the_device_contract_in_process() {
        let device = FakeDevice::default();
        assert_eq!(consume(&device).await.unwrap(), ControlOutcome::Unverified);
        assert_eq!(
            *device.events.lock().unwrap(),
            ["key KeyA true", "release inputs", "switch 1"]
        );
        let mut video = device.video().await.unwrap();
        assert_eq!(
            video.next_frame().await.unwrap().unwrap().jpeg,
            [0xff, 0xd8, 0xff, 0xd9]
        );
        assert!(video.next_frame().await.unwrap().is_none());
        assert!(matches!(
            device.mouse_move(1, 1).await,
            Err(Error::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn consumer_connects_authenticates_and_identifies_through_the_contract() {
        for approval_checks in [0, 3] {
            let connector = FakeConnector { approval_checks };
            let (connection, identity) = connect(
                &connector,
                endpoint(AuthScheme::Session),
                Credentials::new(USERNAME, PASSWORD),
                TEST_POLL,
            )
            .await
            .unwrap();
            assert_eq!(
                connection.caller(),
                Some(CallerIdentity {
                    username: USERNAME.into()
                })
            );
            assert_eq!(identity.label.as_deref(), Some("bench-kvm"));
            assert_eq!(identity.profile, DeviceProfile::GlKvm);
            assert!(!identity.address_verified);
            assert_eq!(identity.serial, Reported::Supported(SERIAL.into()));
            assert_eq!(identity.host, Reported::Unknown);
            assert_eq!(
                identity.id().endpoint,
                EndpointName::Label("bench-kvm".into())
            );
            assert_eq!(
                consume(connection.as_ref()).await.unwrap(),
                ControlOutcome::Unverified
            );
        }
    }

    #[tokio::test]
    async fn refused_and_unconfirmed_credentials_reach_no_device() {
        let connector = FakeConnector::default();
        assert!(matches!(
            connect(
                &connector,
                endpoint(AuthScheme::Session),
                Credentials::new(USERNAME, "wrong"),
                TEST_POLL,
            )
            .await,
            Err(ConnectError::Device(Error::Authentication))
        ));
        // A login that only installs per-request credentials leaves the check to the caller; the contract's `connect` makes it.
        let connection = connector.open(endpoint(AuthScheme::Header)).await.unwrap();
        assert!(connection.status().await.is_err());
        assert_eq!(
            connection
                .login(Credentials::new(USERNAME, PASSWORD))
                .await
                .unwrap(),
            LoginState::Unverified
        );
        connection.check_auth().await.unwrap();
        connection
            .login(Credentials::new(USERNAME, "wrong"))
            .await
            .unwrap();
        assert!(matches!(
            connection.check_auth().await,
            Err(Error::Authentication)
        ));
        // A one-time code under a per-request scheme is refused before the session changes.
        assert!(matches!(
            connection
                .login(Credentials::new(USERNAME, PASSWORD).with_totp("012345"))
                .await,
            Err(Error::InvalidResponse(_))
        ));
        assert_eq!(
            connection.caller(),
            Some(CallerIdentity {
                username: USERNAME.into()
            })
        );
        assert!(matches!(
            connector
                .connect(
                    endpoint(AuthScheme::Basic),
                    Credentials::new(USERNAME, "wrong")
                )
                .await,
            Err(Error::Authentication)
        ));
    }

    #[tokio::test]
    async fn an_unobserved_switch_sends_no_control() {
        for (device, expected) in [
            (
                FakeDevice {
                    switch_unknown: true,
                    ..FakeDevice::default()
                },
                "the switch status is unknown",
            ),
            (
                FakeDevice {
                    active_unknown: true,
                    ..FakeDevice::default()
                },
                "the switch status is unknown",
            ),
            (
                FakeDevice {
                    no_port_list: true,
                    ..FakeDevice::default()
                },
                "the switch reports no port list",
            ),
        ] {
            let error = consume(&device).await.unwrap_err();
            assert_eq!(error.to_string(), expected);
            // No switch control was sent without a known target.
            assert!(
                !device
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| event.starts_with("switch"))
            );
        }
    }
}
