//! IPv4 addresses of this PC, offered as bind addresses in the settings window.

use std::net::Ipv4Addr;

use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
    IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_UNICAST_ADDRESS_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

/// `(address, adapter name)` for every IPv4 address on an adapter that is up.
pub fn local_ipv4() -> Vec<(Ipv4Addr, String)> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size = 16 * 1024u32;
    for _ in 0..3 {
        // u64 storage keeps the adapter structs aligned.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        let first = buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        let rc =
            unsafe { GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(first), &mut size) };
        if rc == 111 {
            continue; // ERROR_BUFFER_OVERFLOW: retry with the size Windows asked for
        }
        if rc != 0 {
            return Vec::new();
        }

        let mut result = Vec::new();
        let mut adapter = first as *const IP_ADAPTER_ADDRESSES_LH;
        while let Some(a) = unsafe { adapter.as_ref() } {
            if a.OperStatus == IfOperStatusUp && a.IfType != IF_TYPE_SOFTWARE_LOOPBACK {
                let name = unsafe { a.FriendlyName.to_string() }.unwrap_or_default();
                let mut unicast = a.FirstUnicastAddress as *const IP_ADAPTER_UNICAST_ADDRESS_LH;
                while let Some(u) = unsafe { unicast.as_ref() } {
                    if let Some(sa) = unsafe { u.Address.lpSockaddr.cast::<SOCKADDR_IN>().as_ref() }
                        && sa.sin_family == AF_INET
                    {
                        let raw = unsafe { sa.sin_addr.S_un.S_addr };
                        result.push((Ipv4Addr::from(raw.to_ne_bytes()), name.clone()));
                    }
                    unicast = u.Next;
                }
            }
            adapter = a.Next;
        }
        return result;
    }
    Vec::new()
}

/// Whether `bind` can be listened on here: a wildcard, loopback, a host name, or one of
/// this PC's addresses.
pub fn is_bindable(bind: &str, local: &[(Ipv4Addr, String)]) -> bool {
    match bind.trim().parse::<Ipv4Addr>() {
        Ok(ip) => ip.is_unspecified() || ip.is_loopback() || local.iter().any(|(a, _)| *a == ip),
        Err(_) => true, // host names and IPv6 are left to the OS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_loopback_free_addresses_and_checks_bind() {
        let local = local_ipv4();
        assert!(local.iter().all(|(ip, _)| !ip.is_loopback()));
        assert!(is_bindable("0.0.0.0", &local));
        assert!(is_bindable("127.0.0.1", &local));
        assert!(is_bindable("localhost", &local));
        assert!(!is_bindable("192.0.2.1", &local));
        if let Some((ip, _)) = local.first() {
            assert!(is_bindable(&ip.to_string(), &local));
        }
    }
}
