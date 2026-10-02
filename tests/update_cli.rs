//! `powerqueue update` against a wiremock "GitHub": release JSON, tarball and
//! checksum are served from the same mock server; the binary being replaced
//! is a throwaway shell script (`--binary-path`), never the test binary.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::json;
use sha2::{Digest, Sha256};
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TARGET: &str = "x86_64-unknown-linux-gnu";
const TAG: &str = "v9.9.9";
const GOOD_SCRIPT: &str = "#!/bin/sh\necho powerqueue 9.9.9\n";
const ORIGINAL_SCRIPT: &str = "#!/bin/sh\necho powerqueue 0.0.1\n";

fn tarball(binary: &[u8]) -> Vec<u8> {
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut ar = tar::Builder::new(enc);
    for (name, data, mode) in [("powerqueue", binary, 0o755), ("README.md", b"docs".as_slice(), 0o644)] {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(mode);
        h.set_cksum();
        ar.append_data(&mut h, name, data).expect("append");
    }
    ar.into_inner().expect("tar").finish().expect("gzip")
}

fn sha_line(data: &[u8], name: &str) -> String {
    format!("{:x}  {name}\n", Sha256::digest(data))
}

/// Serve `/releases/latest` plus the two assets. `sha_override` replaces the
/// checksum file's content (to simulate corruption).
async fn serve(server: &MockServer, binary: &[u8], sha_override: Option<&str>) {
    let asset = format!("powerqueue-{TARGET}.tar.gz");
    let tb = tarball(binary);
    let sha = sha_override.map(str::to_string).unwrap_or_else(|| sha_line(&tb, &asset));
    let base = server.uri();
    Mock::given(method("GET"))
        .and(path("/releases/latest"))
        .and(header_exists("user-agent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "tag_name": TAG,
            "assets": [
                { "name": asset, "browser_download_url": format!("{base}/download/{TAG}/{asset}") },
                { "name": format!("{asset}.sha256"), "browser_download_url": format!("{base}/download/{TAG}/{asset}.sha256") },
            ]
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/download/{TAG}/{asset}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tb))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/download/{TAG}/{asset}.sha256")))
        .respond_with(ResponseTemplate::new(200).set_body_string(sha))
        .mount(server)
        .await;
}

struct Fixture {
    home: tempfile::TempDir,
    bin_dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("home");
        let bin_dir = tempfile::tempdir().expect("bin dir");
        let target = bin_dir.path().join("powerqueue");
        std::fs::write(&target, ORIGINAL_SCRIPT).expect("write dummy binary");
        std::fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o755)).expect("chmod");
        Self { home, bin_dir }
    }
    fn target(&self) -> PathBuf {
        self.bin_dir.path().join("powerqueue")
    }
    fn cmd(&self, server: &MockServer) -> Command {
        self.cmd_for(server, &self.target())
    }
    /// `update --binary-path <binary>` with the mock server as GitHub.
    fn cmd_for(&self, server: &MockServer, binary: &Path) -> Command {
        let mut c = Command::cargo_bin("powerqueue").expect("binary");
        c.env("POWERQUEUE_HOME", self.home.path())
            .env("POWERQUEUE_SECRETS", "file")
            .env("POWERQUEUE_UPDATE_API", server.uri())
            .env("POWERQUEUE_UPDATE_TARGET", TARGET)
            .env_remove("GITHUB_TOKEN")
            .env("NO_COLOR", "1")
            .arg("update")
            .arg("--binary-path")
            .arg(binary);
        c
    }
    fn dir_entries(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.bin_dir.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
}

fn run_script(path: &Path) -> String {
    let out = std::process::Command::new(path).arg("--version").output().expect("run script");
    assert!(out.status.success(), "script exited {}", out.status);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn check_reports_newer_release_with_exit_10() {
    let server = MockServer::start().await;
    serve(&server, GOOD_SCRIPT.as_bytes(), None).await;
    let fx = Fixture::new();

    let assert = fx.cmd(&server).arg("--check").assert().code(10);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(stdout.contains("9.9.9"), "stdout: {stdout}");
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "stdout: {stdout}");
    assert_eq!(std::fs::read_to_string(fx.target()).expect("read"), ORIGINAL_SCRIPT, "--check must not touch the file");

    let assert = fx.cmd(&server).arg("--check").arg("--json").assert().code(10);
    let v: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).expect("json");
    assert_eq!(v["latest"], "9.9.9");
    assert_eq!(v["current"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["updated"], false);
    assert_eq!(v["update_available"], true);
    assert_eq!(v["path"].as_str(), fx.target().canonicalize().expect("canonical").to_str());
}

#[tokio::test]
async fn update_yes_replaces_the_binary() {
    let server = MockServer::start().await;
    serve(&server, GOOD_SCRIPT.as_bytes(), None).await;
    let fx = Fixture::new();

    let assert = fx.cmd(&server).arg("--yes").arg("--json").assert().code(0);
    let v: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).expect("json");
    assert_eq!(v["updated"], true);
    assert_eq!(v["latest"], "9.9.9");
    assert_eq!(std::fs::read_to_string(fx.target()).expect("read"), GOOD_SCRIPT);
    assert_eq!(run_script(&fx.target()), "powerqueue 9.9.9");
    assert_eq!(fx.dir_entries(), vec!["powerqueue"], "no staging files left behind");

    // Plain-text run, through a symlink: the real file is replaced, not the link.
    let fx2 = Fixture::new();
    let link = fx2.home.path().join("pq-link");
    std::os::unix::fs::symlink(fx2.target(), &link).expect("symlink");
    fx2.cmd_for(&server, &link).arg("--yes").assert().code(0).stdout(predicates::str::contains("9.9.9"));
    assert!(std::fs::symlink_metadata(&link).expect("meta").file_type().is_symlink(), "symlink kept");
    assert_eq!(std::fs::read_to_string(fx2.target()).expect("read"), GOOD_SCRIPT);
}

#[tokio::test]
async fn bad_checksum_leaves_original_untouched() {
    let server = MockServer::start().await;
    let bogus = format!("{}  powerqueue-{TARGET}.tar.gz\n", "0".repeat(64));
    serve(&server, GOOD_SCRIPT.as_bytes(), Some(&bogus)).await;
    let fx = Fixture::new();
    let before = std::fs::read(fx.target()).expect("read");

    fx.cmd(&server).arg("--yes").assert().failure().stderr(predicates::str::contains("checksum mismatch"));
    assert_eq!(std::fs::read(fx.target()).expect("read"), before, "original must be byte-identical");
    assert_eq!(fx.dir_entries(), vec!["powerqueue"], "no staging files left behind");
}

#[tokio::test]
async fn failing_version_check_leaves_original_and_no_temp_files() {
    let server = MockServer::start().await;
    serve(&server, b"#!/bin/sh\necho broken >&2\nexit 7\n", None).await;
    let fx = Fixture::new();
    let before = std::fs::read(fx.target()).expect("read");

    fx.cmd(&server).arg("--yes").assert().failure().stderr(predicates::str::contains("--version"));
    assert_eq!(std::fs::read(fx.target()).expect("read"), before);
    assert_eq!(fx.dir_entries(), vec!["powerqueue"], "no staging files left behind");
    assert_eq!(run_script(&fx.target()), "powerqueue 0.0.1");
}

#[tokio::test]
async fn missing_asset_is_a_clear_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/releases/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "tag_name": TAG, "assets": [] })))
        .mount(&server)
        .await;
    let fx = Fixture::new();
    fx.cmd(&server).arg("--yes").assert().failure().stderr(predicates::str::contains("has no asset"));
    assert_eq!(std::fs::read_to_string(fx.target()).expect("read"), ORIGINAL_SCRIPT);
}

#[tokio::test]
async fn specific_tag_uses_the_tags_endpoint_and_same_version_needs_force() {
    let server = MockServer::start().await;
    let current = env!("CARGO_PKG_VERSION");
    Mock::given(method("GET"))
        .and(path(format!("/releases/tags/v{current}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "tag_name": format!("v{current}"), "assets": [] })))
        .mount(&server)
        .await;
    let fx = Fixture::new();
    // Bare version is normalised to the `v` tag; same version without --force is a no-op.
    fx.cmd(&server).args(["--version", current, "--yes"]).assert().code(0).stdout(predicates::str::contains("already"));
    // --force proceeds and then fails on the missing asset, proving it tried.
    fx.cmd(&server)
        .args(["--version", current, "--yes", "--force"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("has no asset"));
    assert_eq!(std::fs::read_to_string(fx.target()).expect("read"), ORIGINAL_SCRIPT);
}
