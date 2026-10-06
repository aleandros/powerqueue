//! Helpers shared by unit tests.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Write an executable shell script (`#!/bin/sh` + `body`) at `path`.
///
/// The file is written and `chmod`ed by a short-lived `sh` child, never
/// through a descriptor of the test process: a writable descriptor here can
/// be inherited by a process another test thread forks at that moment, and
/// executing the script while that child still holds it fails with
/// `ETXTBSY` ("Text file busy").
pub fn write_executable(path: &Path, body: &str) {
    let mut child = Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn sh to write the script");
    child.stdin.take().expect("stdin").write_all(format!("#!/bin/sh\n{body}\n").as_bytes()).expect("write script");
    let status = child.wait().expect("wait for sh");
    assert!(status.success(), "could not write {}", path.display());
}
