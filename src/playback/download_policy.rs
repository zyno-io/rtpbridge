//! Destination validation is applied to every redirect and to the exact IPs
//! passed to the connector, with environment proxies disabled.
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};

use ipnet::IpNet;
use reqwest::{Client, Response, Url};

#[derive(Clone, Debug, Default)]
pub struct DownloadPolicy {
    origins: Vec<String>,
    networks: Vec<IpNet>,
    loopback_only: bool,
}

impl DownloadPolicy {
    pub fn new(origins: &[String], networks: Vec<IpNet>) -> anyhow::Result<Self> {
        let mut normalized = Vec::new();
        for origin in origins {
            let url = Url::parse(origin)
                .map_err(|_| anyhow::anyhow!("invalid file_download_origins URL"))?;
            validate_url(&url)?;
            if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
                anyhow::bail!(
                    "file_download_origins must be origins without paths, queries or fragments"
                );
            }
            normalized.push(url.origin().ascii_serialization());
        }
        Ok(Self {
            origins: normalized,
            networks,
            loopback_only: false,
        })
    }

    /// Explicit local-fixture policy for embedded/test servers. Never selected
    /// by production configuration or by the default constructor.
    #[allow(dead_code)]
    pub fn loopback_only() -> Self {
        Self {
            loopback_only: true,
            ..Self::default()
        }
    }

    pub fn validate(&self, raw: &str) -> anyhow::Result<Url> {
        let url = Url::parse(raw).map_err(|_| anyhow::anyhow!("invalid playback URL"))?;
        validate_url(&url)?;
        let literal_loopback = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip).to_canonical().is_loopback(),
            _ => false,
        };
        if !(self.origins.contains(&url.origin().ascii_serialization())
            || self.loopback_only && literal_loopback)
        {
            anyhow::bail!("playback URL origin is not allowed");
        }
        Ok(url)
    }

    fn allowed_ip(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        if self.loopback_only {
            return ip.is_loopback();
        }
        self.networks.iter().any(|network| network.contains(&ip)) || public_address(ip)
    }

    pub async fn response(
        &self,
        raw: &str,
        headers: Option<&std::collections::HashMap<String, String>>,
    ) -> anyhow::Result<Response> {
        let mut url = self.validate(raw)?;
        let mut forward_headers = true;
        for hop in 0..=5 {
            let host = url
                .host_str()
                .ok_or_else(|| anyhow::anyhow!("playback URL has no host"))?;
            let port = url
                .port_or_known_default()
                .ok_or_else(|| anyhow::anyhow!("playback URL has no port"))?;
            let addresses: Vec<SocketAddr> = match url.host() {
                Some(url::Host::Ipv4(ip)) => vec![SocketAddr::new(ip.into(), port)],
                Some(url::Host::Ipv6(ip)) => vec![SocketAddr::new(ip.into(), port)],
                _ => {
                    let host = host.to_owned();
                    let resolved = resolve_addresses(move || {
                        (host.as_str(), port)
                            .to_socket_addrs()
                            .map(|addresses| addresses.take(64).collect())
                    })
                    .await;
                    resolved?
                }
            };
            if addresses.is_empty()
                || addresses
                    .iter()
                    .any(|address| !self.allowed_ip(address.ip()))
            {
                anyhow::bail!("playback destination address is not allowed");
            }
            // A per-hop pinned client prevents a second unrestricted resolution
            // and cross-policy pool reuse. TLS still verifies the original name.
            let client = Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(std::time::Duration::from_secs(10))
                .resolve_to_addrs(host, &addresses)
                .build()
                .map_err(|_| anyhow::anyhow!("playback HTTP client initialization failed"))?;
            let mut request = client.get(url.clone());
            if forward_headers && let Some(headers) = headers {
                for (name, value) in headers {
                    let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                        .map_err(|_| anyhow::anyhow!("invalid playback header name"))?;
                    if matches!(
                        name.as_str(),
                        "host"
                            | "connection"
                            | "content-length"
                            | "transfer-encoding"
                            | "upgrade"
                            | "forwarded"
                            | "proxy-authorization"
                    ) || name.as_str().starts_with("x-forwarded-")
                    {
                        anyhow::bail!("playback routing/hop-by-hop header is not allowed");
                    }
                    let value = reqwest::header::HeaderValue::from_str(value)
                        .map_err(|_| anyhow::anyhow!("invalid playback header value"))?;
                    request = request.header(name, value);
                }
            }
            let received = request.send().await;
            let response = received.map_err(|_| anyhow::anyhow!("playback HTTP request failed"))?;
            if response.status().is_redirection() {
                if hop == 5 {
                    anyhow::bail!("playback redirect limit exceeded");
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| anyhow::anyhow!("invalid playback redirect"))?;
                let next = url
                    .join(location)
                    .map_err(|_| anyhow::anyhow!("invalid playback redirect"))?;
                let next = self.validate(next.as_str())?;
                if url.scheme() == "https" && next.scheme() != "https" {
                    anyhow::bail!("playback HTTPS downgrade is forbidden");
                }
                if next.origin() != url.origin() {
                    forward_headers = false;
                }
                url = next;
                continue;
            }
            if !response.status().is_success() {
                anyhow::bail!("Download failed with status {}", response.status().as_u16());
            }
            return Ok(response);
        }
        unreachable!("redirect loop is bounded")
    }
}

async fn resolve_addresses(
    resolve: impl FnOnce() -> std::io::Result<Vec<SocketAddr>> + Send + 'static,
) -> anyhow::Result<Vec<SocketAddr>> {
    // A cancelled lookup cannot interrupt the system resolver. The real job
    // owns admission until it exits, independently of download cancellation.
    // Detached, bounded threads also avoid holding Tokio runtime shutdown open.
    static DNS_ADMISSION: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
    let admission = DNS_ADMISSION
        .try_acquire()
        .map_err(|_| anyhow::anyhow!("DNS_BUSY"))?;
    let (reply, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("rtp-dns".into())
        .spawn(move || {
            let _admission = admission;
            if !reply.is_closed() {
                let _ = reply.send(resolve());
            }
        })
        .map_err(|_| anyhow::anyhow!("playback DNS worker unavailable"))?;
    let resolved = receiver.await;
    resolved
        .map_err(|_| anyhow::anyhow!("playback DNS worker failed"))?
        .map_err(|_| anyhow::anyhow!("playback DNS resolution failed"))
}

fn validate_url(url: &Url) -> anyhow::Result<()> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!("playback requires an HTTP(S) URL without userinfo or fragment");
    }
    Ok(())
}

fn public_address(ip: IpAddr) -> bool {
    const V4_DENY: &[&str] = &[
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.0.0.0/24",
        "192.0.2.0/24",
        "192.168.0.0/16",
        "198.18.0.0/15",
        "198.51.100.0/24",
        "203.0.113.0/24",
        "224.0.0.0/4",
        "240.0.0.0/4",
    ];
    match ip {
        IpAddr::V4(_) => !V4_DENY.iter().any(|cidr| {
            cidr.parse::<IpNet>()
                .expect("constant network")
                .contains(&ip)
        }),
        IpAddr::V6(_) => {
            "2000::/3".parse::<IpNet>().unwrap().contains(&ip)
                && !["2001::/23", "2001:db8::/32", "2002::/16"]
                    .iter()
                    .any(|cidr| cidr.parse::<IpNet>().unwrap().contains(&ip))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_dns_waiters_cannot_accumulate_resolver_jobs() {
        let mut releases = Vec::new();
        let mut callers = Vec::new();
        for _ in 0..4 {
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let (started, running) = tokio::sync::oneshot::channel();
            callers.push(tokio::spawn(resolve_addresses(move || {
                let _ = started.send(());
                let _ = wait.recv();
                Ok(vec!["127.0.0.1:80".parse().unwrap()])
            })));
            let started = running.await;
            started.unwrap();
            releases.push(release);
        }
        for caller in callers {
            caller.abort();
            let cancelled = caller.await;
            assert!(cancelled.unwrap_err().is_cancelled());
        }
        // The cancelled waiters must not admit new work while all four actual
        // resolver calls are still blocked. The closure must never run.
        let excess = resolve_addresses(|| panic!("resolver budget exceeded")).await;
        assert!(excess.unwrap_err().to_string().contains("DNS_BUSY"));
        drop(releases);
        let recovered = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let resolved = resolve_addresses(|| {
                    ("localhost", 80)
                        .to_socket_addrs()
                        .map(|addresses| addresses.collect())
                })
                .await;
                match resolved {
                    Ok(addresses) => break addresses,
                    Err(error) if error.to_string().contains("DNS_BUSY") => {
                        tokio::task::yield_now().await;
                    }
                    Err(error) => panic!("resolution did not recover: {error}"),
                }
            }
        })
        .await;
        assert!(!recovered.unwrap().is_empty());
    }

    async fn http_fixture(response: String) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await;
        let listener = listener.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let accepted = listener.accept().await;
            let (mut socket, _) = accepted.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 1024];
                let read = socket.read(&mut buffer).await;
                let count = read.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = socket.write_all(response.as_bytes()).await;
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}"), task)
    }

    #[tokio::test]
    async fn redirect_revalidates_origin_and_strips_all_caller_headers() {
        let (target, target_task) = http_fixture(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into(),
        )
        .await;
        let (origin, origin_task) = http_fixture(format!("HTTP/1.1 302 Found\r\nLocation: {target}/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")).await;
        let policy = DownloadPolicy::new(
            &[origin.clone(), target],
            vec!["127.0.0.0/8".parse().unwrap()],
        )
        .unwrap();
        let headers = std::collections::HashMap::from([
            ("Authorization".into(), "Bearer regression-secret".into()),
            ("X-Custom-Secret".into(), "custom-secret".into()),
        ]);
        let response = policy
            .response(&format!("{origin}/signed?secret=hidden"), Some(&headers))
            .await;
        assert!(response.is_ok());
        let first = origin_task.await;
        let second = target_task.await;
        assert!(first.unwrap().contains("regression-secret"));
        let second = second.unwrap();
        assert!(!second.contains("regression-secret") && !second.contains("custom-secret"));
    }

    #[tokio::test]
    async fn forbidden_redirect_never_reaches_destination() {
        let (origin, task) = http_fixture("HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/credential-sentinel\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()).await;
        let policy = DownloadPolicy::new(
            std::slice::from_ref(&origin),
            vec!["127.0.0.0/8".parse().unwrap()],
        )
        .unwrap();
        let response = policy.response(&origin, None).await;
        let error = response.unwrap_err().to_string();
        assert!(error.contains("not allowed"));
        assert!(!error.contains("credential-sentinel"));
        let _ = task.await;
    }

    #[test]
    fn destination_policy_rejects_unconfigured_origins_and_special_addresses() {
        let policy = DownloadPolicy::new(&["https://media.example".into()], vec![]).unwrap();
        assert!(policy.validate("https://media.example/audio.wav").is_ok());
        for url in [
            "http://media.example/audio.wav",
            "https://media.example:8443/a",
            "https://user:secret@media.example/a",
            "http://127.0.0.1/a",
            "file:///etc/passwd",
        ] {
            assert!(policy.validate(url).is_err());
        }
        for ip in [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.1",
            "100.64.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "fe80::1",
            "fd00::1",
        ] {
            assert!(!policy.allowed_ip(ip.parse().unwrap()), "{ip}");
        }
        let private = DownloadPolicy::new(
            &["https://media.example".into()],
            vec!["10.1.0.0/16".parse().unwrap()],
        )
        .unwrap();
        assert!(private.allowed_ip("10.1.2.3".parse().unwrap()));
        assert!(!private.allowed_ip("10.2.2.3".parse().unwrap()));
    }
}
