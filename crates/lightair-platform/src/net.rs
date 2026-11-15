//! The network, as far as Lightair needs it.

/// This computer's address on its network (the one other devices reach it at), if it has one.
pub fn local_ip() -> Option<String> {
    // a UDP socket "connected" to an outside address picks the interface that route uses;
    // nothing is sent
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then(|| ip.to_string())
}
