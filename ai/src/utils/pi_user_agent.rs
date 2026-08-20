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
        std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|release| release.trim().to_owned())
            .filter(|release| !release.is_empty())
            .unwrap_or_else(|| "unknown".to_owned())
    })
}

#[cfg(not(target_family = "wasm"))]
pub fn get_pi_user_agent() -> String {
    format!("pi ({} {}; {})", platform(), os_release(), architecture())
}

#[cfg(target_family = "wasm")]
pub fn get_pi_user_agent() -> String {
    "pi (browser)".to_owned()
}

#[cfg(test)]
mod tests {
    use super::get_pi_user_agent;

    /// Derived from pi `src/utils/pi-user-agent.ts:17-18`.
    #[test]
    fn matches_native_pi_user_agent_shape() {
        let value = get_pi_user_agent();
        assert!(value.starts_with("pi ("));
        assert!(value.ends_with(')'));
        assert!(value.contains("; "));
    }
}
