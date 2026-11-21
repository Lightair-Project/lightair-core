//! Over the network, by hand: HTTPS and WSS through rustls and the system's verifier, and a
//! certificate the system doesn't trust refused. `cargo run -p lightair-live --example tls_check`

use lightair_live::net;

fn main() {
    let body = net::get(&net::agent(), "https://www.example.com/", &[]).unwrap();
    assert!(body.contains("Example Domain"));
    let mut socket = net::connect("wss://ws.postman-echo.com/raw", std::time::Duration::from_secs(5)).unwrap();
    socket.send(tungstenite::Message::text("lightair")).unwrap();
    let reply = loop {
        if let Some(m) = net::read(&mut socket).unwrap() {
            break m;
        }
    };
    assert_eq!(reply.to_text().unwrap(), "lightair");
    assert!(net::get(&net::agent(), "https://self-signed.badssl.com/", &[]).is_err(), "an untrusted certificate");
    println!("ok");
}
