use std::net::IpAddr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressKind {
    Public,
    Private,
    Loopback,
    Forbidden,
}

/// Conservative public-address profile based on IANA special-purpose registries.
/// Special-purpose exceptions are denied; this is not a physical-location claim.
pub fn address_kind(address: IpAddr) -> AddressKind {
    match address.to_canonical() {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            if ip.is_loopback() {
                AddressKind::Loopback
            } else if ip.is_private() || (a == 100 && (64..=127).contains(&b)) {
                AddressKind::Private
            } else if a == 0
                || a >= 224
                || ip.is_link_local()
                || (a == 192 && b == 0 && (c == 0 || c == 2))
                || (a == 192 && b == 88 && c == 99)
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113)
            {
                AddressKind::Forbidden
            } else {
                AddressKind::Public
            }
        }
        IpAddr::V6(ip) => {
            let [a, b, _, _, _, _, _, _] = ip.segments();
            if ip.is_loopback() {
                AddressKind::Loopback
            } else if ip.is_unique_local() {
                if ip
                    == "fd00:ec2::254"
                        .parse::<std::net::Ipv6Addr>()
                        .expect("constant address")
                {
                    AddressKind::Forbidden
                } else {
                    AddressKind::Private
                }
            } else if (a & 0xe000) != 0x2000
                || a == 0x2002
                || (a == 0x2001 && (b < 0x200 || b == 0xdb8))
                || (a == 0x3fff && b < 0x1000)
            {
                AddressKind::Forbidden
            } else {
                AddressKind::Public
            }
        }
    }
}
