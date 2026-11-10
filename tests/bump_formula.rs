//! Integration: scripts/bump-formula.py rewrites a COPY of Formula/nrg.rb (never the real one).

use std::path::{Path, PathBuf};
use std::process::Command;

const OLD_SUMS: [(&str, &str); 4] = [
    ("aarch64-apple-darwin", "1"),
    ("x86_64-apple-darwin", "2"),
    ("aarch64-unknown-linux-gnu", "3"),
    ("x86_64-unknown-linux-gnu", "4"),
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn hash(c: &str) -> String {
    c.repeat(64)
}

fn run(version: &str, sums: &Path, formula: &Path) -> std::process::Output {
    Command::new("python3")
        .arg(root().join("scripts/bump-formula.py"))
        .args([version])
        .arg(sums)
        .arg(formula)
        .output()
        .unwrap()
}

fn setup(dir: &Path, skip: Option<&str>) -> (PathBuf, PathBuf) {
    let formula = dir.join("nrg.rb");
    std::fs::copy(root().join("Formula/nrg.rb"), &formula).unwrap();
    let mut text = String::new();
    // Extra musl entries must be ignored; order must not matter.
    text.push_str(&format!(
        "{}  nrg-x86_64-unknown-linux-musl.tar.gz\n",
        hash("e")
    ));
    for (t, c) in OLD_SUMS.iter().rev() {
        if Some(*t) != skip {
            text.push_str(&format!("{}  nrg-{t}.tar.gz\n", hash(c)));
        }
    }
    let sums = dir.join("checksums.txt");
    std::fs::write(&sums, text).unwrap();
    (formula, sums)
}

#[test]
fn rewrites_version_and_all_four_hashes_deterministically() {
    let dir = tempfile::tempdir().unwrap();
    let (formula, sums) = setup(dir.path(), None);
    let real_before = std::fs::read(root().join("Formula/nrg.rb")).unwrap();

    let out = run("v9.8.7", &sums, &formula);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let first = std::fs::read_to_string(&formula).unwrap();
    assert!(first.contains("  version \"9.8.7\"\n"));
    for (t, c) in OLD_SUMS {
        let url = format!("nrg-{t}.tar.gz\"\n");
        let at = first.find(&url).unwrap_or_else(|| panic!("url for {t}"));
        let after = &first[at + url.len()..];
        assert!(
            after
                .trim_start()
                .starts_with(&format!("sha256 \"{}\"", hash(c))),
            "{t}"
        );
    }
    assert!(!first.contains(&hash("e")), "musl hash must not leak in");

    // Idempotent, and a bare version works the same as a v-prefixed one.
    assert!(run("9.8.7", &sums, &formula).status.success());
    assert_eq!(std::fs::read_to_string(&formula).unwrap(), first);
    assert_eq!(
        std::fs::read(root().join("Formula/nrg.rb")).unwrap(),
        real_before,
        "the real formula must not be touched"
    );
}

#[test]
fn fails_loudly_and_leaves_the_formula_alone_when_a_target_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let (formula, sums) = setup(dir.path(), Some("aarch64-apple-darwin"));
    let before = std::fs::read(&formula).unwrap();
    let out = run("v9.8.7", &sums, &formula);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("nrg-aarch64-apple-darwin.tar.gz"));
    assert_eq!(std::fs::read(&formula).unwrap(), before);
}

#[test]
fn rejects_a_malformed_version() {
    let dir = tempfile::tempdir().unwrap();
    let (formula, sums) = setup(dir.path(), None);
    assert!(!run("latest", &sums, &formula).status.success());
}
