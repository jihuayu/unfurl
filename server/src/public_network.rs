//! The fetch proxy shares a host with identity/data services. Resolve only public
//! addresses at connection time, including every redirect, to avoid DNS rebinding.
use std::{io, net::IpAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

pub struct PublicResolver;

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .collect::<Vec<_>>();
            if addresses.is_empty() || addresses.iter().any(|addr| !is_public_ip(addr.ip())) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Non-public upstream address",
                )
                .into());
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
                || (a == 192 && b == 0 && (c == 0 || c == 2))
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            // Only global unicast; exclude special-purpose and transition ranges
            // that could embed private IPv4 destinations.
            let [a, b, ..] = ip.segments();
            (0x2000..0x4000).contains(&a)
                && a != 0x2002
                && !(a == 0x2001 && (b < 0x0200 || b == 0x0db8))
                && a != 0x3fff
        }
    }
}

pub fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            attempt.error("Too many upstream redirects")
        } else if crate::utils::validate_public_url(attempt.url().as_str()).is_err() {
            attempt.error("Non-public upstream redirect")
        } else {
            attempt.follow()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_local_shared_reserved_and_ipv6_encodings() {
        for ip in [
            "0.0.0.0",
            "127.0.0.1",
            "10.0.0.1",
            "172.17.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.100.100.200",
            "224.0.0.1",
            "240.0.0.1",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
            "64:ff9b::a00:1",
            "2002:a00:1::1",
            "2001:db8::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
        ] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn resolver_rejects_a_hostname_resolving_to_loopback() {
        assert!(
            PublicResolver
                .resolve("localhost".parse().unwrap())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn redirect_cannot_reach_a_local_service() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream.local_addr().unwrap();
        let responder = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            socket.read(&mut request).await.unwrap();
            socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(redirect_policy())
            // Controlled fake public upstream; production uses PublicResolver.
            .resolve("public-upstream.example", address)
            .build()
            .unwrap();
        let error = client
            .get(format!(
                "http://public-upstream.example:{}/",
                address.port()
            ))
            .send()
            .await
            .unwrap_err();
        assert!(error.is_redirect(), "{error}");
        responder.await.unwrap();
    }

    #[test]
    fn literal_url_checks_cover_bracketed_and_mapped_ipv6() {
        for url in [
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://0x7f000001/",
            "http://2130706433/",
        ] {
            assert!(crate::utils::validate_public_url(url).is_err(), "{url}");
        }
    }
}
