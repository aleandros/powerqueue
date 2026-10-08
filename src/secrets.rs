//! Secret storage.
//!
//! Keys are stored in the OS credential store (macOS Keychain, Windows
//! Credential Manager, Secret Service on Linux) under the `powerqueue`
//! service. When no store is available (headless CI, some containers) we fall
//! back to a `0600` TOML file in the config directory and say so loudly.
//! Environment variables always win, which keeps automation simple.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::APP_NAME;
use crate::config::write_private;
use crate::paths::Paths;

/// Secrets powerqueue knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SecretKind {
    LinearApiKey,
    JevApiKey,
    GitHubToken,
}

impl SecretKind {
    pub const ALL: [SecretKind; 3] = [SecretKind::LinearApiKey, SecretKind::JevApiKey, SecretKind::GitHubToken];

    /// Keychain account name / TOML key.
    pub fn name(&self) -> &'static str {
        match self {
            SecretKind::LinearApiKey => "linear_api_key",
            SecretKind::JevApiKey => "jev_api_key",
            SecretKind::GitHubToken => "github_token",
        }
    }

    /// Environment variable override.
    pub fn env_var(&self) -> &'static str {
        match self {
            SecretKind::LinearApiKey => "LINEAR_API_KEY",
            SecretKind::JevApiKey => "JEV_API_KEY",
            SecretKind::GitHubToken => "GITHUB_TOKEN",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            SecretKind::LinearApiKey => "Linear API key",
            SecretKind::JevApiKey => "Jev (TypeSafe) API key",
            SecretKind::GitHubToken => "GitHub token",
        }
    }
}

impl fmt::Display for SecretKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Where a secret value came from, for diagnostics (never includes the value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecretOrigin {
    Environment,
    Keychain,
    File,
}

impl fmt::Display for SecretOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SecretOrigin::Environment => "environment",
            SecretOrigin::Keychain => "keychain",
            SecretOrigin::File => "file",
        })
    }
}

/// Backend abstraction so tests never touch the real keychain.
pub trait SecretBackend: Send + Sync {
    fn get(&self, kind: SecretKind) -> Result<Option<String>>;
    fn set(&self, kind: SecretKind, value: &str) -> Result<()>;
    fn delete(&self, kind: SecretKind) -> Result<()>;
    fn origin(&self) -> SecretOrigin;
    /// Human description shown by `doctor` / `init`.
    fn describe(&self) -> String;
}

/// OS keychain backend.
pub struct KeychainBackend {
    service: String,
}

impl KeychainBackend {
    pub fn new() -> Self {
        Self { service: APP_NAME.to_string() }
    }

    /// Probe whether the platform store is usable at all.
    pub fn available() -> bool {
        match keyring::Entry::new(APP_NAME, "__probe__") {
            Ok(entry) => match entry.get_password() {
                Ok(_) => true,
                Err(keyring::Error::NoEntry) => true,
                Err(_) => false,
            },
            Err(_) => false,
        }
    }
}

impl Default for KeychainBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretBackend for KeychainBackend {
    fn get(&self, kind: SecretKind) -> Result<Option<String>> {
        let entry = keyring::Entry::new(&self.service, kind.name())?;
        match entry.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow!("keychain read for {kind}: {e}")),
        }
    }
    fn set(&self, kind: SecretKind, value: &str) -> Result<()> {
        let entry = keyring::Entry::new(&self.service, kind.name())?;
        entry.set_password(value).map_err(|e| anyhow!("keychain write for {kind}: {e}"))
    }
    fn delete(&self, kind: SecretKind) -> Result<()> {
        let entry = keyring::Entry::new(&self.service, kind.name())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow!("keychain delete for {kind}: {e}")),
        }
    }
    fn origin(&self) -> SecretOrigin {
        SecretOrigin::Keychain
    }
    fn describe(&self) -> String {
        if cfg!(target_os = "macos") {
            "macOS Keychain".to_string()
        } else if cfg!(target_os = "windows") {
            "Windows Credential Manager".to_string()
        } else {
            "Secret Service (keyring)".to_string()
        }
    }
}

/// Owner-only TOML file backend.
pub struct FileBackend {
    path: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SecretsFile {
    #[serde(flatten)]
    values: BTreeMap<String, String>,
}

impl FileBackend {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
    fn read(&self) -> Result<SecretsFile> {
        if !self.path.exists() {
            return Ok(SecretsFile::default());
        }
        let text = std::fs::read_to_string(&self.path).with_context(|| format!("read {}", self.path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse {}", self.path.display()))
    }
    fn write(&self, file: &SecretsFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string(file)?;
        write_private(&self.path, text.as_bytes())?;
        Ok(())
    }
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

impl SecretBackend for FileBackend {
    fn get(&self, kind: SecretKind) -> Result<Option<String>> {
        Ok(self.read()?.values.get(kind.name()).cloned())
    }
    fn set(&self, kind: SecretKind, value: &str) -> Result<()> {
        let mut f = self.read()?;
        f.values.insert(kind.name().to_string(), value.to_string());
        self.write(&f)
    }
    fn delete(&self, kind: SecretKind) -> Result<()> {
        let mut f = self.read()?;
        f.values.remove(kind.name());
        self.write(&f)
    }
    fn origin(&self) -> SecretOrigin {
        SecretOrigin::File
    }
    fn describe(&self) -> String {
        format!("file {} (0600)", self.path.display())
    }
}

/// Facade: environment → backend.
pub struct Secrets {
    backend: Box<dyn SecretBackend>,
}

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secrets({})", self.backend.describe())
    }
}

impl Secrets {
    /// Choose the best backend for this machine. `POWERQUEUE_SECRETS=file`
    /// forces the file backend (useful in CI and tests).
    pub fn auto(paths: &Paths) -> Self {
        let forced_file = std::env::var("POWERQUEUE_SECRETS").map(|v| v == "file").unwrap_or(false);
        if !forced_file && KeychainBackend::available() {
            Self { backend: Box::new(KeychainBackend::new()) }
        } else {
            Self { backend: Box::new(FileBackend::new(paths.secrets_file())) }
        }
    }

    pub fn with_backend(backend: Box<dyn SecretBackend>) -> Self {
        Self { backend }
    }

    pub fn backend_description(&self) -> String {
        self.backend.describe()
    }

    /// Value and where it came from. Environment overrides storage.
    pub fn get_with_origin(&self, kind: SecretKind) -> Result<Option<(String, SecretOrigin)>> {
        if let Ok(v) = std::env::var(kind.env_var())
            && !v.trim().is_empty()
        {
            return Ok(Some((v, SecretOrigin::Environment)));
        }
        Ok(self.backend.get(kind)?.map(|v| (v, self.backend.origin())))
    }

    pub fn get(&self, kind: SecretKind) -> Result<Option<String>> {
        Ok(self.get_with_origin(kind)?.map(|(v, _)| v))
    }

    /// Like `get` but an error with setup instructions when missing.
    pub fn require(&self, kind: SecretKind) -> Result<String> {
        self.get(kind)?
            .ok_or_else(|| anyhow!("{} not configured. Run `powerqueue init` or set {}.", kind.label(), kind.env_var()))
    }

    pub fn set(&self, kind: SecretKind, value: &str) -> Result<()> {
        let value = value.trim();
        if value.is_empty() {
            return Err(anyhow!("refusing to store an empty {}", kind.label()));
        }
        self.backend.set(kind, value)
    }

    pub fn delete(&self, kind: SecretKind) -> Result<()> {
        self.backend.delete(kind)
    }
}

/// Mask a secret for display: first 4 and last 2 chars.
pub fn mask(value: &str) -> String {
    let n = value.chars().count();
    if n <= 8 {
        return "*".repeat(n);
    }
    let head: String = value.chars().take(4).collect();
    let tail: String = value.chars().skip(n - 2).collect();
    format!("{head}{}{tail}", "*".repeat(n - 6))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_backend_round_trip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let backend = FileBackend::new(dir.path().join("secrets.toml"));
        assert_eq!(backend.get(SecretKind::LinearApiKey).unwrap(), None);
        backend.set(SecretKind::LinearApiKey, "lin_abc").unwrap();
        backend.set(SecretKind::JevApiKey, "jev_xyz").unwrap();
        assert_eq!(backend.get(SecretKind::LinearApiKey).unwrap().as_deref(), Some("lin_abc"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(backend.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        backend.delete(SecretKind::LinearApiKey).unwrap();
        assert_eq!(backend.get(SecretKind::LinearApiKey).unwrap(), None);
        assert_eq!(backend.get(SecretKind::JevApiKey).unwrap().as_deref(), Some("jev_xyz"));
    }

    #[test]
    fn env_overrides_backend() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Secrets::with_backend(Box::new(FileBackend::new(dir.path().join("s.toml"))));
        secrets.set(SecretKind::JevApiKey, "stored").unwrap();
        // SAFETY: tests in this module run single-threaded per process env semantics; we restore after.
        unsafe { std::env::set_var("JEV_API_KEY", "from-env") };
        let (v, origin) = secrets.get_with_origin(SecretKind::JevApiKey).unwrap().unwrap();
        unsafe { std::env::remove_var("JEV_API_KEY") };
        assert_eq!(v, "from-env");
        assert_eq!(origin, SecretOrigin::Environment);
        let (v, origin) = secrets.get_with_origin(SecretKind::JevApiKey).unwrap().unwrap();
        assert_eq!(v, "stored");
        assert_eq!(origin, SecretOrigin::File);
    }

    #[test]
    fn empty_secret_rejected_and_masking() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = Secrets::with_backend(Box::new(FileBackend::new(dir.path().join("s.toml"))));
        assert!(secrets.set(SecretKind::LinearApiKey, "   ").is_err());
        assert_eq!(mask("lin_api_1234567890"), "lin_************90");
        assert_eq!(mask("short"), "*****");
    }
}
