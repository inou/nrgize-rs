//! Integration: a failed transaction runs its compensations (real local_exec), then re-raises.

use assert_cmd::Command;
use std::fs;

#[test]
fn failed_transaction_unwinds_via_local_exec() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    let marker = dir.path().join("rolled-back");
    fs::write(
        dir.path().join("Energize.rhai"),
        format!(
            r#"
        transaction(|| {{
            on_rollback(|| {{ local_exec("touch {m}"); }});
            local_exec("true");          // a real forward step
            throw "deploy failed on host 3";
        }});
        "#,
            m = marker.display()
        ),
    )
    .unwrap();

    Command::cargo_bin("nrg")
        .unwrap()
        .current_dir(dir.path())
        .arg("exec")
        .arg("Energize.rhai")
        .assert()
        .failure() // the throw re-raises after unwinding
        .stderr(predicates::str::contains("deploy failed on host 3"));

    // The compensation ran: the rollback marker file exists.
    assert!(marker.exists(), "rollback compensation should have executed");
}
