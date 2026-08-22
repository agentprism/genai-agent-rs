//! pi user-agent construction ⇐ pi `src/utils/pi-user-agent.ts`.

use std::sync::OnceLock;

#[cfg(not(target_family = "wasm"))]
fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "solaris" => "sunos",
        "windows" => "win32",
        value => value,
    }
}

#[cfg(not(target_family = "wasm"))]
fn architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86" => "ia32",
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "loongarch64" => "loong64",
        "powerpc" => "ppc",
        "powerpc64" => "ppc64",
        value => value,
    }
}

#[cfg(not(target_family = "wasm"))]
fn os_release() -> &'static str {
    static RELEASE: OnceLock<String> = OnceLock::new();
    RELEASE.get_or_init(|| {
        rustix::system::uname()
            .release()
            .to_str()
            .ok()
            .map(str::to_owned)
            .filter(|release| !release.is_empty())
            .unwrap_or_else(|| "unknown".to_owned())
    })
}

#[cfg(not(target_family = "wasm"))]
pub fn get_pi_user_agent() -> String {
    format!("pi ({} {}; {})", platform(), os_release(), architecture())
}

#[cfg(not(target_family = "wasm"))]
pub fn openai_sdk_platform_headers(timeout_ms: Option<f64>) -> Vec<(String, String)> {
    let operating_system = match std::env::consts::OS {
        "macos" => "MacOS".to_owned(),
        "android" => "Android".to_owned(),
        "freebsd" => "FreeBSD".to_owned(),
        "openbsd" => "OpenBSD".to_owned(),
        "linux" => "Linux".to_owned(),
        other => format!("Other:{other}"),
    };
    let architecture = match std::env::consts::ARCH {
        "x86" => "x32".to_owned(),
        "x86_64" => "x64".to_owned(),
        "arm" => "arm".to_owned(),
        "aarch64" => "arm64".to_owned(),
        other => format!("other:{other}"),
    };
    static NODE_VERSION: OnceLock<String> = OnceLock::new();
    let runtime_version = NODE_VERSION.get_or_init(|| {
        std::process::Command::new("node")
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|version| version.trim().to_owned())
            .filter(|version| !version.is_empty())
            .unwrap_or_else(|| "unknown".to_owned())
    });
    let mut headers = vec![("X-Stainless-Retry-Count".to_owned(), "0".to_owned())];
    // The SDK reads the per-request options pi passes (client.js:578-618), and pi sets
    // `timeout` only when `timeoutMs` is given (openai-completions.ts:308,
    // openai-responses.ts:148); the client default never reaches this header.
    if let Some(timeout_ms) = timeout_ms
        && timeout_ms != 0.0
        && !timeout_ms.is_nan()
    {
        headers.push((
            "X-Stainless-Timeout".to_owned(),
            crate::utils::error_body::js_f64_string((timeout_ms / 1_000.0).trunc()),
        ));
    }
    headers.extend([
        ("X-Stainless-Lang".to_owned(), "js".to_owned()),
        (
            "X-Stainless-Package-Version".to_owned(),
            "6.40.0".to_owned(),
        ),
        ("X-Stainless-OS".to_owned(), operating_system),
        ("X-Stainless-Arch".to_owned(), architecture),
        ("X-Stainless-Runtime".to_owned(), "node".to_owned()),
        (
            "X-Stainless-Runtime-Version".to_owned(),
            runtime_version.clone(),
        ),
    ]);
    headers
}

#[cfg(target_family = "wasm")]
pub fn get_pi_user_agent() -> String {
    "pi (browser)".to_owned()
}

#[cfg(test)]
mod tests {
    use super::{get_pi_user_agent, openai_sdk_platform_headers};

    /// Derived from pi `src/utils/pi-user-agent.ts:17-18`.
    #[test]
    fn matches_native_pi_user_agent_shape() {
        let value = get_pi_user_agent();
        assert!(value.starts_with("pi ("));
        assert!(value.ends_with(')'));
        assert!(value.contains("; "));
    }

    /// Pins OpenAI JavaScript SDK 6.40.0 `client.js:578-627` header semantics: the timeout header
    /// exists only for a per-request timeout (pi `openai-completions.ts:308`).
    #[test]
    fn stainless_platform_headers_preserve_javascript_timeout_coercion() {
        let header = |timeout| {
            openai_sdk_platform_headers(timeout)
                .into_iter()
                .find_map(|(name, value)| (name == "X-Stainless-Timeout").then_some(value))
        };
        assert_eq!(header(None), None);
        assert_eq!(header(Some(600_000.0)).as_deref(), Some("600"));
        assert_eq!(header(Some(1_500.9)).as_deref(), Some("1"));
        assert_eq!(header(Some(-500.0)).as_deref(), Some("0"));
        assert_eq!(header(Some(f64::INFINITY)).as_deref(), Some("Infinity"));
        assert_eq!(header(Some(0.0)), None);
        assert_eq!(header(Some(f64::NAN)), None);

        let headers = openai_sdk_platform_headers(None);
        for name in [
            "X-Stainless-Retry-Count",
            "X-Stainless-Lang",
            "X-Stainless-Package-Version",
            "X-Stainless-OS",
            "X-Stainless-Arch",
            "X-Stainless-Runtime",
            "X-Stainless-Runtime-Version",
        ] {
            assert!(headers.iter().any(|(candidate, _)| candidate == name));
        }
    }
}
