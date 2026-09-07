//! API-key resolution and storage. Precedence: explicit flag >
//! CELIS_API_KEY environment variable > OS keyring (saved via `ai login`).
//! Keys are NEVER written to project folders, logs, or the ledger.

use bt_core::error::{CoreError, CoreResult};

pub const KEYRING_SERVICE: &str = "stratz";
pub const KEYRING_ACCOUNT: &str = "api-key";
pub const ENV_VAR: &str = "STRATZ_API_KEY";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    Flag,
    Env,
    Keyring,
    None,
}

impl KeySource {
    pub fn label(self) -> &'static str {
        match self {
            KeySource::Flag => "command-line flag (session only)",
            KeySource::Env => "environment variable CELIS_API_KEY",
            KeySource::Keyring => "OS keyring",
            KeySource::None => "not configured",
        }
    }
}

/// Pure resolution logic (testable): flag > env > keyring.
pub fn resolve_from_parts(
    flag: Option<&str>,
    env_value: Option<&str>,
    keyring_value: Option<String>,
) -> (Option<String>, KeySource) {
    if let Some(k) = flag.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        return (Some(k.to_string()), KeySource::Flag);
    }
    if let Some(k) = env_value.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        return (Some(k.to_string()), KeySource::Env);
    }
    if let Some(k) = keyring_value.filter(|s| !s.trim().is_empty()) {
        return (Some(k), KeySource::Keyring);
    }
    (None, KeySource::None)
}

/// Resolve with the real environment + keyring.
pub fn resolve_api_key(flag: Option<&str>) -> (Option<String>, KeySource) {
    let env_value = std::env::var(ENV_VAR).ok();
    let keyring_value = read_keyring();
    resolve_from_parts(flag, env_value.as_deref(), keyring_value)
}

pub fn key_source_label(flag: Option<&str>) -> &'static str {
    resolve_api_key(flag).1.label()
}

/// Save a key into the OS keyring.
pub fn store_key(api_key: &str) -> CoreResult<()> {
    let entry = keyring_entry()?;
    entry
        .set_password(api_key)
        .map_err(|e| CoreError::InvalidData(format!("keyring write failed: {e}")))
}

/// Remove the keyring entry (`ai login --clear`).
pub fn clear_key() -> CoreResult<()> {
    let entry = keyring_entry()?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(CoreError::InvalidData(format!(
            "keyring delete failed: {e}"
        ))),
    }
}

fn keyring_entry() -> CoreResult<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .map_err(|e| CoreError::InvalidData(format!(
            "keyring unavailable on this platform ({e}); use --api-key or the {ENV_VAR} environment variable"
        )))
}

fn read_keyring() -> Option<String> {
    // NoEntry is the normal "nothing saved" case; anything else also
    // degrades to None with the CLI falling back to flag/env.
    let entry = keyring_entry().ok()?;
    entry.get_password().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_flag_env_keyring() {
        let (k, src) =
            resolve_from_parts(Some("flag-key"), Some("env-key"), Some("ring-key".into()));
        assert_eq!(k.as_deref(), Some("flag-key"));
        assert_eq!(src, KeySource::Flag);

        let (k, src) = resolve_from_parts(None, Some("env-key"), Some("ring-key".into()));
        assert_eq!(k.as_deref(), Some("env-key"));
        assert_eq!(src, KeySource::Env);

        let (k, src) = resolve_from_parts(None, None, Some("ring-key".into()));
        assert_eq!(k.as_deref(), Some("ring-key"));
        assert_eq!(src, KeySource::Keyring);

        let (k, src) = resolve_from_parts(None, None, None);
        assert!(k.is_none());
        assert_eq!(src, KeySource::None);
    }

    #[test]
    fn empty_strings_do_not_count() {
        let (k, src) = resolve_from_parts(Some("  "), Some(""), Some("  ".into()));
        assert!(k.is_none());
        assert_eq!(src, KeySource::None);
    }
}
