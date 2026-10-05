//! HTTP admission is about the actual destination addresses, including DNS results.
use super::*;
use std::net::IpAddr;

fn private_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || (ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.segments()[0] & 0xfe00 == 0xfc00
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|ip| private_address(IpAddr::V4(ip)))
        }
    }
}

struct PrivateDns;
fn admitted_addresses(addresses: Vec<std::net::SocketAddr>) -> io::Result<reqwest::dns::Addrs> {
    if addresses.is_empty()
        || addresses
            .iter()
            .any(|address| !private_address(address.ip()))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "HTTP requires loopback, private or tailnet addresses; use HTTPS or an explicit --allow-public-http override on an already-encrypted path",
        ));
    }
    Ok(Box::new(addresses.into_iter()))
}
impl reqwest::dns::Resolve for PrivateDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .collect::<Vec<_>>();
            Ok(admitted_addresses(addresses)?)
        })
    }
}

pub(super) fn validate(url: &reqwest::Url, allow_public_http: bool) -> Result<()> {
    if url.scheme() == "http" && !allow_public_http {
        let host = url.host_str().context("HTTP origin needs a host")?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(address) = host.parse::<IpAddr>() {
            ensure!(
                private_address(address),
                "HTTP requires loopback, private or tailnet addresses; use HTTPS or --allow-public-http on an already-encrypted path"
            );
        }
        // A hostname is checked by PrivateDns when it resolves for the actual connection.
    }
    Ok(())
}

pub(super) fn client(endpoint: &str, allow_public_http: bool) -> Result<reqwest::Client> {
    let url = reqwest::Url::parse(endpoint)?;
    validate(&url, allow_public_http)?;
    let mut client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if url.scheme() == "http" {
        eprintln!(
            "HTTP carries pairing codes and credentials across this address. It must already be an encrypted path, such as Tailscale/WireGuard or an SSH tunnel."
        );
        if !allow_public_http {
            // An ambient proxy must not bypass the checked destination or carry the code to
            // a public proxy. The resolver returns only the addresses it just admitted.
            client = client.no_proxy().dns_resolver(Arc::new(PrivateDns));
        }
    }
    Ok(client.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dns_rejects_empty_and_mixed_public_answers() {
        assert!(admitted_addresses(vec![]).is_err());
        let private = "100.64.1.2:0".parse().unwrap();
        let public = "203.0.113.1:0".parse().unwrap();
        assert!(admitted_addresses(vec![private, public]).is_err());
        let admitted = admitted_addresses(vec![private, "[::1]:0".parse().unwrap()])
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(admitted.len(), 2);
        assert_eq!(admitted[0], private);
    }

    #[test]
    fn http_admits_only_loopback_private_and_tailnet_literals_by_default() {
        for host in [
            "127.0.0.1",
            "10.2.3.4",
            "172.16.0.1",
            "192.168.1.2",
            "100.64.0.1",
            "100.127.255.254",
            "[::1]",
            "[fd7a:115c:a1e0::1]",
            "[::ffff:127.0.0.1]",
        ] {
            validate(
                &reqwest::Url::parse(&format!("http://{host}")).unwrap(),
                false,
            )
            .unwrap();
        }
        for host in [
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.1",
            "172.32.0.1",
            "169.254.169.254",
            "[2606:4700:4700::1111]",
            "[::ffff:8.8.8.8]",
        ] {
            let url = reqwest::Url::parse(&format!("http://{host}")).unwrap();
            assert!(validate(&url, false).is_err());
            validate(&url, true).unwrap();
        }
        validate(&reqwest::Url::parse("https://8.8.8.8").unwrap(), false).unwrap();
    }
}
