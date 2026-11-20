use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};
use tungstenite::Message;

use crate::{ApiError, Call, Config, Handler, Server};

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// A Lightair with one scene, answering a few methods and noting every call.
fn server(password: Option<&str>) -> (Server, Calls) {
    let calls = Arc::new(Mutex::new(vec![]));
    let seen = calls.clone();
    let handler: Handler = Arc::new(move |c: Call| {
        seen.lock().push((c.method.clone(), c.params.clone()));
        match c.method.as_str() {
            "state.get" => Ok(json!({
                "program": {"id": "s1", "name": "Main"},
                "scenes": [{"id": "s1", "name": "Main", "objects": [
                    {"id": "o1", "name": "Cam", "source": "src1", "parent": null, "visible": true, "locked": false, "x": 0, "y": 0, "width": 1920, "height": 1080}
                ]}, {"id": "s2", "name": "BRB", "objects": []}],
                "sources": [{"id": "src1", "name": "Cam", "kind": "lightair_camera"}],
                "tracks": [{"id": "t1", "name": "Cam", "source": "src1", "volume_db": -6.0, "muted": false}],
                "outputs": {"recording": {"active": false, "paused": false}},
            })),
            "app.version" => Ok(json!({"version": "0.1.0", "workspace": "Test"})),
            "screenshot" => Ok(json!({"mime": "image/png", "data": "iVBORw0KGgo=", "width": 2, "height": 2})),
            "record.start" | "scene.switch" | "object.update" | "audio.set" => Ok(json!({})),
            _ => Err(ApiError::new(crate::ErrorKind::Unknown, "no such method")),
        }
    });
    let config = Config { port: 0, lan: false, password: password.map(str::to_owned) };
    (Server::start(config, handler).unwrap(), calls)
}

fn post(port: u16, path: &str, body: &str, auth: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let auth = auth.map(|a| format!("Authorization: Bearer {a}\r\n")).unwrap_or_default();
    write!(s, "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out[9..12].parse().unwrap();
    (status, out.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
}

#[test]
fn mcp_lists_and_calls_tools() {
    let (srv, calls) = server(Some("pw"));
    let port = srv.addr().port();
    let (status, _) = post(port, "/mcp", r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#, None);
    assert_eq!(status, 401, "the password is asked");
    let (status, body) = post(
        port,
        "/mcp",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        Some("pw"),
    );
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
    let (status, _) = post(port, "/mcp", r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, Some("pw"));
    assert_eq!(status, 202);
    let (_, body) = post(port, "/mcp", r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, Some("pw"));
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["result"]["tools"].as_array().unwrap().len(), crate::mcp::tool_names().len());
    assert!(crate::mcp::schemas_parse());
    let (_, body) = post(
        port,
        "/mcp",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"recording","arguments":{"action":"start"}}}"#,
        Some("pw"),
    );
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["result"]["isError"].is_null(), "{v}");
    assert_eq!(calls.lock().last().unwrap().0, "record.start");
    let (_, body) = post(
        port,
        "/mcp",
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"screenshot","arguments":{}}}"#,
        Some("pw"),
    );
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["result"]["content"][0]["type"], "image");
    let (_, body) = post(
        port,
        "/mcp",
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"get_stats","arguments":{}}}"#,
        Some("pw"),
    );
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["result"]["isError"], true, "the method's error is the tool's");
}

#[test]
fn other_hosts_are_refused() {
    let (srv, _) = server(None);
    let port = srv.addr().port();
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(s, "GET / HTTP/1.1\r\nHost: evil.example:4455\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");
}

fn read_json(ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>) -> Value {
    loop {
        if let Message::Text(t) = ws.read().unwrap() {
            return serde_json::from_str(t.as_str()).unwrap();
        }
    }
}

type Ws = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>;

fn connect(url: &str) -> Ws {
    let mut req = tungstenite::client::IntoClientRequest::into_client_request(url).unwrap();
    if url.ends_with('/') {
        req.headers_mut().insert("Sec-WebSocket-Protocol", "obswebsocket.json".parse().unwrap());
    }
    let stream = TcpStream::connect(req.uri().authority().unwrap().as_str()).unwrap();
    let (ws, _) = tungstenite::client(req, tungstenite::stream::MaybeTlsStream::Plain(stream)).unwrap();
    ws
}

#[test]
fn obs_websocket_identifies_and_answers() {
    let (srv, calls) = server(Some("secret"));
    let mut ws = connect(&format!("ws://127.0.0.1:{}/", srv.addr().port()));
    let hello = read_json(&mut ws);
    assert_eq!(hello["op"], 0);
    let a = &hello["d"]["authentication"];
    let auth = crate::obs::auth_string("secret", a["salt"].as_str().unwrap(), a["challenge"].as_str().unwrap());
    ws.send(Message::text(json!({"op": 1, "d": {"rpcVersion": 1, "authentication": auth}}).to_string())).unwrap();
    assert_eq!(read_json(&mut ws)["op"], 2);

    ws.send(Message::text(json!({"op": 6, "d": {"requestType": "GetSceneList", "requestId": "a"}}).to_string()))
        .unwrap();
    let r = read_json(&mut ws);
    assert_eq!(r["d"]["requestStatus"]["code"], 100);
    assert_eq!(r["d"]["responseData"]["currentProgramSceneName"], "Main");
    // bottom first
    assert_eq!(r["d"]["responseData"]["scenes"][0]["sceneName"], "BRB");

    ws.send(Message::text(json!({"op": 6, "d": {"requestType": "SetInputMute", "requestId": "b", "requestData": {"inputName": "Cam", "inputMuted": true}}}).to_string())).unwrap();
    assert_eq!(read_json(&mut ws)["d"]["requestStatus"]["result"], true);
    assert_eq!(calls.lock().last().unwrap(), &("audio.set".to_string(), json!({"track": "t1", "muted": true})));

    let id = crate::obs::item_id("o1");
    ws.send(Message::text(json!({"op": 8, "d": {"requestId": "c", "requests": [
        {"requestType": "SetSceneItemEnabled", "requestData": {"sceneName": "Main", "sceneItemId": id, "sceneItemEnabled": false}},
        {"requestType": "Nope"},
    ]}}).to_string())).unwrap();
    let b = read_json(&mut ws);
    assert_eq!(b["op"], 9);
    assert_eq!(b["d"]["results"][0]["requestStatus"]["result"], true);
    assert_eq!(b["d"]["results"][1]["requestStatus"]["code"], 204);

    srv.emit("scene.program", json!({"id": "s2", "name": "BRB"}));
    let e = read_json(&mut ws);
    assert_eq!(e["d"]["eventType"], "CurrentProgramSceneChanged");
    assert_eq!(e["d"]["eventData"]["sceneName"], "BRB");
}

#[test]
fn obs_websocket_refuses_a_wrong_password() {
    let (srv, _) = server(Some("secret"));
    let mut ws = connect(&format!("ws://127.0.0.1:{}/", srv.addr().port()));
    let _hello = read_json(&mut ws);
    ws.send(Message::text(json!({"op": 1, "d": {"rpcVersion": 1, "authentication": "nope"}}).to_string())).unwrap();
    match ws.read() {
        Ok(Message::Close(Some(f))) => assert_eq!(u16::from(f.code), 4009),
        other => panic!("expected a close, got {other:?}"),
    }
}

#[test]
fn json_rpc_needs_the_password_then_answers() {
    let (srv, _) = server(Some("pw"));
    let mut ws = connect(&format!("ws://127.0.0.1:{}/rpc", srv.addr().port()));
    ws.send(Message::text(r#"{"jsonrpc":"2.0","id":1,"method":"state.get"}"#)).unwrap();
    assert_eq!(read_json(&mut ws)["error"]["code"], -32001);
    ws.send(Message::text(r#"{"jsonrpc":"2.0","id":2,"method":"auth.login","params":{"password":"pw"}}"#)).unwrap();
    assert_eq!(read_json(&mut ws)["result"]["authenticated"], true);
    ws.send(Message::text(
        r#"[{"jsonrpc":"2.0","id":3,"method":"app.version"},{"jsonrpc":"2.0","id":4,"method":"nope"}]"#,
    ))
    .unwrap();
    let b = read_json(&mut ws);
    assert_eq!(b[0]["result"]["workspace"], "Test");
    assert_eq!(b[1]["error"]["code"], -32601);
    srv.emit("record.state", json!({"active": true}));
    let e = read_json(&mut ws);
    assert_eq!(e["method"], "event");
    assert_eq!(e["params"]["name"], "record.state");
}

#[test]
fn stopping_disconnects() {
    let (srv, _) = server(None);
    let port = srv.addr().port();
    let mut ws = connect(&format!("ws://127.0.0.1:{port}/rpc"));
    drop(srv);
    let closed = (0..50).any(|_| matches!(ws.read(), Ok(Message::Close(_)) | Err(_)));
    assert!(closed);
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err() || post(port, "/mcp", "{}", None).0 != 200);
}

#[test]
fn a_stalled_client_drops_meters_and_is_told_to_resync() {
    let handler: Handler = Arc::new(|_c: Call| Ok(serde_json::json!({})));
    let server = Server::start(Config { port: 0, lan: false, password: None }, handler).unwrap();
    // a client that reads nothing
    let joined = server.shared.join("test", "nowhere".into());
    for _ in 0..crate::QUEUE + 50 {
        server.emit("audio.levels", serde_json::json!({}));
    }
    assert_eq!(joined.events.len(), crate::QUEUE, "the queue is bounded");
    assert!(!joined.resync.load(std::sync::atomic::Ordering::Acquire), "meters dropped quietly");
    server.emit("scene.program", serde_json::json!({"id": "s2"}));
    assert!(joined.resync.load(std::sync::atomic::Ordering::Acquire), "a lost scene change means a resync");
}
