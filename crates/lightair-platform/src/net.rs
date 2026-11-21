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

/// A network connection this computer has up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    /// The system's name for it ("en0").
    pub name: String,
    pub address: std::net::IpAddr,
}

/// The connections that are up (not loopback), one entry per address (an interface with IPv4 and
/// IPv6 addresses appears twice). Link-local IPv6 addresses are left out: they can't reach out.
/// Windows: empty for now.
pub fn interfaces() -> Vec<Interface> {
    #[cfg(unix)]
    {
        unix_interfaces()
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

#[cfg(unix)]
fn unix_interfaces() -> Vec<Interface> {
    use std::ffi::CStr;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let mut out = Vec::new();
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            let flags = ifa.ifa_flags as i32;
            if flags & libc::IFF_UP == 0 || flags & libc::IFF_LOOPBACK != 0 || ifa.ifa_addr.is_null() {
                continue;
            }
            let address = match (*ifa.ifa_addr).sa_family as i32 {
                libc::AF_INET => {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)))
                }
                libc::AF_INET6 => {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
                    // fe80::/10: usable only on its own link
                    if ip.segments()[0] & 0xffc0 == 0xfe80 {
                        continue;
                    }
                    IpAddr::V6(ip)
                }
                _ => continue,
            };
            if address.is_unspecified() {
                continue;
            }
            out.push(Interface { name: CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned(), address });
        }
        libc::freeifaddrs(list);
    }
    out
}

/// A person's name for an interface: "Wi-Fi", "Ethernet", else the system's.
pub fn interface_label(name: &str) -> String {
    #[cfg(target_os = "macos")]
    {
        // `networksetup` lists "Hardware Port: Wi-Fi / Device: en0" pairs
        use std::sync::OnceLock;
        static PORTS: OnceLock<Vec<(String, String)>> = OnceLock::new();
        let ports = PORTS.get_or_init(|| {
            std::process::Command::new("networksetup")
                .arg("-listallhardwareports")
                .output()
                .map(|o| parse_hardware_ports(&String::from_utf8_lossy(&o.stdout)))
                .unwrap_or_default()
        });
        if let Some((port, _)) = ports.iter().find(|(_, device)| device == name) {
            return port.clone();
        }
    }
    name.to_string()
}

/// (port, device) pairs from `networksetup -listallhardwareports`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_hardware_ports(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut port: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("Hardware Port:") {
            port = Some(p.trim().to_string());
        } else if let Some(d) = line.strip_prefix("Device:")
            && let Some(p) = port.take()
        {
            out.push((p, d.trim().to_string()));
        }
    }
    out
}

/// The address to bind a connection to for interface `name` and an address family (`None`:
/// either; IPv4 is preferred when both are there). `None` when that interface isn't up or has no
/// such address.
pub fn address_of(list: &[Interface], name: &str, family: Option<bool>) -> Option<std::net::IpAddr> {
    let mine = list.iter().filter(|i| i.name == name);
    let pick = |want_v6: bool| mine.clone().find(|i| i.address.is_ipv6() == want_v6).map(|i| i.address);
    match family {
        Some(v6) => pick(v6),
        None => pick(false).or_else(|| pick(true)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn iface(name: &str, ip: &str) -> Interface {
        Interface { name: name.into(), address: ip.parse::<IpAddr>().unwrap() }
    }

    #[test]
    fn an_interface_is_found_by_name_and_family() {
        let list = [iface("en0", "2001:db8::5"), iface("en0", "192.168.1.20"), iface("en5", "10.0.0.7")];
        assert_eq!(address_of(&list, "en0", None), Some("192.168.1.20".parse().unwrap()), "IPv4 preferred");
        assert_eq!(address_of(&list, "en0", Some(true)), Some("2001:db8::5".parse().unwrap()));
        assert_eq!(address_of(&list, "en5", Some(true)), None, "no IPv6 there");
        assert_eq!(address_of(&list, "en9", None), None, "not connected");
    }

    #[test]
    fn hardware_ports_read_as_names() {
        let text = "Hardware Port: Wi-Fi\nDevice: en0\nEthernet Address: aa\n\nHardware Port: Thunderbolt Ethernet\nDevice: en5\n";
        assert_eq!(
            parse_hardware_ports(text),
            vec![("Wi-Fi".to_string(), "en0".to_string()), ("Thunderbolt Ethernet".to_string(), "en5".to_string())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn this_computers_interfaces_leave_out_loopback() {
        assert!(interfaces().iter().all(|i| !i.address.is_loopback()));
    }
}
