//! A sender's socket on one network (Wi-Fi or cellular per destination): bound to
//! an interface before it connects. iOS and macOS: `IP_BOUND_IF` with the interface's index
//! (`en0`, `pdp_ip0`); Android: `android_setsocknetwork` with the `Network`'s handle. Windows
//! has no binding of its own yet; a `Bind` there is given the socket's handle.

use std::io;
use std::net::{SocketAddr, TcpStream, UdpSocket};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
/// A socket's own handle, as a `Bind` is given it (Windows: a socket handle).
#[cfg(windows)]
pub type RawFd = std::os::windows::io::RawSocket;
use std::sync::Arc;
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};

/// Binds a socket to a network before it's used.
#[derive(Clone)]
pub struct Bind(Arc<dyn Fn(RawFd, bool) -> io::Result<()> + Send + Sync>);

impl std::fmt::Debug for Bind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Bind")
    }
}

impl PartialEq for Bind {
    fn eq(&self, other: &Bind) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Bind {}

impl Bind {
    /// Any function of (the socket, whether it's IPv6).
    pub fn new(f: impl Fn(RawFd, bool) -> io::Result<()> + Send + Sync + 'static) -> Bind {
        Bind(Arc::new(f))
    }

    /// iOS and macOS: the interface named `name` (`en0` Wi-Fi, `pdp_ip0` cellular).
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub fn interface(name: &str) -> io::Result<Bind> {
        const IP_BOUND_IF: libc::c_int = 25;
        const IPV6_BOUND_IF: libc::c_int = 125;
        let c = std::ffi::CString::new(name).map_err(|_| io::ErrorKind::InvalidInput)?;
        let index = unsafe { libc::if_nametoindex(c.as_ptr()) };
        if index == 0 {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("no network interface {name}")));
        }
        Ok(Bind::new(move |fd, v6| {
            let (level, opt) = if v6 { (libc::IPPROTO_IPV6, IPV6_BOUND_IF) } else { (libc::IPPROTO_IP, IP_BOUND_IF) };
            let i = index as libc::c_uint;
            let r = unsafe {
                libc::setsockopt(
                    fd,
                    level,
                    opt,
                    &i as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_uint>() as libc::socklen_t,
                )
            };
            if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
        }))
    }

    /// Android: the `Network` whose `getNetworkHandle()` is `handle`.
    #[cfg(target_os = "android")]
    pub fn network(handle: u64) -> Bind {
        #[link(name = "android")]
        unsafe extern "C" {
            fn android_setsocknetwork(network: u64, fd: libc::c_int) -> libc::c_int;
        }
        Bind::new(move |fd, _| {
            if unsafe { android_setsocknetwork(handle, fd) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
        })
    }

    fn apply(&self, fd: RawFd, v6: bool) -> io::Result<()> {
        (self.0)(fd, v6)
    }
}

/// `s`'s own handle.
#[cfg(unix)]
fn raw(s: &Socket) -> RawFd {
    s.as_raw_fd()
}

/// `s`'s own handle.
#[cfg(windows)]
fn raw(s: &Socket) -> RawFd {
    std::os::windows::io::AsRawSocket::as_raw_socket(s)
}

/// A TCP connection to `addr`, on `bind`'s network when given.
pub fn tcp_connect(addr: &SocketAddr, timeout: Duration, bind: Option<&Bind>) -> io::Result<TcpStream> {
    let Some(bind) = bind else {
        return TcpStream::connect_timeout(addr, timeout);
    };
    let s = Socket::new(Domain::for_address(*addr), Type::STREAM, Some(Protocol::TCP))?;
    bind.apply(raw(&s), addr.is_ipv6())?;
    s.connect_timeout(&(*addr).into(), timeout)?;
    Ok(s.into())
}

/// A UDP socket for talking to `peer`, on `bind`'s network when given (non-blocking, for tokio).
pub fn udp_for(peer: &SocketAddr, bind: Option<&Bind>) -> io::Result<UdpSocket> {
    let local: SocketAddr =
        if peer.is_ipv6() { "[::]:0".parse().expect("an address") } else { "0.0.0.0:0".parse().expect("an address") };
    let s = Socket::new(Domain::for_address(*peer), Type::DGRAM, Some(Protocol::UDP))?;
    if let Some(b) = bind {
        b.apply(raw(&s), peer.is_ipv6())?;
    }
    s.bind(&local.into())?;
    s.set_nonblocking(true)?;
    Ok(s.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bound_socket_connects_and_a_failing_bind_says_why() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let seen = calls.clone();
        let bind = Bind::new(move |_, v6| {
            assert!(!v6);
            seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        });
        tcp_connect(&addr, Duration::from_secs(2), Some(&bind)).unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        let refuse = Bind::new(|_, _| Err(io::Error::other("no such network")));
        assert!(tcp_connect(&addr, Duration::from_secs(2), Some(&refuse)).is_err());
        assert!(udp_for(&addr, Some(&bind)).is_ok());
        #[cfg(target_os = "macos")]
        {
            // the loopback interface exists everywhere
            let lo = Bind::interface("lo0").unwrap();
            tcp_connect(&addr, Duration::from_secs(2), Some(&lo)).unwrap();
            assert!(Bind::interface("nonexistent9").is_err());
        }
    }
}
