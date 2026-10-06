//! `powerqueue update` — replace this binary with a GitHub release.
//!
//! The running binary is never left corrupted or missing: the release
//! tarball and its `.sha256` are downloaded to memory, the checksum is
//! verified, the new `powerqueue` is written to a temporary file *next to*
//! the target (same filesystem), fsynced, executed with `--version` as a
//! sanity check, and only then renamed over the target. Any failure before
//! the rename removes the temporary file and leaves the old binary alone.
//!
//! Environment overrides (for tests and mirrors):
//! `POWERQUEUE_UPDATE_API` replaces the GitHub API base
//! (`https://api.github.com/repos/aleandros/powerqueue`),
//! `POWERQUEUE_UPDATE_TARGET` replaces the detected target triple, and
//! `GITHUB_TOKEN` is sent as a bearer token to the API (never to downloads,
//! never logged).

use std::cmp::Ordering;
use std::fmt;
use std::io::{IsTerminal, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use owo_colors::{OwoColorize, Stream};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::cli::commands::runtime;
use crate::cli::output::print_warning;
use crate::cli::{Context, UpdateArgs};

/// Exit code of `update --check` when a newer release exists.
pub const UPDATE_AVAILABLE_EXIT: i32 = 10;
/// GitHub API base for the project's releases.
pub const DEFAULT_API: &str = "https://api.github.com/repos/aleandros/powerqueue";
/// Name of the executable inside release tarballs.
const BINARY_NAME: &str = "powerqueue";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(60);

/// A `MAJOR.MINOR.PATCH[-pre]` version; build metadata (`+...`) is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<String>,
}

impl Version {
    /// Parse `1.2.3`, `v1.2.3`, `1.2.3-rc.1` or `1.2.3+build`.
    /// Fails when the core is not three dot-separated numbers.
    pub fn parse(s: &str) -> Result<Self> {
        let raw = s.trim();
        let body = raw.strip_prefix('v').or_else(|| raw.strip_prefix('V')).unwrap_or(raw);
        let body = body.split('+').next().unwrap_or("");
        let (core, pre) = match body.split_once('-') {
            Some((c, p)) => (c, p),
            None => (body, ""),
        };
        let mut nums = core.split('.');
        let mut next = |what: &str| -> Result<u64> {
            nums.next()
                .ok_or_else(|| anyhow!("version `{raw}` is missing its {what} number"))?
                .parse::<u64>()
                .map_err(|_| anyhow!("version `{raw}` has a non-numeric {what} number"))
        };
        let major = next("major")?;
        let minor = next("minor")?;
        let patch = next("patch")?;
        if nums.next().is_some() {
            bail!("version `{raw}` has more than three numbers");
        }
        if pre.is_empty() && body.contains('-') {
            bail!("version `{raw}` has an empty pre-release");
        }
        let pre = if pre.is_empty() { Vec::new() } else { pre.split('.').map(str::to_string).collect() };
        Ok(Self { major, minor, patch, pre })
    }

    fn core(&self) -> (u64, u64, u64) {
        (self.major, self.minor, self.patch)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core().cmp(&other.core()).then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            // A release is newer than any of its pre-releases.
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => compare_prerelease(&self.pre, &other.pre),
        })
    }
}

/// Semver 2.0 pre-release ordering: numeric identifiers compare as numbers
/// and sort before alphanumeric ones; a longer list wins when all shared
/// identifiers are equal.
fn compare_prerelease(a: &[String], b: &[String]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(n), Ok(m)) => n.cmp(&m),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// The release-asset target triple for this build, or the
/// `POWERQUEUE_UPDATE_TARGET` override. Fails when no prebuilt binary exists
/// for the platform.
pub fn detect_target() -> Result<String> {
    if let Some(t) = std::env::var_os("POWERQUEUE_UPDATE_TARGET")
        && !t.is_empty()
    {
        return Ok(t.to_string_lossy().into_owned());
    }
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => bail!("no prebuilt powerqueue binary for the {other} architecture; build from source with cargo"),
    };
    let os = match std::env::consts::OS {
        "linux" => "unknown-linux-gnu",
        "macos" => "apple-darwin",
        other => bail!("no prebuilt powerqueue binary for {other}; build from source with cargo"),
    };
    Ok(format!("{arch}-{os}"))
}

/// `POWERQUEUE_UPDATE_API` or the GitHub default, without a trailing slash.
fn api_base() -> String {
    std::env::var("POWERQUEUE_UPDATE_API")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_API.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// `v1.2.3` for `1.2.3` or `v1.2.3`.
fn normalise_tag(tag: &str) -> String {
    let t = tag.trim();
    if t.starts_with('v') { t.to_string() } else { format!("v{t}") }
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

impl Release {
    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
    fn asset_names(&self) -> String {
        if self.assets.is_empty() {
            "none".to_string()
        } else {
            self.assets.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
        }
    }
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("powerqueue/{}", crate::VERSION))
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(TOTAL_TIMEOUT)
        .build()
        .context("build HTTP client")
}

/// GET a release from the API: `/releases/latest` or `/releases/tags/<tag>`.
async fn fetch_release(client: &reqwest::Client, api: &str, tag: Option<&str>) -> Result<Release> {
    let url = match tag {
        Some(t) => format!("{api}/releases/tags/{t}"),
        None => format!("{api}/releases/latest"),
    };
    let mut req = client.get(&url).header("Accept", "application/vnd.github+json").header("X-GitHub-Api-Version", "2022-11-28");
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.trim().is_empty()
    {
        req = req.bearer_auth(token.trim());
    }
    let resp = req.send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(match tag {
            Some(t) => anyhow!("release {t} not found at {url}"),
            None => anyhow!("no published release found at {url}"),
        });
    }
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        bail!("GitHub API refused the request ({status}); you may be rate limited — set GITHUB_TOKEN and retry");
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("GET {url} failed with {status}: {}", body.trim());
    }
    resp.json::<Release>().await.with_context(|| format!("parse release JSON from {url}"))
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let resp = client.get(url).send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("GET {url} failed with {status}");
    }
    Ok(resp.bytes().await.with_context(|| format!("read body of {url}"))?.to_vec())
}

/// Check `data` against a `shasum -a 256` line (`<hex>  <name>`). Fails on
/// a malformed file or a mismatch, naming the asset.
pub fn verify_sha256(data: &[u8], sha_file: &str, asset_name: &str) -> Result<()> {
    let expected = sha_file
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| anyhow!("{asset_name}.sha256 is empty"))?
        .to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("{asset_name}.sha256 does not contain a SHA-256 hex digest (got `{expected}`)");
    }
    let actual = format!("{:x}", Sha256::digest(data));
    if actual != expected {
        bail!("checksum mismatch for {asset_name}: expected {expected}, downloaded file has {actual}; refusing to install");
    }
    Ok(())
}

/// Pull the `powerqueue` regular file out of a `.tar.gz`. Fails when the
/// archive is not gzip/tar or has no such entry.
pub fn extract_binary(tarball: &[u8]) -> Result<Vec<u8>> {
    let decoder = flate2::read::GzDecoder::new(tarball);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().context("read tarball")? {
        let mut entry = entry.context("read tarball entry")?;
        let path = entry.path().context("read tarball entry path")?.into_owned();
        let is_binary = path.file_name().is_some_and(|n| n == BINARY_NAME);
        if is_binary && entry.header().entry_type().is_file() {
            let mut out = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut out).with_context(|| format!("extract {} from tarball", path.display()))?;
            return Ok(out);
        }
    }
    bail!("the release tarball does not contain a `{BINARY_NAME}` file")
}

/// The staging file beside the target binary. Opened *before* the download
/// so an unwritable directory fails fast; removed on drop unless `commit`
/// renamed it over the target.
pub struct Staging {
    target: PathBuf,
    path: PathBuf,
    file: Option<std::fs::File>,
    armed: bool,
}

impl Staging {
    /// Create `.powerqueue.update-<pid>` next to `target` (mode 0755 on
    /// unix). A permission error becomes advice to use `sudo` or another
    /// install directory.
    pub fn open(target: &Path) -> Result<Self> {
        let dir = target.parent().ok_or_else(|| anyhow!("{} has no parent directory", target.display()))?;
        let path = dir.join(format!(".{BINARY_NAME}.update-{}", std::process::id()));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o755);
        }
        match opts.open(&path) {
            Ok(file) => Ok(Self { target: target.to_path_buf(), path, file: Some(file), armed: true }),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(anyhow!(
                "cannot write to {}: {e}. Re-run as `sudo powerqueue update`, or install to a directory you own \
                 (`POWERQUEUE_INSTALL_DIR=~/.local/bin` with install.sh)",
                dir.display()
            )),
            Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
        }
    }

    /// Write `bytes`, make the file executable, fsync, check `--version`,
    /// then rename it over the target. On any failure the staging file is
    /// removed and the target is untouched.
    pub fn commit(mut self, bytes: &[u8], expected_version: &str) -> Result<()> {
        let mut file = self.file.take().ok_or_else(|| anyhow!("staging file already committed"))?;
        file.write_all(bytes).with_context(|| format!("write {}", self.path.display()))?;
        file.sync_all().with_context(|| format!("fsync {}", self.path.display()))?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o755))
                .with_context(|| format!("chmod 755 {}", self.path.display()))?;
        }
        sanity_check(&self.path, expected_version)?;
        std::fs::rename(&self.path, &self.target)
            .with_context(|| format!("replace {} with {}", self.target.display(), self.path.display()))?;
        self.armed = false;
        clear_quarantine(&self.target);
        Ok(())
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.armed {
            drop(self.file.take());
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Run `<path> --version` and require exit 0 plus `expected` in its output.
fn sanity_check(path: &Path, expected: &str) -> Result<()> {
    let out = run_fresh_executable(|| Process::new(path).arg("--version").env_remove("POWERQUEUE_HOME").output())
        .with_context(|| format!("run {} --version", path.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        bail!(
            "the downloaded binary failed `--version` ({}): {}",
            out.status,
            if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() }
        );
    }
    if !stdout.contains(expected) {
        bail!("the downloaded binary reports `{}` instead of version {expected}", stdout.trim());
    }
    Ok(())
}

/// Run a program written moments ago, retrying briefly while it is "busy".
/// A process forked by another thread at the moment the file was still open
/// for writing keeps that descriptor until it execs, and running the file in
/// that window fails with `ETXTBSY`; it clears within milliseconds.
fn run_fresh_executable(mut run: impl FnMut() -> std::io::Result<std::process::Output>) -> std::io::Result<std::process::Output> {
    const ATTEMPTS: u64 = 10;
    let mut attempt = 1;
    loop {
        match run() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < ATTEMPTS => {
                std::thread::sleep(std::time::Duration::from_millis(20 * attempt));
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// Best effort: drop macOS' quarantine attribute so Gatekeeper never blocks
/// the new file. Failures are ignored.
fn clear_quarantine(target: &Path) {
    if cfg!(target_os = "macos") && which::which("xattr").is_ok() {
        let _ = Process::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// [`Staging::open`] followed by [`Staging::commit`]: replace `target`
/// with `bytes` atomically, or leave it untouched.
pub fn install(target: &Path, bytes: &[u8], expected_version: &str) -> Result<()> {
    Staging::open(target)?.commit(bytes, expected_version)
}

/// The file to replace: `--binary-path` or the running executable, with
/// symlinks resolved so the real file is swapped.
fn resolve_target(override_path: Option<&Path>) -> Result<PathBuf> {
    let raw = match override_path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_exe().context("locate the running executable")?,
    };
    raw.canonicalize().with_context(|| format!("resolve {}", raw.display()))
}

fn under_cargo_bin(path: &Path) -> bool {
    let mut candidates = Vec::new();
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        candidates.push(PathBuf::from(home).join("bin"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".cargo").join("bin"));
    }
    candidates.iter().any(|c| c.canonicalize().map(|c| path.starts_with(c)).unwrap_or(false))
}

/// Whether a daemon is heartbeating, when a database exists. Never fails:
/// any problem reads as "unknown" (`None`).
fn daemon_running(ctx: &mut Context) -> Option<bool> {
    if !ctx.paths.database().exists() {
        return Some(false);
    }
    let tick_secs = ctx.config_or_default().ok()?.scheduler.tick_secs.max(1) as i64;
    let store = ctx.store().ok()?;
    store.daemon_alive(chrono::Duration::seconds(3 * tick_secs)).ok()
}

fn bold(s: &str) -> String {
    s.if_supports_color(Stream::Stdout, |t| t.bold()).to_string()
}

/// Handle `powerqueue update`.
pub fn run(ctx: &mut Context, args: UpdateArgs) -> Result<i32> {
    let current = Version::parse(crate::VERSION).context("parse the compiled-in version")?;
    let target_triple = detect_target()?;
    let api = api_base();
    let requested_tag = args.version.as_deref().map(normalise_tag);

    let client = http_client()?;
    let rt = runtime()?;
    let release = rt.block_on(fetch_release(&client, &api, requested_tag.as_deref()))?;
    let latest = Version::parse(&release.tag_name).with_context(|| format!("parse release tag `{}`", release.tag_name))?;
    let path = resolve_target(args.binary_path.as_deref())?;

    let newer = latest > current;
    let json = |updated: bool| {
        serde_json::json!({
            "current": current.to_string(),
            "latest": latest.to_string(),
            "tag": release.tag_name,
            "target": target_triple,
            "update_available": newer,
            "updated": updated,
            "path": path,
        })
    };

    if args.check {
        if ctx.json {
            println!("{}", serde_json::to_string_pretty(&json(false))?);
        } else if newer {
            println!("update available: {} -> {} (run `powerqueue update`)", current, bold(&latest.to_string()));
        } else {
            println!("powerqueue {current} is up to date (latest release is {latest})");
        }
        return Ok(if newer { UPDATE_AVAILABLE_EXIT } else { 0 });
    }

    let same = latest == current;
    if same && !args.force {
        if ctx.json {
            println!("{}", serde_json::to_string_pretty(&json(false))?);
        } else {
            println!(
                "powerqueue {current} is already the {} release; use --force to reinstall",
                match requested_tag {
                    Some(_) => "requested",
                    None => "latest",
                }
            );
        }
        return Ok(0);
    }
    if latest < current && requested_tag.is_none() && !args.force {
        if ctx.json {
            println!("{}", serde_json::to_string_pretty(&json(false))?);
        } else {
            println!(
                "powerqueue {current} is newer than the latest release {latest}; nothing to do (use --force to install it anyway)"
            );
        }
        return Ok(0);
    }

    if under_cargo_bin(&path) {
        print_warning(&format!(
            "{} was installed with cargo; `cargo install --git https://github.com/aleandros/powerqueue --locked` \
             is the alternative. Proceeding with the release binary anyway.",
            path.display()
        ));
    }

    let (verb, done) = match latest.cmp(&current) {
        Ordering::Greater => ("update", "updated"),
        Ordering::Equal => ("reinstall", "reinstalled"),
        Ordering::Less => ("downgrade", "downgraded"),
    };
    if !args.yes && std::io::stdin().is_terminal() {
        if ctx.json {
            bail!("--json needs --yes (or a non-interactive stdin) because it cannot prompt");
        }
        let prompt = format!("{verb} powerqueue {current} -> {latest} at {}?", path.display());
        let ok = dialoguer::Confirm::new().with_prompt(prompt).default(true).interact().context("read confirmation")?;
        if !ok {
            eprintln!("aborted; nothing was changed");
            return Ok(1);
        }
    }

    let asset_name = format!("{BINARY_NAME}-{target_triple}.tar.gz");
    let sha_name = format!("{asset_name}.sha256");
    let asset = release
        .asset(&asset_name)
        .ok_or_else(|| anyhow!("release {} has no asset `{asset_name}` (assets: {})", release.tag_name, release.asset_names()))?;
    let sha_asset = release.asset(&sha_name).ok_or_else(|| {
        anyhow!("release {} has no checksum asset `{sha_name}` (assets: {})", release.tag_name, release.asset_names())
    })?;

    // Fail before downloading anything when the directory is not writable.
    let staging = Staging::open(&path)?;
    if !ctx.json {
        eprintln!("downloading {asset_name} ({})...", release.tag_name);
    }
    let tarball = rt.block_on(download(&client, &asset.browser_download_url))?;
    let sha_text = rt.block_on(download(&client, &sha_asset.browser_download_url))?;
    verify_sha256(&tarball, &String::from_utf8_lossy(&sha_text), &asset_name)?;
    let binary = extract_binary(&tarball)?;
    tracing::info!(tag = %release.tag_name, bytes = binary.len(), path = %path.display(), "verified release binary");

    staging.commit(&binary, &latest.to_string())?;

    let daemon = daemon_running(ctx);
    if ctx.json {
        let mut out = json(true);
        out["daemon_running"] = serde_json::json!(daemon);
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("{done} powerqueue {current} -> {} at {}", bold(&latest.to_string()), path.display());
        if daemon == Some(true) {
            println!("the daemon is running and keeps the old version until restarted: `powerqueue stop`, then `powerqueue run`");
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn parses_versions_with_prefix_pre_and_build() {
        assert_eq!(v("v1.2.3").to_string(), "1.2.3");
        assert_eq!(v("1.2.3-rc.1+abc").to_string(), "1.2.3-rc.1");
        assert_eq!(v("0.2.0").core(), (0, 2, 0));
        assert!(Version::parse("1.2").is_err());
        assert!(Version::parse("1.2.x").is_err());
        assert!(Version::parse("1.2.3.4").is_err());
        assert!(Version::parse("1.2.3-").is_err());
    }

    #[test]
    fn orders_semantically() {
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("1.0.0-rc.1"));
        assert!(v("1.0.0-rc.2") > v("1.0.0-rc.1"));
        assert!(v("1.0.0-rc.10") > v("1.0.0-rc.9"));
        assert!(v("1.0.0-beta") > v("1.0.0-alpha"));
        assert!(v("1.0.0-alpha.1") > v("1.0.0-alpha"));
        assert!(v("1.0.0-1") < v("1.0.0-alpha"));
        assert_eq!(v("v2.0.0"), v("2.0.0+build"));
    }

    #[test]
    fn tag_normalisation() {
        assert_eq!(normalise_tag("0.3.0"), "v0.3.0");
        assert_eq!(normalise_tag(" v0.3.0 "), "v0.3.0");
    }

    #[test]
    fn sha256_verification() {
        let data = b"hello";
        let hex = format!("{:x}", Sha256::digest(data));
        verify_sha256(data, &format!("{hex}  powerqueue-x.tar.gz\n"), "powerqueue-x.tar.gz").unwrap();
        verify_sha256(data, &hex.to_uppercase(), "a").unwrap();
        assert!(verify_sha256(b"hellO", &hex, "a").unwrap_err().to_string().contains("checksum mismatch"));
        assert!(verify_sha256(data, "", "a").is_err());
        assert!(verify_sha256(data, "deadbeef  a", "a").is_err());
    }

    fn tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(enc);
        for (name, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            ar.append_data(&mut h, name, *data).unwrap();
        }
        ar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn extracts_the_binary_entry_only() {
        let tb = tarball(&[("README.md", b"docs"), ("./powerqueue", b"#!/bin/sh\necho hi\n")]);
        assert_eq!(extract_binary(&tb).unwrap(), b"#!/bin/sh\necho hi\n");
        let tb = tarball(&[("README.md", b"docs")]);
        assert!(extract_binary(&tb).unwrap_err().to_string().contains("does not contain"));
        assert!(extract_binary(b"not a tarball").is_err());
    }

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn fresh_executables_are_retried_while_busy() {
        use std::os::unix::process::ExitStatusExt;
        let busy = || std::io::Error::from(std::io::ErrorKind::ExecutableFileBusy);
        let ok =
            || std::process::Output { status: std::process::ExitStatus::from_raw(0), stdout: Vec::new(), stderr: Vec::new() };
        let mut calls = 0;
        let out = run_fresh_executable(|| {
            calls += 1;
            if calls < 3 { Err(busy()) } else { Ok(ok()) }
        });
        assert!(out.is_ok() && calls == 3);
        let mut calls = 0;
        let err = run_fresh_executable(|| {
            calls += 1;
            Err(busy())
        });
        assert_eq!((err.unwrap_err().kind(), calls), (std::io::ErrorKind::ExecutableFileBusy, 10), "gives up after 10 tries");
        let mut calls = 0;
        let err = run_fresh_executable(|| {
            calls += 1;
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        });
        assert_eq!((err.unwrap_err().kind(), calls), (std::io::ErrorKind::NotFound, 1), "other errors are not retried");
    }

    #[test]
    fn install_replaces_atomically_and_cleans_up_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("powerqueue");
        std::fs::write(&target, "#!/bin/sh\necho powerqueue 0.0.1\n").unwrap();
        let good = b"#!/bin/sh\necho powerqueue 9.9.9\n";
        install(&target, good, "9.9.9").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), good);

        let bad = b"#!/bin/sh\nexit 3\n";
        let err = install(&target, bad, "9.9.9").unwrap_err().to_string();
        assert!(err.contains("failed `--version`"), "{err}");
        assert_eq!(std::fs::read(&target).unwrap(), good);

        let wrong = b"#!/bin/sh\necho powerqueue 1.0.0\n";
        assert!(install(&target, wrong, "9.9.9").is_err());
        assert_eq!(std::fs::read(&target).unwrap(), good);

        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("powerqueue")]);
    }
}
