//! Integration: scripts/install.sh's OS/arch → target-triple resolution (roadmap 3.1).
//!
//! `--print-target` exists specifically so this logic is testable without a network call —
//! it resolves the target and exits, using NRG_TEST_UNAME_S/NRG_TEST_UNAME_M in place of a
//! real `uname` so every OS/arch branch is reachable regardless of what this test runs on.

use assert_cmd::Command;
use std::path::PathBuf;

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
    assert_eq!(resolved_target("Linux", "aarch64"), "aarch64-unknown-linux-gnu");
    assert_eq!(resolved_target("Linux", "arm64"), "aarch64-unknown-linux-gnu");
    assert_eq!(resolved_target("Linux", "x86_64"), "x86_64-unknown-linux-gnu");
    assert_eq!(resolved_target("Linux", "amd64"), "x86_64-unknown-linux-gnu");
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
        std::fs::set_permissions(serve_dir.join("nrg"), std::fs::Permissions::from_mode(0o755)).unwrap();
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
        .arg(format!("shasum -a 256 {archive_name} > {archive_name}.sha256"))
        .current_dir(&serve_dir)
        .status()
        .unwrap();
    assert!(checksum_status.success());

    // A free local port for the throwaway HTTP server.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let mut server = std::process::Command::new("python3")
        .args(["-m", "http.server", &port.to_string(), "--directory"])
        .arg(&serve_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("python3 must be on PATH for this test");
    // Give the server a moment to bind before hitting it.
    std::thread::sleep(std::time::Duration::from_millis(500));

    let base_url = format!("http://127.0.0.1:{port}");

    let run_install = || {
        Command::new("sh")
            .arg(script_path())
            .args(["--bin-dir"])
            .arg(&bin_dir)
            .env("NRG_TEST_UNAME_S", "Linux")
            .env("NRG_TEST_UNAME_M", "x86_64")
            .env("NRG_TEST_BASE_URL", &base_url)
            .assert()
    };

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // First install: the real, untampered archive must install successfully.
        run_install().success();
        let installed = std::fs::read_to_string(bin_dir.join("nrg")).unwrap();
        assert_eq!(installed, fake_binary, "installed binary must match the served one exactly");

        // Tamper with the served archive so its bytes no longer match the checksum file.
        let archive_path = serve_dir.join(archive_name);
        let mut bytes = std::fs::read(&archive_path).unwrap();
        bytes.extend_from_slice(b"tampered");
        std::fs::write(&archive_path, bytes).unwrap();
        std::fs::remove_file(bin_dir.join("nrg")).unwrap();

        run_install()
            .failure()
            .stderr(predicates::str::contains("checksum verification failed"));
        assert!(!bin_dir.join("nrg").exists(), "a tampered archive must never be installed");
    }));

    let _ = server.kill();
    let _ = server.wait();
    result.unwrap();
}
