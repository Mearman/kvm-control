//! The connector's provided `connect`, against a connection that records what it was asked.

use std::{
    future::Future,
    pin::pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::Duration,
};

use async_trait::async_trait;
use kvm_core::{
    AuthScheme, Authenticate, CallerIdentity, Capabilities, Confirmation, Connected, Connection,
    Connector, Control, ControlOutcome, Credentials, Device, DeviceIdentity, DeviceProfile,
    DeviceStatus, Endpoint, Error, LoginState, Result, ServerTrust, Snapshot, VideoStream,
};

/// Runs a future that never waits, which every call on the recording connection is.
fn ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the recording connection never waits"),
    }
}

/// A connection whose login returns `login` and whose authentication check succeeds unless `refuse_check` is set.
struct Recording {
    endpoint: Endpoint,
    login: LoginState,
    refuse_check: bool,
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl Device for Recording {
    async fn video(&self) -> Result<Box<dyn VideoStream>> {
        Err(Error::Unsupported("video".into()))
    }
    async fn status(&self) -> Result<DeviceStatus> {
        Err(Error::Unsupported("status".into()))
    }
    async fn capabilities(&self) -> Result<Capabilities> {
        Err(Error::Unsupported("capabilities".into()))
    }
    async fn snapshot(&self) -> Result<Snapshot> {
        Err(Error::Unsupported("snapshot".into()))
    }
    async fn key(&self, _: &str, _: bool) -> Result<()> {
        Err(Error::Unsupported("key".into()))
    }
    async fn mouse_button(&self, _: &str, _: bool) -> Result<()> {
        Err(Error::Unsupported("mouse".into()))
    }
    async fn mouse_move(&self, _: i16, _: i16) -> Result<()> {
        Err(Error::Unsupported("mouse".into()))
    }
    async fn release_inputs(&self) -> Result<()> {
        Ok(())
    }
    async fn control(&self, _: Control, _: Confirmation) -> Result<ControlOutcome> {
        Err(Error::Unsupported("control".into()))
    }
}

#[async_trait]
impl Authenticate for Recording {
    async fn login(&self, credentials: Credentials) -> Result<LoginState> {
        self.calls.lock().unwrap().push(format!(
            "login {} {}",
            credentials.caller.username,
            credentials.password.expose()
        ));
        Ok(self.login)
    }
    async fn complete_approval(&self) -> Result<LoginState> {
        Ok(LoginState::Authenticated)
    }
    async fn check_auth(&self) -> Result<()> {
        self.calls.lock().unwrap().push("check".into());
        if self.refuse_check {
            Err(Error::Authentication)
        } else {
            Ok(())
        }
    }
    fn caller(&self) -> Option<CallerIdentity> {
        None
    }
}

#[async_trait]
impl Connection for Recording {
    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
    async fn identity(&self) -> Result<DeviceIdentity> {
        Err(Error::Unsupported("identity".into()))
    }
}

struct Connect {
    login: LoginState,
    refuse_check: bool,
    opened: Mutex<Option<Arc<Recording>>>,
}

#[async_trait]
impl Connector for Connect {
    async fn open(&self, endpoint: Endpoint) -> Result<Arc<dyn Connection>> {
        let connection = Arc::new(Recording {
            endpoint,
            login: self.login,
            refuse_check: self.refuse_check,
            calls: Mutex::default(),
        });
        *self.opened.lock().unwrap() = Some(connection.clone());
        Ok(connection)
    }
}

fn endpoint(auth: AuthScheme) -> Endpoint {
    Endpoint {
        label: Some("bench".into()),
        address: url::Url::parse("https://device.invalid/").unwrap(),
        profile: DeviceProfile::PiKvm,
        auth,
        trust: ServerTrust::Verified {
            additional_ca_pem: None,
        },
        timeout: Duration::from_secs(5),
    }
}

fn connect(login: LoginState, refuse_check: bool) -> (Result<Connected>, Vec<String>) {
    let connector = Connect {
        login,
        refuse_check,
        opened: Mutex::default(),
    };
    let result = ready(connector.connect(
        endpoint(AuthScheme::Header),
        Credentials::new("operator", "synthetic-password"),
    ));
    let calls = connector
        .opened
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .calls
        .lock()
        .unwrap()
        .clone();
    (result, calls)
}

// Breaks if `connect` hands back per-request credentials nobody has checked, or checks credentials the device already confirmed.
#[test]
fn connect_confirms_only_unverified_credentials() {
    let (result, calls) = connect(LoginState::Unverified, false);
    assert!(matches!(result, Ok(Connected::Authenticated(_))));
    assert_eq!(calls, ["login operator synthetic-password", "check"]);

    let (result, calls) = connect(LoginState::Unverified, true);
    assert!(matches!(result, Err(Error::Authentication)));
    assert_eq!(calls, ["login operator synthetic-password", "check"]);

    let (result, calls) = connect(LoginState::Authenticated, true);
    assert!(matches!(result, Ok(Connected::Authenticated(_))));
    assert_eq!(calls, ["login operator synthetic-password"]);
}

// Breaks if a pending approval is reported as authenticated or loses its expiry.
#[test]
fn connect_passes_a_pending_approval_through() {
    let (result, calls) = connect(
        LoginState::ApprovalPending {
            expires_in: Some(60),
        },
        false,
    );
    let Ok(Connected::ApprovalPending {
        connection,
        expires_in,
    }) = result
    else {
        panic!("approval was not reported as pending");
    };
    assert_eq!(expires_in, Some(60));
    assert_eq!(connection.endpoint().label.as_deref(), Some("bench"));
    assert_eq!(calls, ["login operator synthetic-password"]);
    // The connection is usable as a plain device.
    let device: Arc<dyn Device> = connection;
    assert!(ready(device.release_inputs()).is_ok());
}
