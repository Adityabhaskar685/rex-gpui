//! Where a Rex server listens, and how to open a WebSocket to it.
//!
//! A local server listens on a Unix socket. A remote one listens on HTTP or HTTPS, usually
//! through Tailscale, which identifies the caller; the client sends no credentials itself.

use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::control::Control;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub type Ws = WebSocketStream<MaybeTlsStream<Box<dyn Io>>>;

#[derive(Clone, Debug, PartialEq)]
pub enum Endpoint {
    Unix(PathBuf),
    /// `http://host:port` or `https://host:port`, without a path.
    Url {
        tls: bool,
        host: String,
        port: u16,
    },
}

impl Endpoint {
    /// The local server's Unix socket: `$REX_SOCKET`, else `$XDG_RUNTIME_DIR/rex/server.sock`.
    pub fn local() -> Self {
        if let Some(path) = std::env::var_os("REX_SOCKET") {
            return Self::Unix(path.into());
        }
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { getuid() })));
        Self::Unix(runtime.join("rex/server.sock"))
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Unix(_))
    }

    /// The machine to show in the title bar.
    pub fn host_name(&self) -> String {
        match self {
            Self::Unix(_) => std::fs::read_to_string("/proc/sys/kernel/hostname")
                .map(|h| h.trim().to_string())
                .unwrap_or_else(|_| "localhost".into()),
            Self::Url { host, .. } => host.split('.').next().unwrap_or(host).to_string(),
        }
    }

    /// The addresses to try for an address written the way `rex -S` takes it, in order. A
    /// value with a scheme or port is one address; a bare host name tries HTTPS on 443, then
    /// HTTPS and HTTP on 7297, as the CLI does.
    pub fn candidates(address: &str) -> Result<Vec<Self>> {
        let address = address.trim().trim_end_matches('/');
        if let Some(path) = address.strip_prefix("unix://") {
            return Ok(vec![Self::Unix(path.into())]);
        }
        if address.starts_with('/') {
            return Ok(vec![Self::Unix(address.into())]);
        }
        let (scheme, rest) = match address.split_once("://") {
            Some((scheme, rest)) => (Some(scheme), rest),
            None => (None, address),
        };
        if rest.contains('@') {
            bail!("login-server accounts (ACCOUNT@host) aren't supported yet");
        }
        let rest = rest.split('/').next().unwrap_or(rest);
        let (host, port) = match rest.rsplit_once(':') {
            Some((host, port)) if !host.ends_with(']') || rest.starts_with('[') => (
                host.to_string(),
                Some(port.parse::<u16>().context("invalid port")?),
            ),
            _ => (rest.to_string(), None),
        };
        let url = |tls, port| Self::Url {
            tls,
            host: host.clone(),
            port,
        };
        Ok(match (scheme, port) {
            (Some("https"), port) => vec![url(true, port.unwrap_or(443))],
            (Some("http"), port) => vec![url(false, port.unwrap_or(80))],
            (Some(other), _) => bail!("unsupported scheme {other}://"),
            (None, Some(port)) => vec![url(true, port), url(false, port)],
            (None, None) => vec![url(true, 443), url(true, 7297), url(false, 7297)],
        })
    }

    /// Open a WebSocket to `path` on the server, asking for `subprotocol`.
    pub async fn connect(&self, path: &str, subprotocol: &str) -> Result<Ws> {
        let (url, stream): (String, Box<dyn Io>) = match self {
            Self::Unix(socket) => {
                let stream = UnixStream::connect(socket)
                    .await
                    .with_context(|| format!("connecting to {}", socket.display()))?;
                // Over a Unix socket any host name works.
                (format!("ws://localhost{path}"), Box::new(stream))
            }
            Self::Url { tls, host, port } => {
                let stream = TcpStream::connect((host.as_str(), *port))
                    .await
                    .with_context(|| format!("connecting to {host}:{port}"))?;
                stream.set_nodelay(true)?;
                let scheme = if *tls { "wss" } else { "ws" };
                (format!("{scheme}://{host}:{port}{path}"), Box::new(stream))
            }
        };
        let mut request = url.into_client_request()?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(subprotocol)?,
        );
        let connecting =
            tokio_tungstenite::client_async_tls_with_config(request, stream, None, None);
        let (ws, _) = tokio::time::timeout(std::time::Duration::from_secs(10), connecting)
            .await
            .context("timed out opening the WebSocket")?
            .map_err(describe_ws_error)?;
        Ok(ws)
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "unix://{}", path.display()),
            Self::Url { tls, host, port } => {
                write!(f, "{}://{host}:{port}", if *tls { "https" } else { "http" })
            }
        }
    }
}

/// Turn a refused upgrade, such as Rex's 403 for a caller it doesn't admit, into its message.
fn describe_ws_error(err: tokio_tungstenite::tungstenite::Error) -> anyhow::Error {
    use tokio_tungstenite::tungstenite::Error;
    match err {
        Error::Http(response) => {
            let body = response
                .body()
                .as_ref()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
                .and_then(|v| v["error"].as_str().map(str::to_string));
            anyhow::anyhow!(
                "server answered {}{}",
                response.status(),
                body.map(|b| format!(": {b}")).unwrap_or_default()
            )
        }
        other => other.into(),
    }
}

/// Resolve `-S SERVER`: `local`, a label from the local server's host list (`rex hosts`),
/// else an address. Labels need the local server, so a failure to reach it just skips them.
pub async fn resolve(server: &str) -> Result<Vec<Endpoint>> {
    // `local` names this machine's own server.
    if server == "local" {
        return Ok(vec![Endpoint::local()]);
    }
    let looks_like_address = server.contains("://")
        || server.contains('.')
        || server.contains(':')
        || server.starts_with('/');
    if !looks_like_address
        && let Ok((local, _events)) = Control::connect(&[Endpoint::local()], false).await
        && let Ok(reply) = local.call("hosts.list", json!({})).await
    {
        let host = reply["hosts"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|h| h["label"] == server || h["id"] == server);
        if let Some(host) = host {
            let mut out = Vec::new();
            for e in host["endpoints"].as_array().into_iter().flatten() {
                out.extend(Endpoint::candidates(e.as_str().unwrap_or_default())?);
            }
            return Ok(out);
        }
    }
    Endpoint::candidates(server)
}

unsafe extern "C" {
    fn getuid() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(tls: bool, host: &str, port: u16) -> Endpoint {
        Endpoint::Url {
            tls,
            host: host.into(),
            port,
        }
    }

    #[test]
    fn parses_addresses_like_the_cli() {
        assert_eq!(
            Endpoint::candidates("build.example.ts.net").unwrap(),
            [
                url(true, "build.example.ts.net", 443),
                url(true, "build.example.ts.net", 7297),
                url(false, "build.example.ts.net", 7297)
            ]
        );
        assert_eq!(
            Endpoint::candidates("https://build.x.ts.net/").unwrap(),
            [url(true, "build.x.ts.net", 443)]
        );
        assert_eq!(
            Endpoint::candidates("http://build:7297").unwrap(),
            [url(false, "build", 7297)]
        );
        assert_eq!(
            Endpoint::candidates("10.0.0.2:7297").unwrap(),
            [url(true, "10.0.0.2", 7297), url(false, "10.0.0.2", 7297)]
        );
        assert!(Endpoint::candidates("https://root@build").is_err());
    }
}
