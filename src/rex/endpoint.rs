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
        if let Some(path) = std::env::var_os("REX_SOCKET").filter(|p| !p.is_empty()) {
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
            Some((scheme, rest)) => (Some(scheme.to_ascii_lowercase()), rest),
            None => (None, address),
        };
        let rest = rest.split('/').next().unwrap_or(rest);
        if rest.contains('@') {
            bail!("login-server accounts (ACCOUNT@host) aren't supported yet");
        }
        let (host, port) = split_host_port(rest)?;
        if host.is_empty() {
            bail!("no host in {address:?}");
        }
        let url = |tls, port| Self::Url {
            tls,
            host: host.clone(),
            port,
        };
        Ok(match (scheme.as_deref(), port) {
            (Some("https"), port) => vec![url(true, port.unwrap_or(443))],
            (Some("http"), port) => vec![url(false, port.unwrap_or(80))],
            (Some(other), _) => bail!("unsupported scheme {other}://"),
            (None, Some(port)) => vec![url(true, port), url(false, port)],
            (None, None) => vec![url(true, 443), url(true, 7297), url(false, 7297)],
        })
    }

    /// Open a WebSocket to `path` on the server, asking for `subprotocol`. Gives up after
    /// `CONNECT_TIMEOUT`, so a host that doesn't answer can't hold anything up for long.
    pub async fn connect(&self, path: &str, subprotocol: &str) -> Result<Ws> {
        tokio::time::timeout(CONNECT_TIMEOUT, self.connect_now(path, subprotocol))
            .await
            .with_context(|| {
                format!("{self} didn't answer within {}s", CONNECT_TIMEOUT.as_secs())
            })?
    }

    async fn connect_now(&self, path: &str, subprotocol: &str) -> Result<Ws> {
        let (url, stream): (String, Box<dyn Io>) = match self {
            Self::Unix(socket) => {
                let stream = UnixStream::connect(socket)
                    .await
                    .with_context(|| format!("connecting to {}", socket.display()))?;
                // Over a Unix socket any host name works.
                (format!("ws://localhost{path}"), Box::new(stream))
            }
            Self::Url { tls, host, port } => {
                let unreachable = || Unreachable {
                    host: host.clone(),
                    port: *port,
                };
                let stream =
                    tokio::time::timeout(TCP_TIMEOUT, TcpStream::connect((host.as_str(), *port)))
                        .await
                        .map_err(|_| anyhow::Error::new(unreachable()).context("no answer"))?
                        .map_err(|err| anyhow::Error::new(unreachable()).context(err))?;
                stream.set_nodelay(true)?;
                let scheme = if *tls { "wss" } else { "ws" };
                (
                    format!("{scheme}://{}:{port}{path}", url_host(host)),
                    Box::new(stream),
                )
            }
        };
        let mut request = url.into_client_request()?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(subprotocol)?,
        );
        let (ws, _) = tokio_tungstenite::client_async_tls_with_config(request, stream, None, None)
            .await
            .map_err(describe_ws_error)?;
        Ok(ws)
    }
}

/// How long to wait for a server to accept a connection and finish the WebSocket handshake.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// How long to wait for a TCP connection alone. A host that doesn't answer in this time
/// usually doesn't answer at all.
const TCP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// A host and port that refused or didn't answer a TCP connection, so other addresses for
/// the same host and port needn't be tried.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Unreachable {
    pub host: String,
    pub port: u16,
}

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{} is unreachable", url_host(&self.host), self.port)
    }
}

impl std::error::Error for Unreachable {}

/// Split `host`, `host:port`, `[v6]`, `[v6]:port`, or a bare IPv6 address such as `::1`.
fn split_host_port(rest: &str) -> Result<(String, Option<u16>)> {
    let port = |p: &str| {
        p.parse::<u16>()
            .with_context(|| format!("invalid port {p:?}"))
    };
    if let Some(bracketed) = rest.strip_prefix('[') {
        let (host, after) = bracketed
            .split_once(']')
            .context("missing ] in IPv6 address")?;
        return match after.strip_prefix(':') {
            Some(p) => Ok((host.to_string(), Some(port(p)?))),
            None if after.is_empty() => Ok((host.to_string(), None)),
            None => bail!("unexpected {after:?} after the IPv6 address"),
        };
    }
    // More than one colon and no brackets: an IPv6 address without a port.
    if rest.matches(':').count() > 1 {
        return Ok((rest.to_string(), None));
    }
    match rest.split_once(':') {
        Some((host, p)) => Ok((host.to_string(), Some(port(p)?))),
        None => Ok((rest.to_string(), None)),
    }
}

/// A host as it goes in a URL: IPv6 addresses in brackets.
fn url_host(host: &str) -> std::borrow::Cow<'_, str> {
    if host.contains(':') {
        format!("[{host}]").into()
    } else {
        host.into()
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "unix://{}", path.display()),
            Self::Url { tls, host, port } => {
                let scheme = if *tls { "https" } else { "http" };
                write!(f, "{scheme}://{}:{port}", url_host(host))
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

    #[test]
    fn parses_ipv6_and_any_case_scheme() {
        assert_eq!(
            Endpoint::candidates("http://[::1]:7297").unwrap(),
            [url(false, "::1", 7297)]
        );
        assert_eq!(
            Endpoint::candidates("[::1]:7297").unwrap(),
            [url(true, "::1", 7297), url(false, "::1", 7297)]
        );
        assert_eq!(
            Endpoint::candidates("[fe80::1]").unwrap(),
            [
                url(true, "fe80::1", 443),
                url(true, "fe80::1", 7297),
                url(false, "fe80::1", 7297)
            ]
        );
        assert_eq!(
            Endpoint::candidates("::1").unwrap()[0],
            url(true, "::1", 443)
        );
        assert_eq!(
            Endpoint::candidates("HTTPS://Build.x.ts.net").unwrap(),
            [url(true, "Build.x.ts.net", 443)]
        );
        assert_eq!(url(false, "::1", 7297).to_string(), "http://[::1]:7297");
        assert!(Endpoint::candidates("[::1").is_err());
        assert!(Endpoint::candidates("host:notaport").is_err());
    }
}
