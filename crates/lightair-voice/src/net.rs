//! HTTPS and secure WebSockets for the vendors: rustls with ring, certificates checked by the
//! system's own verifier (Mozilla's roots on Android).

use std::net::TcpStream;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

pub type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

fn tls_config() -> ureq::tls::TlsConfig {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .root_certs(if cfg!(target_os = "android") { RootCerts::WebPki } else { RootCerts::PlatformVerifier })
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build()
}

/// An HTTP client that returns every status as a response (the caller reads the error body).
pub fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .tls_config(tls_config())
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

fn rustls_client() -> Result<Arc<rustls::ClientConfig>, String> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            #[cfg(target_os = "android")]
            {
                let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
                let config = rustls::ClientConfig::builder_with_provider(provider)
                    .with_safe_default_protocol_versions()
                    .map_err(|e| e.to_string())?
                    .with_root_certificates(roots)
                    .with_no_client_auth();
                return Ok(Arc::new(config));
            }
            #[allow(unreachable_code)]
            let verifier = rustls_platform_verifier::Verifier::new(provider.clone()).map_err(|e| e.to_string())?;
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth();
            Ok(Arc::new(config))
        })
        .clone()
}

/// Opens a secure WebSocket whose reads give up after `tick`, so its thread can also send.
pub fn connect(url: &str, headers: &[(&'static str, &str)], tick: Duration) -> Result<Socket, String> {
    let mut request = tungstenite::client::IntoClientRequest::into_client_request(url).map_err(|e| e.to_string())?;
    for (k, v) in headers {
        request.headers_mut().insert(*k, v.parse().map_err(|_| format!("bad header {k}"))?);
    }
    let uri = request.uri();
    let host = uri.host().ok_or("no host")?.trim_start_matches('[').trim_end_matches(']').to_string();
    let port = uri.port_u16().unwrap_or(443);
    let stream = TcpStream::connect((host.as_str(), port)).map_err(|e| format!("{host}: {e}"))?;
    stream.set_nodelay(true).ok();
    let connector = tungstenite::Connector::Rustls(rustls_client()?);
    let (socket, _) =
        tungstenite::client_tls_with_config(request, stream, None, Some(connector)).map_err(|e| match e {
            tungstenite::HandshakeError::Failure(tungstenite::Error::Http(r)) => {
                let body = r.body().as_ref().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
                let body: String = body.chars().take(200).collect();
                format!(
                    "{host} answered {}{}",
                    r.status().as_u16(),
                    if body.is_empty() { String::new() } else { format!(": {body}") }
                )
            }
            e => format!("{host}: {e}"),
        })?;
    let tcp = match socket.get_ref() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::Rustls(s) => s.get_ref(),
        _ => return Err("unexpected stream".into()),
    };
    tcp.set_read_timeout(Some(tick)).map_err(|e| e.to_string())?;
    Ok(socket)
}

/// The next message; `None` when the read timed out.
pub fn read(socket: &mut Socket) -> Result<Option<Message>, String> {
    match socket.read() {
        Ok(m) => Ok(Some(m)),
        Err(tungstenite::Error::Io(e))
            if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.to_string()),
    }
}
