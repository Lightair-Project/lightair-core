//! Lightair's own protocol: JSON-RPC 2.0 over WebSocket at `/rpc`.
//!
//! Requests name Lightair's methods (`state.get`, `scene.switch`, `object.add`, …; see
//! `docs/remote-api.md`); events arrive as notifications `{"method": "event", "params":
//! {"name", "data"}}`. With a password set, a client sends it as `Authorization: Bearer …` when
//! it connects or calls `auth.login {"password"}` first.

use std::io;
use std::net::TcpStream;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::http::Request;
use crate::{ErrorKind, Event, Shared};

pub(crate) const PARSE: i64 = -32700;
pub(crate) const INVALID_REQUEST: i64 = -32600;
pub(crate) const NO_METHOD: i64 = -32601;
pub(crate) const INVALID_PARAMS: i64 = -32602;
pub(crate) const UNAUTHORIZED: i64 = -32001;
pub(crate) const NOT_FOUND: i64 = -32004;
pub(crate) const STATE: i64 = -32005;
pub(crate) const FAILED: i64 = -32000;

pub(crate) fn code(kind: ErrorKind) -> i64 {
    match kind {
        ErrorKind::Unknown => NO_METHOD,
        ErrorKind::Invalid => INVALID_PARAMS,
        ErrorKind::NotFound => NOT_FOUND,
        ErrorKind::State => STATE,
        ErrorKind::Failed => FAILED,
    }
}

pub(crate) fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

pub(crate) fn bearer(req: &Request) -> Option<&str> {
    req.header("authorization").and_then(|a| a.strip_prefix("Bearer ").or_else(|| a.strip_prefix("bearer ")))
}

pub(crate) fn run(stream: TcpStream, req: &Request, shared: &Arc<Shared>, peer: String) -> io::Result<()> {
    let ws = crate::ws::accept(stream, req, None)?;
    let joined = shared.join("JSON-RPC", peer);
    let session = Session { shared: shared.clone(), authed: shared.password_ok(bearer(req)), events: None };
    crate::ws::run(ws, joined, shared, session)
}

struct Session {
    shared: Arc<Shared>,
    authed: bool,
    /// Only these events (`None`: all).
    events: Option<Vec<String>>,
}

impl Session {
    fn one(&mut self, msg: Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let reply_id = id.clone().unwrap_or(Value::Null);
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            return Some(error(&reply_id, INVALID_REQUEST, "a request needs a method"));
        };
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let result = match method {
            "auth.login" => {
                if self.shared.password_ok(params.get("password").and_then(Value::as_str)) {
                    self.authed = true;
                    Ok(json!({"authenticated": true}))
                } else {
                    Err((UNAUTHORIZED, "wrong password".to_string()))
                }
            }
            _ if !self.authed => Err((UNAUTHORIZED, "send the password first (auth.login)".to_string())),
            "events.subscribe" => {
                self.events = params
                    .get("events")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|e| e.as_str().map(str::to_owned)).collect());
                Ok(json!({"events": self.events}))
            }
            _ => self.shared.call("JSON-RPC", method, params).map_err(|e| (code(e.kind), e.message)),
        };
        // a notification gets no answer
        let id = id?;
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((c, m)) => error(&id, c, &m),
        })
    }
}

impl crate::ws::Protocol for Session {
    fn message(&mut self, text: &str) -> Result<Vec<String>, (u16, String)> {
        let reply = match serde_json::from_str::<Value>(text) {
            Err(e) => Some(error(&Value::Null, PARSE, &e.to_string())),
            Ok(Value::Array(batch)) if batch.is_empty() => Some(error(&Value::Null, INVALID_REQUEST, "empty batch")),
            Ok(Value::Array(batch)) => {
                let out: Vec<Value> = batch.into_iter().filter_map(|m| self.one(m)).collect();
                (!out.is_empty()).then_some(Value::Array(out))
            }
            Ok(m) => self.one(m),
        };
        Ok(reply.map(|r| r.to_string()).into_iter().collect())
    }

    fn event(&mut self, event: &Event) -> Option<String> {
        if !self.authed || self.events.as_ref().is_some_and(|e| !e.contains(&event.name)) {
            return None;
        }
        Some(json!({"jsonrpc": "2.0", "method": "event", "params": {"name": event.name, "data": event.data}}).to_string())
    }
}
