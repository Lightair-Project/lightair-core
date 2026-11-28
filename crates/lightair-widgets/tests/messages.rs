//! The channel between a widget page and Lightair that script plugins' overlays use: what a page
//! sends (`lightair.send`) reaches Lightair with the page's widget, and what Lightair sends to one
//! widget reaches only that widget's pages. Pages are stood in for by WebSocket clients on the
//! widget's own feed, as the bridge script connects.

use std::net::TcpStream;
use std::time::Duration;

use lightair_widgets::{Instance, Widgets};
use serde_json::{Value, json};
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

fn page(w: &Widgets, i: &Instance) -> WebSocket<MaybeTlsStream<TcpStream>> {
    let url = format!("ws://127.0.0.1:{}/w/{}/ws?key={}", w.port(), i.id, i.key);
    let (mut ws, _) = tungstenite::connect(url).expect("the widget's feed opens with its key");
    if let MaybeTlsStream::Plain(s) = ws.get_ref() {
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    }
    // the first message is the page's load (its settings and session)
    ws.read().expect("load message");
    ws
}

fn next_text(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>) -> Value {
    loop {
        match ws.read().expect("a message within 5 s") {
            Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
            _ => continue,
        }
    }
}

#[test]
fn a_pages_message_reaches_lightair_with_its_widget_and_lightairs_reaches_only_that_widget() {
    let store = tempfile::tempdir().unwrap();
    let w = Widgets::start(lightair_live::Hub::new(), store.path().to_path_buf()).expect("the widget server starts");
    let a = Instance::new("custom", None);
    let b = Instance::new("custom", None);
    w.set_instances(vec![a.clone(), b.clone()]);

    let (tx, rx) = crossbeam_channel::unbounded::<(String, Value)>();
    w.set_message_hook(Box::new(move |id, m| {
        let _ = tx.send((id.to_string(), m));
    }));

    let mut page_a = page(&w, &a);
    let mut page_b = page(&w, &b);
    page_a.send(Message::text(json!({"type": "message", "message": {"count": 3}}).to_string())).unwrap();
    let (from, message) = rx.recv_timeout(Duration::from_secs(5)).expect("the page's message reached Lightair");
    assert_eq!((from.as_str(), message), (a.id.as_str(), json!({"count": 3})));

    w.send_message(&b.id, &json!({"hello": "b"}));
    w.send_message(&a.id, &json!({"hello": "a"}));
    assert_eq!(next_text(&mut page_b), json!({"type": "message", "message": {"hello": "b"}}));
    assert_eq!(
        next_text(&mut page_a),
        json!({"type": "message", "message": {"hello": "a"}}),
        "a's page got only its own"
    );
}
