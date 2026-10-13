pub mod auth;
pub use auth::{ProxyAuth, ProxyAuthStatus};

use crate::{Error, Result};
use std::{fmt, str::FromStr};

/// A validated HTTP, HTTPS, SOCKS4, or SOCKS5 proxy URL.
/// [`std::fmt::Display`] and [`std::fmt::Debug`] never include credentials.
/// Chrome Fetch authentication supports HTTP proxy credentials; SOCKS URLs with
/// credentials are rejected.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Proxy {
    server: String,
    credentials: Option<(String, String)>,
}

/// Authentication timing for browser/context proxy setup.
/// Without configured credentials, neither mode starts a worker or preflight.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProxyAuthMode {
    /// Check readiness at `https://www.example.com/` before setup returns.
    /// This is the default for a configured authentication worker.
    #[default]
    Preflight,
    /// Handle challenges as requests occur; disable interception after verified authentication.
    OnDemand,
}

impl Proxy {
    pub fn server(&self) -> &str {
        &self.server
    }
    pub fn requires_authentication(&self) -> bool {
        self.credentials().is_some()
    }
    pub(crate) fn credentials(&self) -> Option<(&str, &str)> {
        self.credentials
            .as_ref()
            .map(|(user, password)| (user.as_str(), password.as_str()))
    }

    pub(crate) fn matches_origin(&self, origin: &str) -> bool {
        let (Ok(origin), Ok(server)) = (url::Url::parse(origin), url::Url::parse(&self.server))
        else {
            return false;
        };
        origin.username().is_empty()
            && origin.password().is_none()
            && matches!(origin.path(), "" | "/")
            && origin.query().is_none()
            && origin.fragment().is_none()
            && origin.origin() == server.origin()
    }
}

impl FromStr for Proxy {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        let mut url = url::Url::parse(value)
            .map_err(|_| Error::InvalidArgument("invalid proxy URL".into()))?;
        if !matches!(url.scheme(), "http" | "https" | "socks4" | "socks5")
            || url.host_str().is_none()
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::InvalidArgument(
                "proxy URL must be an http(s) or socks4/5 server without path, query or fragment"
                    .into(),
            ));
        }
        let host = percent_encoding::percent_decode_str(url.host_str().unwrap_or_default())
            .decode_utf8()
            .map_err(|_| Error::InvalidArgument("invalid proxy host".into()))?;
        if host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || ",;=%/?#@".contains(c) || c == '\\')
        {
            return Err(Error::InvalidArgument(
                "proxy host contains a proxy-rule delimiter".into(),
            ));
        }
        let credentials = if !url.username().is_empty() || url.password().is_some() {
            if matches!(url.scheme(), "socks4" | "socks5") {
                return Err(Error::InvalidArgument(
                    "Chrome's Fetch authentication does not support SOCKS credentials".into(),
                ));
            }
            let decode = |value: &str| {
                percent_encoding::percent_decode_str(value)
                    .decode_utf8()
                    .map(|value| value.into_owned())
                    .map_err(|_| Error::InvalidArgument("proxy credentials must be UTF-8".into()))
            };
            Some((
                decode(url.username())?,
                decode(url.password().unwrap_or_default())?,
            ))
        } else {
            None
        };
        url.set_username("")
            .map_err(|_| Error::InvalidArgument("invalid proxy authority".into()))?;
        url.set_password(None)
            .map_err(|_| Error::InvalidArgument("invalid proxy authority".into()))?;
        Ok(Self {
            server: url.as_str().trim_end_matches('/').into(),
            credentials,
        })
    }
}

impl fmt::Display for Proxy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.server)
    }
}

impl fmt::Debug for Proxy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Proxy")
            .field("server", &self.server)
            .field("requires_authentication", &self.requires_authentication())
            .finish()
    }
}
