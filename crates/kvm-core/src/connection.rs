//! Connection and authentication contracts.
//!
//! A [`Connector`] turns a serialisable [`Endpoint`] into a [`Connection`]: a [`Device`] that can also authenticate ([`Authenticate`]) and identify itself ([`Connection::identity`]). Credentials travel separately from the endpoint as [`Credentials`], whose secrets are zeroised when dropped, redacted from `Debug` output and never serialised.
//!
//! Nothing here fixes a transport or a product: an implementation documents what its endpoint address means, which [`AuthScheme`] values it supports and whether opening a connection contacts the device. An adapter that needs no login implements [`Device`] alone; these contracts are for implementations that connect and authenticate.

use std::{fmt, sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use url::Url;
use zeroize::Zeroizing;

use crate::{Device, DeviceIdentity, DeviceProfile, Result};

/// A secret value, such as a password or a one-time code.
///
/// The value is zeroed when the secret is dropped, `Debug` prints only `Secret(<redacted>)`, and the type implements neither `Clone` nor `Serialize` nor `Deserialize`, so a secret is not copied or written out by accident. Copies the caller made before constructing it, and copies an implementation makes while encoding a request, are not covered.
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Wraps a value, taking ownership of its buffer.
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// The secret value, for an implementation that has to send it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self::new(value.to_owned())
    }
}

impl From<Zeroizing<String>> for Secret {
    fn from(value: Zeroizing<String>) -> Self {
        Self(value)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

/// Who a caller presents itself as to a device: the account name in its credentials.
///
/// This is the caller's claim. The device accepting the credentials that carry it is what authenticates it, and an implementation reports that acceptance through [`LoginState`] and [`Authenticate::check_auth`]; it does not learn a device-side account identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CallerIdentity {
    pub username: String,
}

/// The credentials a caller authenticates with.
///
/// `Debug` shows the caller identity and whether a one-time code is present, never a secret value. Credentials are consumed by [`Authenticate::login`], so the implementation drops, and so zeroes, its secrets when it no longer needs them.
pub struct Credentials {
    pub caller: CallerIdentity,
    pub password: Secret,
    /// A current one-time code (TOTP) for a device that requires one; a fresh code is needed for each login.
    pub totp: Option<Secret>,
}

impl Credentials {
    /// Credentials with no one-time code.
    pub fn new(username: impl Into<String>, password: impl Into<Secret>) -> Self {
        Self {
            caller: CallerIdentity {
                username: username.into(),
            },
            password: password.into(),
            totp: None,
        }
    }

    /// Adds a current one-time code.
    pub fn with_totp(self, code: impl Into<Secret>) -> Self {
        Self {
            totp: Some(code.into()),
            ..self
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("caller", &self.caller)
            .field("password", &self.password)
            .field("totp", &self.totp)
            .finish()
    }
}

/// How credentials reach the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AuthScheme {
    /// A login exchanges the credentials once for a session, which the device may hold for approval and which may need a one-time code.
    Session,
    /// The credentials are sent with every request in authentication headers the device defines. A one-time code cannot be used, since each request would reuse it.
    Header,
    /// The credentials are sent with every request as HTTP Basic authentication. A one-time code cannot be used.
    Basic,
}

/// How the device's identity is checked when a connection is established, for transports that authenticate the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerTrust {
    /// The server must prove it holds the endpoint address, against the implementation's default trust roots and, when given, this additional PEM certificate authority (the way to trust a device's own CA).
    Verified { additional_ca_pem: Option<String> },
    /// The server is not checked at all, host name included: anyone who can intercept the connection can read the session and the credentials.
    Disabled,
}

/// Where and how to connect to one device. It carries no secret, so it can be stored and exchanged as configuration.
///
/// Its serialised form carries no version field, and like the rest of the API it has no stability guarantee across releases (see the README's API stability note): a stored endpoint is read back reliably only by the release that wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// A caller-chosen stable name for this endpoint. It names the endpoint in [`crate::DeviceId`] when present, so a device keeps its identifier when its address changes; without it the address does.
    pub label: Option<String>,
    /// The device's address. Its meaning, and which schemes it accepts, are the implementation's.
    pub address: Url,
    /// The device profile, chosen by the caller. Implementations never infer it from what the device reports.
    pub profile: DeviceProfile,
    pub auth: AuthScheme,
    pub trust: ServerTrust,
    /// The implementation's request deadline. How it bounds connecting, a whole request and long-lived streams is the implementation's to document. It serialises as an object of whole seconds and nanoseconds (`{"secs": 5, "nanos": 0}`).
    pub timeout: Duration,
}

/// Where an authentication attempt stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoginState {
    /// The device confirmed the credentials for this session.
    Authenticated,
    /// The device holds the login until it is approved out of band, for example on the device itself; call [`Authenticate::complete_approval`] to learn the outcome.
    ApprovalPending {
        /// Seconds until the approval request expires, as the device reported; `None` when it did not report an expiry.
        expires_in: Option<u64>,
    },
    /// The credentials will be sent with each request under a per-request [`AuthScheme`], and nothing has checked them yet; [`Authenticate::check_auth`] does.
    Unverified,
}

/// Authentication of a connection's session.
#[async_trait]
pub trait Authenticate: Send + Sync {
    /// Authenticates with `credentials` under the endpoint's [`AuthScheme`], replacing any previous session.
    ///
    /// Returns [`LoginState::Authenticated`] when the device confirmed the credentials, [`LoginState::ApprovalPending`] when it holds the login for approval, and [`LoginState::Unverified`] for a per-request scheme that has not contacted the device. A refusal is [`crate::Error::Authentication`], and it does not say whether a one-time code is required. Credentials the implementation cannot use (a malformed one-time code, or one under a per-request scheme) are [`crate::Error::InvalidResponse`], returned before contacting the device. An implementation documents whether a failed login leaves the previous session in place.
    async fn login(&self, credentials: Credentials) -> Result<LoginState>;

    /// Asks whether a pending approval has been granted.
    ///
    /// Returns [`LoginState::Authenticated`] once approved and [`LoginState::ApprovalPending`] while still pending. It is [`crate::Error::Unsupported`] where the profile or scheme has no approval flow, and [`crate::Error::InvalidResponse`] when no approval is pending.
    async fn complete_approval(&self) -> Result<LoginState>;

    /// Asks the device whether it accepts the current credentials or session; a refusal is [`crate::Error::Authentication`].
    async fn check_auth(&self) -> Result<()>;

    /// The caller identity whose credentials the current session uses, or `None` while no credentials are in use (before a login, after a failed one, or while an approval is pending). It says who the session presents as; whether the device accepted it is what [`LoginState`] and [`Self::check_auth`] report.
    fn caller(&self) -> Option<CallerIdentity>;
}

/// A device reached through an [`Endpoint`], with its authentication and identity.
#[async_trait]
pub trait Connection: Device + Authenticate {
    /// The endpoint this connection was opened with.
    fn endpoint(&self) -> &Endpoint;

    /// Reads the device's identity. Like [`Device::status`], it sends no input or control request.
    async fn identity(&self) -> Result<DeviceIdentity>;
}

/// A connection opened and authenticated by [`Connector::connect`].
pub enum Connected {
    /// The device confirmed the credentials.
    Authenticated(Arc<dyn Connection>),
    /// The device holds the login for approval; poll [`Authenticate::complete_approval`] until it reports [`LoginState::Authenticated`] or the approval expires.
    ApprovalPending {
        connection: Arc<dyn Connection>,
        /// Seconds until the approval request expires; `None` when the device did not report an expiry.
        expires_in: Option<u64>,
    },
}

impl fmt::Debug for Connected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let connection = self.connection();
        let mut debug = match self {
            Self::Authenticated(_) => formatter.debug_struct("Authenticated"),
            Self::ApprovalPending { .. } => formatter.debug_struct("ApprovalPending"),
        };
        debug
            .field("endpoint", connection.endpoint())
            .field("caller", &connection.caller());
        if let Self::ApprovalPending { expires_in, .. } = self {
            debug.field("expires_in", expires_in);
        }
        debug.finish()
    }
}

impl Connected {
    /// Logs `connection` in with `credentials`. Under a per-request scheme the credentials are then confirmed with [`Authenticate::check_auth`], so a returned connection's credentials have always been accepted or are pending approval.
    ///
    /// A caller that should validate the endpoint before collecting credentials, such as one that prompts for a one-time code, calls [`Connector::open`] first and this afterwards.
    pub async fn authenticate(
        connection: Arc<dyn Connection>,
        credentials: Credentials,
    ) -> Result<Self> {
        match connection.login(credentials).await? {
            LoginState::Authenticated => Ok(Self::Authenticated(connection)),
            LoginState::ApprovalPending { expires_in } => Ok(Self::ApprovalPending {
                connection,
                expires_in,
            }),
            LoginState::Unverified => {
                connection.check_auth().await?;
                Ok(Self::Authenticated(connection))
            }
        }
    }

    /// The connection, whatever its state.
    pub fn connection(&self) -> &Arc<dyn Connection> {
        match self {
            Self::Authenticated(connection) | Self::ApprovalPending { connection, .. } => {
                connection
            }
        }
    }
}

/// Opens connections to devices of one implementation.
#[async_trait]
pub trait Connector: Send + Sync {
    /// Opens an unauthenticated connection to `endpoint`. An implementation documents whether this contacts the device and which endpoints it rejects.
    async fn open(&self, endpoint: Endpoint) -> Result<Arc<dyn Connection>>;

    /// Opens a connection and logs in with `credentials`, as [`Connected::authenticate`] does.
    async fn connect(&self, endpoint: Endpoint, credentials: Credentials) -> Result<Connected> {
        Connected::authenticate(self.open(endpoint).await?, credentials).await
    }
}
