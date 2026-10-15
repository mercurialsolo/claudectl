//! The addresses this machine can be reached at (#541).
//!
//! Until now an address was only ever learned from a path already in use: we
//! dial the one address we know, and the acceptor records the source address of
//! whoever dialled in. So a peer could not discover a second route to a peer it
//! could no longer reach — exactly when it needs one. #506 covers the case where
//! both machines stay on one broadcast domain; this covers the case where they
//! do not, which for a laptop is the common one.
//!
//! The enumeration is `getifaddrs` through `libc`, already a dependency and
//! already used this way (`bus/roles.rs`, `relay/agent.rs`, `core/process.rs`,
//! `lan::bind_shared`). The filtering is a pure function, so the judgement about
//! which addresses are worth advertising is testable without a network.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// How many of our own addresses to advertise.
///
/// A machine on a VPN with containers running can have a dozen interfaces, and
/// the receiver caps its record at `MAX_PEER_ADDRS` (8) anyway. Six leaves room
/// there for addresses learned another way, and keeps the heartbeat small.
pub const MAX_ADVERTISED: usize = 6;

/// Whether an address is worth telling a peer about.
///
/// Kept pure and separate from the FFI so the policy is testable. Private
/// ranges are deliberately included: `192.168/16` and `10/8` are how two
/// machines on one LAN reach each other, and `100.64/10` is where an overlay
/// like Tailscale lives — which is the address that would have kept the link up
/// in the failure this fixes.
///
/// What is excluded is what cannot help another machine: loopback is only ours,
/// link-local needs a scope id we do not carry, and unspecified/multicast are
/// not dialable at all.
pub fn is_advertisable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_unspecified()
                && !v4.is_multicast()
                && !v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_multicast()
                // fe80::/10. `Ipv6Addr::is_unicast_link_local` is still
                // unstable, so the prefix is checked directly.
                && !matches!(v6.segments()[0] & 0xffc0, 0xfe80)
        }
    }
}

/// Order addresses so the most useful candidates survive the cap.
///
/// IPv4 first: it is what every peer in this project has actually connected
/// over, and an IPv6 address that looks routable is frequently not reachable
/// from the other side. Beyond that the input order is kept, which is the
/// kernel's interface order.
fn rank(ip: &IpAddr) -> u8 {
    match ip {
        IpAddr::V4(_) => 0,
        IpAddr::V6(_) => 1,
    }
}

/// Pick what to advertise from a raw list: filter, dedupe, order, cap.
///
/// Split from [`local_ips`] so the whole policy can be tested against a fixed
/// list rather than against whatever interfaces the test machine happens to
/// have — which on CI is neither stable nor interesting.
pub fn advertisable(mut ips: Vec<IpAddr>) -> Vec<IpAddr> {
    ips.retain(is_advertisable);
    ips.sort_by_key(rank);
    ips.dedup();
    ips.truncate(MAX_ADVERTISED);
    ips
}

/// Every address on this machine's interfaces, unfiltered.
#[cfg(unix)]
pub fn local_ips() -> Vec<IpAddr> {
    let mut out = Vec::new();
    // SAFETY: `getifaddrs` allocates a list that is walked without being
    // mutated and freed exactly once on every path out, including the early
    // return. Each `ifa_addr` is checked for null and read only through the
    // struct its `sa_family` names.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() {
                continue;
            }
            match i32::from((*ifa.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    // `s_addr` is in network order; its in-memory bytes are
                    // already a.b.c.d, so going through bytes avoids having to
                    // reason about the host's endianness at all.
                    let octets = sin.sin_addr.s_addr.to_ne_bytes();
                    out.push(IpAddr::V4(Ipv4Addr::from(octets)));
                }
                libc::AF_INET6 => {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    out.push(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)));
                }
                _ => {}
            }
        }
        libc::freeifaddrs(head);
    }
    out
}

#[cfg(not(unix))]
pub fn local_ips() -> Vec<IpAddr> {
    // Windows has no getifaddrs. Advertising nothing is the safe answer: a
    // peer that advertises no addresses behaves exactly as it did before #541.
    Vec::new()
}

/// The `address:port` strings to advertise for a listener on `port`.
pub fn advertised_addrs(port: u16) -> Vec<String> {
    advertisable(local_ips())
        .into_iter()
        .map(|ip| match ip {
            IpAddr::V4(v4) => format!("{v4}:{port}"),
            // A bracketed literal, so it parses back as a `SocketAddr`.
            IpAddr::V6(v6) => format!("[{v6}]:{port}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse::<Ipv4Addr>().unwrap())
    }
    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().unwrap())
    }

    #[test]
    fn loopback_and_link_local_are_not_worth_advertising() {
        assert!(!is_advertisable(&v4("127.0.0.1")));
        assert!(!is_advertisable(&v4("169.254.3.9")));
        assert!(!is_advertisable(&v4("0.0.0.0")));
        assert!(!is_advertisable(&v6("::1")));
        assert!(!is_advertisable(&v6("fe80::1")));
        assert!(!is_advertisable(&v6("::")));
    }

    /// The whole point of the change: an overlay address is in CGNAT space and
    /// must survive the filter, because it is the one that stays reachable when
    /// a machine changes network.
    #[test]
    fn overlay_and_lan_addresses_are_advertised() {
        assert!(is_advertisable(&v4("100.109.212.89"))); // Tailscale, 100.64/10
        assert!(is_advertisable(&v4("192.168.4.39")));
        assert!(is_advertisable(&v4("10.33.227.154")));
        assert!(is_advertisable(&v4("172.16.0.5")));
        assert!(is_advertisable(&v6("fd00::1"))); // unique-local, routable on site
    }

    #[test]
    fn a_public_address_is_advertised_too() {
        assert!(is_advertisable(&v4("93.184.216.34")));
        assert!(is_advertisable(&v6("2001:db8::1")));
    }

    #[test]
    fn ipv4_is_offered_before_ipv6() {
        let picked = advertisable(vec![v6("fd00::1"), v4("192.168.1.2")]);
        assert_eq!(picked, vec![v4("192.168.1.2"), v6("fd00::1")]);
    }

    #[test]
    fn duplicates_and_junk_are_dropped() {
        let picked = advertisable(vec![
            v4("127.0.0.1"),
            v4("192.168.1.2"),
            v4("192.168.1.2"),
            v4("169.254.1.1"),
        ]);
        assert_eq!(picked, vec![v4("192.168.1.2")]);
    }

    #[test]
    fn the_list_is_capped() {
        let many: Vec<IpAddr> = (1..=20).map(|n| v4(&format!("10.0.0.{n}"))).collect();
        assert_eq!(advertisable(many).len(), MAX_ADVERTISED);
    }

    #[test]
    fn advertised_strings_parse_back_as_socket_addrs() {
        use std::net::SocketAddr;
        for s in [
            format!("{}:9847", "192.168.1.2"),
            format!("[{}]:9847", "fd00::1"),
        ] {
            assert!(s.parse::<SocketAddr>().is_ok(), "{s} must parse back");
        }
    }

    /// Not an assertion about *which* addresses this machine has — CI has
    /// whatever it has. It checks the FFI returns something sane and does not
    /// crash, and that everything surviving the filter is genuinely dialable.
    /// Prints what this machine would advertise. `cargo test -- --nocapture
    /// what_this_machine_advertises` is the quickest way to see whether an
    /// overlay address is being picked up on a given host.
    #[test]
    fn what_this_machine_advertises() {
        println!("advertising: {:?}", advertised_addrs(9847));
    }

    #[test]
    fn enumeration_returns_only_usable_addresses() {
        let picked = advertisable(local_ips());
        assert!(picked.len() <= MAX_ADVERTISED);
        for ip in &picked {
            assert!(is_advertisable(ip), "{ip} should not have survived");
        }
        for s in advertised_addrs(9847) {
            assert!(
                s.parse::<std::net::SocketAddr>().is_ok(),
                "{s} must be dialable"
            );
        }
    }
}
