//! Environment proxy resolution ⇐ pi `src/utils/node-http-proxy.ts`.

use crate::types::ProviderEnv;
use crate::utils::provider_env::get_provider_env_value;
use thiserror::Error;
use url::{Host, Url};

pub const UNSUPPORTED_PROXY_PROTOCOL_MESSAGE: &str = "Unsupported proxy protocol. SOCKS and PAC proxy URLs are not supported; use an HTTP or HTTPS proxy URL.";

#[derive(Debug, Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ProxyResolutionError(pub String);

fn proxy_env(key: &str, env: Option<&ProviderEnv>) -> String {
    let lowercase = key.to_lowercase();
    let uppercase = key.to_uppercase();
    env.and_then(|env| env.get(&lowercase))
        .filter(|value| !value.is_empty())
        .or_else(|| {
            env.and_then(|env| env.get(&uppercase))
                .filter(|value| !value.is_empty())
        })
        .cloned()
        .or_else(|| get_provider_env_value(&lowercase, None))
        .or_else(|| get_provider_env_value(&uppercase, None))
        .unwrap_or_default()
}

fn default_port(protocol: &str) -> u16 {
    match protocol {
        "ftp" => 21,
        "gopher" => 70,
        "http" | "ws" => 80,
        "https" | "wss" => 443,
        _ => 0,
    }
}

fn should_proxy_hostname(hostname: &str, port: u16, env: Option<&ProviderEnv>) -> bool {
    let no_proxy = proxy_env("no_proxy", env).to_lowercase();
    if no_proxy.is_empty() {
        return true;
    }
    if no_proxy == "*" {
        return false;
    }
    no_proxy
        .split(|character: char| character == ',' || character.is_whitespace())
        .filter(|value| !value.is_empty())
        .all(|value| {
            let (mut proxy_hostname, proxy_port) = value
                .rsplit_once(':')
                .and_then(|(host, port)| port.parse::<u16>().ok().map(|port| (host, port)))
                .map_or((value, 0), |value| value);
            if proxy_port != 0 && proxy_port != port {
                return true;
            }
            if !proxy_hostname.starts_with(['.', '*']) {
                return hostname != proxy_hostname;
            }
            if let Some(stripped) = proxy_hostname.strip_prefix('*') {
                proxy_hostname = stripped;
            }
            !hostname.ends_with(proxy_hostname)
        })
}

fn proxy_for_url(target: &Url, env: Option<&ProviderEnv>) -> String {
    let protocol = target.scheme();
    let Some(host) = target.host() else {
        return String::new();
    };
    let hostname = match host {
        Host::Ipv6(address) => format!("[{address}]"),
        _ => host.to_string(),
    };
    let port = target.port().unwrap_or_else(|| default_port(protocol));
    if !should_proxy_hostname(&hostname, port, env) {
        return String::new();
    }
    let mut proxy = proxy_env(&format!("{protocol}_proxy"), env);
    if proxy.is_empty() {
        proxy = proxy_env("all_proxy", env);
    }
    if !proxy.is_empty() && !proxy.contains("://") {
        proxy = format!("{protocol}://{proxy}");
    }
    proxy
}

pub trait ProxyTarget {
    fn proxy_target_url(&self) -> Option<Url>;
}

impl ProxyTarget for str {
    fn proxy_target_url(&self) -> Option<Url> {
        Url::parse(self).ok()
    }
}

impl ProxyTarget for String {
    fn proxy_target_url(&self) -> Option<Url> {
        self.as_str().proxy_target_url()
    }
}

impl ProxyTarget for Url {
    fn proxy_target_url(&self) -> Option<Url> {
        Some(self.clone())
    }
}

pub fn resolve_http_proxy_url_for_target<T: ProxyTarget + ?Sized>(
    target_url: &T,
    env: Option<&ProviderEnv>,
) -> Result<Option<Url>, ProxyResolutionError> {
    let Some(target) = target_url.proxy_target_url() else {
        return Ok(None);
    };
    let proxy = proxy_for_url(&target, env);
    if proxy.is_empty() {
        return Ok(None);
    }
    let proxy_url = Url::parse(&proxy).map_err(|error| {
        ProxyResolutionError(format!(
            "Invalid proxy URL {}: {error}",
            serde_json::to_string(&proxy).expect("string serializes")
        ))
    })?;
    if !matches!(proxy_url.scheme(), "http" | "https") {
        return Err(ProxyResolutionError(format!(
            "{UNSUPPORTED_PROXY_PROTOCOL_MESSAGE} Got {}:",
            proxy_url.scheme()
        )));
    }
    Ok(Some(proxy_url))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ProviderEnv;

    #[test]
    fn scoped_proxy_and_no_proxy_match_pi() {
        let env = ProviderEnv::from([
            ("https_proxy".to_owned(), "proxy.test:8080".to_owned()),
            (
                "no_proxy".to_owned(),
                ".internal.test, api.skip.test:444".to_owned(),
            ),
        ]);
        assert_eq!(
            resolve_http_proxy_url_for_target("https://api.example.test/v1", Some(&env))
                .expect("proxy")
                .expect("present")
                .as_str(),
            "https://proxy.test:8080/"
        );
        assert!(
            resolve_http_proxy_url_for_target("https://foo.internal.test", Some(&env))
                .expect("valid")
                .is_none()
        );

        let schemes = ProviderEnv::from([
            (
                "http_proxy".to_owned(),
                "http://http-proxy.test:8080".to_owned(),
            ),
            (
                "https_proxy".to_owned(),
                "https://https-proxy.test:8443".to_owned(),
            ),
            ("no_proxy".to_owned(), "never-match.test".to_owned()),
        ]);
        assert_eq!(
            resolve_http_proxy_url_for_target("http://service.test", Some(&schemes))
                .expect("valid")
                .expect("proxy")
                .as_str(),
            "http://http-proxy.test:8080/"
        );
        assert_eq!(
            resolve_http_proxy_url_for_target("https://service.test", Some(&schemes))
                .expect("valid")
                .expect("proxy")
                .as_str(),
            "https://https-proxy.test:8443/"
        );

        for proxy in [
            "socks5://proxy.test:1080",
            "pac+http://proxy.test/proxy.pac",
        ] {
            let env = ProviderEnv::from([
                ("https_proxy".to_owned(), proxy.to_owned()),
                ("no_proxy".to_owned(), "never-match.test".to_owned()),
            ]);
            assert_eq!(
                resolve_http_proxy_url_for_target("https://service.test", Some(&env))
                    .expect_err("unsupported")
                    .0,
                format!(
                    "{UNSUPPORTED_PROXY_PROTOCOL_MESSAGE} Got {}:",
                    proxy.split(':').next().expect("scheme")
                )
            );
        }
    }

    /// Ports pi `src/utils/node-http-proxy.ts:25-28`'s pre-parsed URL branch.
    #[test]
    fn accepts_preparsed_url_targets() {
        let env = ProviderEnv::from([(
            "https_proxy".to_owned(),
            "https://proxy.test:8443".to_owned(),
        )]);
        let target = Url::parse("https://service.test/path").expect("target");
        assert_eq!(
            resolve_http_proxy_url_for_target(&target, Some(&env))
                .expect("valid")
                .expect("proxy")
                .as_str(),
            "https://proxy.test:8443/"
        );
    }
}
