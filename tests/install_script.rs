//! Integration: scripts/install.sh's OS/arch → target-triple resolution (roadmap 3.1).
//!
//! `--print-target` exists specifically so this logic is testable without a network call —
//! it resolves the target and exits, using NRG_TEST_UNAME_S/NRG_TEST_UNAME_M in place of a
//! real `uname` so every OS/arch branch is reachable regardless of what this test runs on.

use assert_cmd::Command;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

/// Own the bound listener for the entire test: no interpreter startup, IPv4/IPv6
/// default, or free-port handoff race. Drop stops and joins it even after an assertion fails.
struct ReleaseServer {
    url: String,
    stop: mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<std::io::Result<()>>>,
}

impl ReleaseServer {
    fn start(directory: PathBuf) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let (stop, stopped) = mpsc::channel();
        let worker = std::thread::spawn(move || loop {
            match stopped.recv_timeout(Duration::from_millis(10)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            };
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            let mut reader = std::io::BufReader::new(&stream);
            let mut request = String::new();
            reader.read_line(&mut request)?;
            let path = request.split_whitespace().nth(1).unwrap_or("");
            let name = path.strip_prefix('/').unwrap_or("");
            // This fixture only serves the archive and checksum, never arbitrary paths.
            let body = match name {
                "nrg-x86_64-unknown-linux-gnu.tar.gz"
                | "nrg-x86_64-unknown-linux-gnu.tar.gz.sha256" => {
                    std::fs::read(directory.join(name))?
                }
                _ => {
                    return Err(std::io::Error::other(format!(
                        "unexpected request: {request}"
                    )))
                }
            };
            let mut header = String::new();
            loop {
                header.clear();
                if reader.read_line(&mut header)? == 0 || header == "\r\n" {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )?;
            stream.write_all(&body)?;
        });
        Self {
            url,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for ReleaseServer {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        let result = self.worker.take().unwrap().join();
        if !std::thread::panicking() {
            result
                .expect("release server thread panicked")
                .expect("release server failed");
        }
    }
}

fn script_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh")
}

fn resolved_target(uname_s: &str, uname_m: &str) -> String {
    let out = Command::new("sh")
        .arg(script_path())
        .arg("--print-target")
        .env("NRG_TEST_UNAME_S", uname_s)
        .env("NRG_TEST_UNAME_M", uname_m)
        .assert()
        .success()
        .get_output()
        .clone();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn resolves_every_supported_os_arch_combination_to_the_right_target_triple() {
    assert_eq!(resolved_target("Darwin", "arm64"), "aarch64-apple-darwin");
    assert_eq!(resolved_target("Darwin", "x86_64"), "x86_64-apple-darwin");
    assert_eq!(
        resolved_target("Linux", "aarch64"),
        "aarch64-unknown-linux-gnu"
    );
    assert_eq!(
        resolved_target("Linux", "arm64"),
        "aarch64-unknown-linux-gnu"
    );
    assert_eq!(
        resolved_target("Linux", "x86_64"),
        "x86_64-unknown-linux-gnu"
    );
    assert_eq!(
        resolved_target("Linux", "amd64"),
        "x86_64-unknown-linux-gnu"
    );
}

#[test]
fn rejects_an_unsupported_os_before_any_network_access() {
    Command::new("sh")
        .arg(script_path())
        .arg("--print-target")
        .env("NRG_TEST_UNAME_S", "Windows_NT")
        .env("NRG_TEST_UNAME_M", "x86_64")
        .assert()
        .failure()
        .stderr(predicates::str::contains("unsupported OS"));
}

#[test]
fn rejects_an_unsupported_architecture_before_any_network_access() {
    Command::new("sh")
        .arg(script_path())
        .arg("--print-target")
        .env("NRG_TEST_UNAME_S", "Linux")
        .env("NRG_TEST_UNAME_M", "i686")
        .assert()
        .failure()
        .stderr(predicates::str::contains("unsupported architecture"));
}

#[test]
fn rejects_a_malformed_version_flag_before_any_network_access() {
    // A `--version` value flows straight into a download URL — this must be validated (and
    // fail) before the script ever calls uname/curl, not just produce a 404 later.
    Command::new("sh")
        .arg(script_path())
        .args(["--version", "not-a-version"])
        .arg("--print-target")
        .env("NRG_TEST_UNAME_S", "Linux")
        .env("NRG_TEST_UNAME_M", "x86_64")
        .assert()
        .failure()
        .stderr(predicates::str::contains("must look like"));
}

#[test]
fn rejects_a_version_flag_shaped_like_a_path_or_containing_whitespace() {
    // The `v[0-9]*` glob alone would accept `/`, spaces, and other characters that have no
    // business in a release tag (e.g. "v0/../../evil") — the value only ever lands in a
    // quoted URL, so this was never exploitable, but it should still be rejected as malformed
    // input rather than silently accepted and left to fail as a confusing 404 later.
    for bogus in ["v0/../../evil", "v1 2", "v1;rm -rf /"] {
        Command::new("sh")
            .arg(script_path())
            .args(["--version", bogus])
            .arg("--print-target")
            .env("NRG_TEST_UNAME_S", "Linux")
            .env("NRG_TEST_UNAME_M", "x86_64")
            .assert()
            .failure()
            .stderr(predicates::str::contains("must look like"));
    }
}

#[test]
fn accepts_latest_and_a_well_formed_version_tag() {
    Command::new("sh")
        .arg(script_path())
        .arg("--print-target")
        .env("NRG_TEST_UNAME_S", "Linux")
        .env("NRG_TEST_UNAME_M", "x86_64")
        .assert()
        .success();

    Command::new("sh")
        .arg(script_path())
        .args(["--version", "v1.2.3"])
        .arg("--print-target")
        .env("NRG_TEST_UNAME_S", "Linux")
        .env("NRG_TEST_UNAME_M", "x86_64")
        .assert()
        .success();
}

#[test]
fn rejects_an_unknown_flag() {
    Command::new("sh")
        .arg(script_path())
        .arg("--not-a-real-flag")
        .assert()
        .failure()
        .stderr(predicates::str::contains("unknown argument"));
}

#[test]
fn help_documents_every_flag() {
    Command::new("sh")
        .arg(script_path())
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("--version"))
        .stdout(predicates::str::contains("--bin-dir"))
        .stdout(predicates::str::contains("--print-target"));
}

#[test]
fn a_real_download_and_install_round_trip_works_and_a_tampered_archive_is_rejected() {
    // End-to-end: serve a fake "release" (a tarball whose sole entry is a script named
    // `nrg`, plus its real sha256) over a local HTTP server, point install.sh at it (via
    // NRG_TEST_BASE_URL, a test-only override), and confirm the full download → verify →
    // extract → install pipeline actually works — then corrupt the served archive and
    // confirm checksum verification refuses to install it.
    let dir = tempfile::tempdir().unwrap();
    let serve_dir = dir.path().join("serve");
    let bin_dir = dir.path().join("bindir");
    std::fs::create_dir_all(&serve_dir).unwrap();

    let fake_binary = "#!/bin/sh\necho fake-nrg-ran\n";
    std::fs::write(serve_dir.join("nrg"), fake_binary).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            serve_dir.join("nrg"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }

    let archive_name = "nrg-x86_64-unknown-linux-gnu.tar.gz";
    let tar_status = std::process::Command::new("tar")
        .args(["czf", archive_name, "nrg"])
        .current_dir(&serve_dir)
        .status()
        .unwrap();
    assert!(tar_status.success());

    let checksum_status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "shasum -a 256 {archive_name} > {archive_name}.sha256"
        ))
        .current_dir(&serve_dir)
        .status()
        .unwrap();
    assert!(checksum_status.success());

    let server = ReleaseServer::start(serve_dir.clone());

    let run_install = || {
        Command::new("sh")
            .arg(script_path())
            .args(["--bin-dir"])
            .arg(&bin_dir)
            .env("NRG_TEST_UNAME_S", "Linux")
            .env("NRG_TEST_UNAME_M", "x86_64")
            .env("NRG_TEST_BASE_URL", &server.url)
            .env("NO_PROXY", "127.0.0.1")
            .env("no_proxy", "127.0.0.1")
            .timeout(Duration::from_secs(30))
            .assert()
    };

    // First install: the real, untampered archive must install successfully.
    run_install().success();
    let installed = std::fs::read_to_string(bin_dir.join("nrg")).unwrap();
    assert_eq!(
        installed, fake_binary,
        "installed binary must match the served one exactly"
    );
    // Repeat installation over an existing executable must also work.
    run_install().success();
    Command::new(bin_dir.join("nrg"))
        .assert()
        .success()
        .stdout("fake-nrg-ran\n");

    // Tamper with the served archive so its bytes no longer match the checksum file.
    let archive_path = serve_dir.join(archive_name);
    let mut bytes = std::fs::read(&archive_path).unwrap();
    bytes.extend_from_slice(b"tampered");
    std::fs::write(&archive_path, bytes).unwrap();
    run_install()
        .failure()
        .stderr(predicates::str::contains("checksum verification failed"));
    assert_eq!(
        std::fs::read_to_string(bin_dir.join("nrg")).unwrap(),
        fake_binary,
        "a corrupt upgrade must preserve the existing executable"
    );
    std::fs::remove_file(bin_dir.join("nrg")).unwrap();

    run_install()
        .failure()
        .stderr(predicates::str::contains("checksum verification failed"));
    assert!(
        !bin_dir.join("nrg").exists(),
        "a tampered archive must never be installed"
    );
    assert_eq!(
        std::fs::read_dir(&bin_dir).unwrap().count(),
        0,
        "failed installation must leave no temporary destination files"
    );
}
