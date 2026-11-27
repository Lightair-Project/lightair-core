//! HTTPS for publishing and community catalogs: rustls with the system's certificate checks.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

pub struct Http {
    agent: ureq::Agent,
}

/// A response's status and body.
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as text, shortened, for an error message.
    pub fn text(&self) -> String {
        let t = String::from_utf8_lossy(&self.body);
        t.chars().take(400).collect()
    }
}

impl Http {
    pub fn new() -> Http {
        use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::PlatformVerifier)
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .build();
        let agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .timeout_global(Some(Duration::from_secs(600)))
            .http_status_as_error(false)
            .user_agent("lightair-pack")
            .build()
            .into();
        Http { agent }
    }

    fn finish(res: Result<ureq::http::Response<ureq::Body>, ureq::Error>, limit: u64) -> Result<Reply, String> {
        let mut res = res.map_err(|e| e.to_string())?;
        let status = res.status().as_u16();
        let mut body = Vec::new();
        res.body_mut().with_config().limit(limit).reader().read_to_end(&mut body).map_err(|e| e.to_string())?;
        Ok(Reply { status, body })
    }

    pub fn get(&self, url: &str, token: Option<&str>) -> Result<Reply, String> {
        let mut req = self.agent.get(url).header("Accept", "application/vnd.github+json");
        if let Some(t) = token {
            req = req.header("Authorization", &format!("Bearer {t}"));
        }
        Http::finish(req.call(), 64 << 20)
    }

    /// Downloads into `out`, up to `limit` bytes.
    pub fn download(&self, url: &str, out: &std::path::Path, limit: u64) -> Result<(), String> {
        let mut res = self.agent.get(url).call().map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("{url} answered {}", res.status()));
        }
        let mut file = std::fs::File::create(out).map_err(|e| e.to_string())?;
        let mut reader = res.body_mut().with_config().limit(limit).reader();
        std::io::copy(&mut reader, &mut file).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn send_json(&self, method: &str, url: &str, token: Option<&str>, body: &Value) -> Result<Reply, String> {
        let text = body.to_string();
        let res = match method {
            "PATCH" => {
                let mut r = self.agent.patch(url).header("Content-Type", "application/json");
                if let Some(t) = token {
                    r = r.header("Authorization", &format!("Bearer {t}"));
                }
                r.header("Accept", "application/vnd.github+json").send(text.as_bytes())
            }
            _ => {
                let mut r = self.agent.post(url).header("Content-Type", "application/json");
                if let Some(t) = token {
                    r = r.header("Authorization", &format!("Bearer {t}"));
                }
                r.header("Accept", "application/vnd.github+json").send(text.as_bytes())
            }
        };
        Http::finish(res, 16 << 20)
    }

    pub fn upload(&self, url: &str, token: &str, content_type: &str, bytes: &[u8]) -> Result<Reply, String> {
        let res = self
            .agent
            .post(url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", content_type)
            .header("Accept", "application/vnd.github+json")
            .send(bytes);
        Http::finish(res, 16 << 20)
    }
}

/// A GitHub token: `GITHUB_TOKEN` or `GH_TOKEN`, else what the GitHub command line is signed in
/// with (`gh auth token`).
pub fn github_token() -> Result<String, String> {
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(t) = std::env::var(var)
            && !t.trim().is_empty()
        {
            return Ok(t.trim().to_string());
        }
    }
    let out = std::process::Command::new("gh").args(["auth", "token"]).output().map_err(|_| {
        "no GitHub token: set GITHUB_TOKEN, or sign in with the GitHub command line (gh auth login)".to_string()
    })?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && !t.is_empty() {
        Ok(t)
    } else {
        Err("no GitHub token: set GITHUB_TOKEN, or sign in with the GitHub command line (gh auth login)".into())
    }
}
