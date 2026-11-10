//! Issue #28 — the StateStore read-modify-write (reload -> mutate -> flush) used to run with no
//! mutual exclusion, and a re-entrant nested `nrg` takes no run lock (its ancestor holds it),
//! so concurrent nested writers could each rename a full map over the other's keys.

use assert_cmd::cargo::cargo_bin;
use assert_cmd::Command;
use std::fs;
use std::time::Duration;

const WRITERS: usize = 4;
const KEYS: usize = 150;

#[test]
fn concurrent_reentrant_writers_do_not_lose_keys() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".energize")).unwrap();
    let bin = cargo_bin("nrg");

    for w in 0..WRITERS {
        fs::write(
            dir.path().join(format!("w{w}.rhai")),
            format!(r#"for i in 0..{KEYS} {{ state_set("w{w}k" + i, "x"); }}"#),
        )
        .unwrap();
    }
    // The parent holds the run lock, then backgrounds several nested nrg writers that all
    // inherit NRG_STATE_LOCK and so skip the run lock.
    let shell = (0..WRITERS)
        .map(|w| format!("{} exec w{w}.rhai &", bin.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
        + " wait";
    fs::write(
        dir.path().join("outer.rhai"),
        format!(
            r#"state_set("outer", "ok");
               let r = local_exec({shell:?});
               if !r.ok {{ throw "nested nrg failed: " + r.stderr; }}"#
        ),
    )
    .unwrap();

    Command::new(&bin)
        .current_dir(dir.path())
        .args(["exec", "outer.rhai"])
        .timeout(Duration::from_secs(120)) // a deadlock against the parent's run lock hits this
        .assert()
        .success();

    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join(".energize/state.json")).unwrap())
            .unwrap();
    let data = state["data"].as_object().unwrap();
    assert!(data.contains_key("outer"));
    let found = data.keys().filter(|k| k.starts_with('w')).count();
    assert_eq!(
        found,
        WRITERS * KEYS,
        "lost updates: {found} of {}",
        WRITERS * KEYS
    );
}
