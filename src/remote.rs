// Адреса, на которых ядру можно слушать не-loopback.
//
// Единственная защита API — bearer-токен, TLS нет. Поэтому «просто открыть
// порт» нельзя: наружу ядро выставляется либо в приватной сети (tailnet,
// WireGuard, LAN — там шифрование делает транспорт), либо явным решением
// человека с флагом `--insecure-public`. Проверка адреса ловит опечатку
// (`0.0.0.0` вместо `100.x`), а не заменяет голову: whitelist приватных
// диапазонов — это эргономика, а не граница безопасности.

use std::net::IpAddr;

/// Приватные диапазоны, где слушать ядро допустимо без TLS:
/// RFC1918 (10/8, 172.16/12, 192.168/16), CGNAT/tailnet (100.64/10),
/// link-local (169.254/16), unique-local IPv6 (fc00::/7) и IPv6 loopback.
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private() || v4.is_link_local() || in_cgnat(v4) || v4.is_loopback()
        }
        IpAddr::V6(v6) => {
            // fc00::/7 — unique local; ::1 — loopback; ::ffff:a.b.c.d — IPv4-mapped.
            let seg = v6.segments()[0];
            (seg & 0xfe00) == 0xfc00 || v6.is_loopback() || mapped_v4(v6).is_some_and(is_private)
        }
    }
}

/// 100.64.0.0/10 — CGNAT. Здесь живут Tailscale и подобные оверлеи: формально
/// не RFC1918, но именно этот диапазон человек и имеет в виду под «приватно».
fn in_cgnat(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

fn mapped_v4(v6: std::net::Ipv6Addr) -> Option<IpAddr> {
    match v6.to_ipv4_mapped() {
        Some(v4) => Some(IpAddr::V4(v4)),
        None => None,
    }
}

/// Можно ли слушать этот адрес без `--insecure-public`.
pub fn allowed_without_override(ip: IpAddr) -> bool {
    ip.is_loopback() || is_private(ip)
}

/// Человеческая подсказка, что делать, если адрес не разрешён.
pub fn refusal(ip: IpAddr) -> String {
    format!(
        "адрес {ip} не в приватной сети: слушать его значит выставить API с \
         единственным bearer-токеном в открытую сеть. Возьми адрес tailnet-а/ \
         WireGuard (100.64/10, 10/8, 192.168/16) либо запусти с --insecure-public \
         осознанно (и лучше с TLS: --tls-cert/--tls-key)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn loopback_and_private_networks_are_allowed() {
        for addr in ["127.0.0.1", "10.1.2.3", "172.16.5.4", "192.168.1.10", "100.73.69.107"] {
            assert!(allowed_without_override(ip(addr)), "{addr} должен быть разрешён");
        }
        // IPv6: loopback и unique-local.
        assert!(allowed_without_override(ip("::1")));
        assert!(allowed_without_override(ip("fd7a:115c:a1e0::1")));
        // IPv4-mapped — тоже приватный, если внутри приватный.
        assert!(allowed_without_override(ip("::ffff:192.168.1.5")));
    }

    #[test]
    fn public_addresses_are_refused_with_a_hint() {
        for addr in ["8.8.8.8", "1.1.1.1", "203.0.113.9"] {
            assert!(!allowed_without_override(ip(addr)), "{addr} не должен проходить");
            assert!(refusal(ip(addr)).contains("--insecure-public"));
        }
        // 0.0.0.0 — «все интерфейсы»: это ровно тот случай, который надо ловить.
        assert!(!allowed_without_override(ip("0.0.0.0")));
        // 100.128.x — уже вне CGNAT, хотя похоже.
        assert!(!allowed_without_override(ip("100.128.0.1")));
    }
}
