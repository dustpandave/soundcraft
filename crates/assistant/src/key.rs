//! Where the Anthropic API key comes from: the `ANTHROPIC_API_KEY` environment variable, or the
//! macOS Keychain (saved from the Assistant window). Elsewhere a key typed into the window lasts
//! for the session only.

#[cfg(target_os = "macos")]
const SERVICE: &str = "SoundCraft Assistant";
#[cfg(target_os = "macos")]
const ACCOUNT: &str = "anthropic-api-key";
#[cfg(target_os = "macos")]
const SECURITY: &str = "/usr/bin/security";

/// Where a loaded key came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    Environment,
    Keychain,
    /// Typed into the window and kept for this run only.
    Session,
}

/// The stored key, if any.
pub fn load() -> Option<(String, KeySource)> {
    if let Some(k) = std::env::var("ANTHROPIC_API_KEY").ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty()) {
        return Some((k, KeySource::Environment));
    }
    keychain_load().map(|k| (k, KeySource::Keychain))
}

/// Whether keys can be remembered on this platform.
pub fn can_save() -> bool {
    cfg!(target_os = "macos")
}

#[cfg(target_os = "macos")]
fn keychain_load() -> Option<String> {
    let out = std::process::Command::new(SECURITY).args(["find-generic-password", "-s", SERVICE, "-a", ACCOUNT, "-w"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|k| !k.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn keychain_load() -> Option<String> {
    None
}

/// Remember `key` in the Keychain, replacing any earlier one.
#[cfg(target_os = "macos")]
pub fn save(key: &str) -> Result<(), String> {
    let out = std::process::Command::new(SECURITY)
        .args(["add-generic-password", "-U", "-s", SERVICE, "-a", ACCOUNT, "-w", key])
        .output()
        .map_err(|e| format!("could not run the Keychain tool: {e}"))?;
    if out.status.success() { Ok(()) } else { Err(format!("the Keychain refused the key: {}", String::from_utf8_lossy(&out.stderr).trim())) }
}

#[cfg(not(target_os = "macos"))]
pub fn save(_key: &str) -> Result<(), String> {
    Err("saving the key is only supported on macOS; set ANTHROPIC_API_KEY instead".into())
}

/// Remove a remembered key.
#[cfg(target_os = "macos")]
pub fn forget() {
    let _ = std::process::Command::new(SECURITY).args(["delete-generic-password", "-s", SERVICE, "-a", ACCOUNT]).output();
}

#[cfg(not(target_os = "macos"))]
pub fn forget() {}

/// A key shown on screen: its prefix and last four characters.
pub fn masked(key: &str) -> String {
    let n = key.chars().count();
    if n <= 12 {
        return "•".repeat(n.max(4));
    }
    let head: String = key.chars().take(7).collect();
    let tail: String = key.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn masks_keys() {
        assert_eq!(super::masked("sk-ant-api03-abcdefghijklmnop-WXYZ"), "sk-ant-…WXYZ");
        assert_eq!(super::masked("short"), "•••••");
    }
}
