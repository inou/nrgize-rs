//! Doc-drift regression test: docs must not silently fall behind the code.
//!
//! Four checks, each failing with the missing item and the file to edit:
//!   a. every Rhai builtin registered in `src/engine/**` is documented in `docs/builtins.md`;
//!   b. every public `lib/*.rhai` fn / embedded module is documented in the stdlib docs;
//!   c. every subcommand and `--long-flag` in `nrg --help` is in `docs/cli.md`, and every flag on
//!      an `nrg ...` invocation in `docs/cli.md` exists in the CLI;
//!   d. relative markdown links and `#anchors` (GitHub slug rules) resolve.

use assert_cmd::Command;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("cannot read {rel}: {e}"))
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// True if `name` occurs in `text` as a whole identifier (not as part of a longer one).
fn mentions(text: &str, name: &str) -> bool {
    let mut start = 0;
    while let Some(i) = text[start..].find(name) {
        let s = start + i;
        let e = s + name.len();
        let before = text[..s].chars().next_back();
        let after = text[e..].chars().next();
        if !before.is_some_and(is_ident) && !after.is_some_and(is_ident) {
            return true;
        }
        start = e;
    }
    false
}

fn walk(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk(&p, ext, out);
        } else if p.extension().is_some_and(|e| e == ext) {
            out.push(p);
        }
    }
}

// ---------------------------------------------------------------- a. builtins

/// Registered names that are intentionally not documented as builtins in `docs/builtins.md`.
/// - "+": operator overloads on `Secret` (secret.rs), not callable builtins.
const BUILTIN_ALLOWLIST: &[&str] = &["+"];

fn quoted_literal(s: &str) -> Option<String> {
    let s = s.trim_start();
    let rest = s.strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_string())
}

/// Names registered with `engine.register_fn(` in non-test engine code. Handles the name on the
/// following line and the `for (name, _) in [("a", ..), ("b", ..)]` loop in `builtins/transfer.rs`.
fn registered_builtins() -> BTreeMap<String, String> {
    let mut files = Vec::new();
    walk(&root().join("src/engine"), "rs", &mut files);
    let mut out = BTreeMap::new();
    for f in files {
        let rel = f.strip_prefix(root()).unwrap().display().to_string();
        let src = fs::read_to_string(&f).unwrap();
        // Everything from the first `#[cfg(test)]` on is test code (helpers like mk/make200).
        let body = match src.find("#[cfg(test)]") {
            Some(i) => &src[..i],
            None => &src[..],
        };
        let mut search = 0;
        while let Some(i) = body[search..].find("register_fn(") {
            let after = search + i + "register_fn(".len();
            search = after;
            let arg = body[after..].trim_start();
            if let Some(n) = quoted_literal(arg) {
                out.insert(n, rel.clone());
            } else if arg.starts_with("name,") {
                // Loop registration: find the nearest preceding `for (name, ...) in [ ... ]`.
                let head = &body[..after];
                let f_idx = head.rfind("for (name,").unwrap_or_else(|| {
                    panic!("{rel}: `register_fn(name, ..)` without a `for (name, ..)` loop")
                });
                let list = &body[f_idx..after];
                let list = &list[list.find('[').unwrap()..];
                let list = &list[..list.find(']').unwrap()];
                for chunk in list.split("(\"").skip(1) {
                    let n = &chunk[..chunk.find('"').unwrap()];
                    out.insert(n.to_string(), rel.clone());
                }
            } else {
                panic!(
                    "{rel}: cannot determine registered name near `{}`",
                    &arg[..arg.len().min(40)]
                );
            }
        }
    }
    out
}

#[test]
fn every_registered_builtin_is_documented_in_builtins_md() {
    let doc = read("docs/builtins.md");
    let names = registered_builtins();
    assert!(
        names.len() > 40,
        "parser found only {} builtins; it is broken",
        names.len()
    );
    for (name, file) in &names {
        if BUILTIN_ALLOWLIST.contains(&name.as_str()) {
            continue;
        }
        assert!(
            mentions(&doc, name),
            "builtin `{name}` (registered in {file}) is not mentioned in docs/builtins.md; \
             document it there (or add it to BUILTIN_ALLOWLIST in tests/doc_drift.rs with a reason)"
        );
    }
}

// ---------------------------------------------------------------- b. stdlib

/// Public (non-`private`) top-level fn names in a `.rhai` source.
fn public_fns(src: &str) -> Vec<String> {
    src.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("fn ")?;
            let end = rest.find('(')?;
            Some(rest[..end].trim().to_string())
        })
        .collect()
}

/// Docs that stdlib.md delegates to for some modules.
const STDLIB_DOCS: &[&str] = &[
    "docs/stdlib.md",
    "docs/workflows.md",
    "docs/rehearsal.md",
    "docs/deploy.md",
];

#[test]
fn every_public_stdlib_fn_is_documented() {
    let docs: Vec<(String, String)> = STDLIB_DOCS
        .iter()
        .map(|d| (d.to_string(), read(d)))
        .collect();
    let mut files = Vec::new();
    walk(&root().join("lib"), "rhai", &mut files);
    // top-level lib/*.rhai only (lib/examples are sample projects, not modules)
    files.retain(|f| f.parent() == Some(root().join("lib").as_path()));
    assert!(files.len() >= 14, "found only {} lib modules", files.len());
    for f in files {
        let module = f.file_stem().unwrap().to_string_lossy().to_string();
        let src = fs::read_to_string(&f).unwrap();
        for name in public_fns(&src) {
            assert!(
                docs.iter().any(|(_, t)| mentions(t, &name)),
                "public fn `{name}` in lib/{module}.rhai is not mentioned in any of {STDLIB_DOCS:?}; \
                 document it in docs/stdlib.md (or make it `private fn`)"
            );
        }
    }
}

#[test]
fn every_embedded_module_is_mentioned_in_stdlib_md() {
    let doc = read("docs/stdlib.md");
    let src = read("src/engine/stdlib.rs");
    let start = src.find("const EMBEDDED").expect("EMBEDDED not found");
    let block = &src[start..src[start..].find("];").unwrap() + start];
    let mut found = 0;
    for chunk in block.split("include_str!(\"../../lib/").skip(1) {
        let m = &chunk[..chunk.find(".rhai").unwrap()];
        found += 1;
        assert!(
            doc.contains(&format!("std/{m}")),
            "embedded module `std/{m}` (src/engine/stdlib.rs) is not mentioned in docs/stdlib.md; add it"
        );
    }
    assert!(found >= 14, "parsed only {found} embedded modules");
}

// ---------------------------------------------------------------- c. CLI

fn help(args: &[&str]) -> String {
    let out = Command::cargo_bin("nrg")
        .unwrap()
        .args(args)
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success(), "nrg {args:?} --help failed");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// (subcommand paths, long flags) discovered by walking `--help` recursively.
fn cli_surface() -> (BTreeSet<Vec<String>>, BTreeSet<String>) {
    let mut cmds = BTreeSet::new();
    let mut flags = BTreeSet::new();
    let mut stack: Vec<Vec<String>> = vec![vec![]];
    while let Some(path) = stack.pop() {
        let args: Vec<&str> = path.iter().map(String::as_str).collect();
        let text = help(&args);
        let mut section = "";
        for line in text.lines() {
            if !line.starts_with(' ') && line.ends_with(':') {
                section = if line.starts_with("Commands") {
                    "cmds"
                } else if line.starts_with("Options") {
                    "opts"
                } else {
                    ""
                };
                continue;
            }
            match section {
                "cmds" if line.starts_with("  ") && !line.starts_with("   ") => {
                    let name = line.split_whitespace().next().unwrap();
                    if name != "help" {
                        let mut p = path.clone();
                        p.push(name.to_string());
                        cmds.insert(p.clone());
                        stack.push(p);
                    }
                }
                "opts" => {
                    if let Some(i) = line.find("--") {
                        let tok: String = line[i..]
                            .chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                            .collect();
                        // option lines look like `  -y, --yes` or `      --host <HOST>`;
                        // prose continuation lines never start with `-`.
                        if tok.len() > 2 && line.trim_start().starts_with('-') {
                            flags.insert(tok);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    (cmds, flags)
}

/// Flags of other tools that legitimately appear next to `nrg` in cli.md examples.
/// - "--path": `mise ... --path` style invocations.
const CLI_DOC_FLAG_ALLOWLIST: &[&str] = &["--path"];

#[test]
fn every_cli_subcommand_and_flag_is_documented_in_cli_md() {
    let doc = read("docs/cli.md");
    let (cmds, flags) = cli_surface();
    assert!(
        cmds.len() >= 15,
        "found only {} subcommands; parser broken",
        cmds.len()
    );
    for c in &cmds {
        let full = format!("nrg {}", c.join(" "));
        assert!(
            doc.contains(&full),
            "subcommand `{full}` is not documented in docs/cli.md; add a section for it"
        );
    }
    for f in flags.iter().filter(|f| *f != "--help") {
        assert!(
            mentions_flag(&doc, f),
            "flag `{f}` (from `nrg ... --help`) is not documented in docs/cli.md; add it"
        );
    }
}

fn mentions_flag(text: &str, flag: &str) -> bool {
    let mut start = 0;
    while let Some(i) = text[start..].find(flag) {
        let e = start + i + flag.len();
        if !text[e..]
            .chars()
            .next()
            .is_some_and(|c| is_ident(c) || c == '-')
        {
            return true;
        }
        start = e;
    }
    false
}

/// Code lines (fenced blocks and inline spans) of a markdown document.
fn code_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            out.push(line.to_string());
        } else {
            for (i, span) in line.split('`').enumerate() {
                if i % 2 == 1 {
                    out.push(span.to_string());
                }
            }
        }
    }
    out
}

#[test]
fn every_flag_on_an_nrg_invocation_in_cli_md_exists() {
    let doc = read("docs/cli.md");
    let (_, flags) = cli_surface();
    let mut known: BTreeSet<String> = flags;
    known.insert("--help".into());
    known.insert("--version".into());
    for line in code_lines(&doc) {
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(pos) = words.iter().position(|w| *w == "nrg") else {
            continue;
        };
        for w in &words[pos..] {
            if let Some(rest) = w.strip_prefix("--") {
                let tok: String = std::iter::once("--".to_string())
                    .chain(std::iter::once(
                        rest.chars()
                            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                            .collect::<String>(),
                    ))
                    .collect();
                if tok.len() > 2
                    && !known.contains(&tok)
                    && !CLI_DOC_FLAG_ALLOWLIST.contains(&tok.as_str())
                {
                    panic!(
                        "docs/cli.md shows `{tok}` on an `nrg` invocation (`{}`) but no such flag exists in \
                         `nrg --help`; fix docs/cli.md (or add to CLI_DOC_FLAG_ALLOWLIST in tests/doc_drift.rs)",
                        line.trim()
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------- d. links

/// GitHub heading slug: lowercase, drop everything except alphanumerics/space/hyphen/underscore,
/// spaces to hyphens. Inline markup (backticks, emphasis, link syntax) is stripped first.
fn slug(heading: &str) -> String {
    let mut h = String::new();
    let mut chars = heading.trim().chars().peekable();
    // [text](url) -> text
    while let Some(c) = chars.next() {
        if c == '[' {
            let mut t = String::new();
            for c2 in chars.by_ref() {
                if c2 == ']' {
                    break;
                }
                t.push(c2);
            }
            if chars.peek() == Some(&'(') {
                for c2 in chars.by_ref() {
                    if c2 == ')' {
                        break;
                    }
                }
            }
            h.push_str(&t);
        } else {
            h.push(c);
        }
    }
    h.chars()
        .filter(|c| *c != '`' && *c != '*')
        .flat_map(|c| c.to_lowercase())
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

fn anchors(text: &str) -> BTreeSet<String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = BTreeSet::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let t = line.trim_end();
        let hashes = t.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            let s = slug(t[hashes..].trim().trim_end_matches('#').trim_end());
            let n = seen.entry(s.clone()).or_insert(0);
            out.insert(if *n == 0 { s } else { format!("{s}-{n}") });
            *n += 1;
        }
    }
    out
}

/// Inline link targets `](target)` outside code fences and code spans.
fn link_targets(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut fenced = false;
    for (ln, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        // blank out code spans
        let clean: String = line
            .split('`')
            .enumerate()
            .map(|(i, s)| if i % 2 == 1 { "" } else { s })
            .collect::<Vec<_>>()
            .join("`");
        let mut rest = clean.as_str();
        while let Some(i) = rest.find("](") {
            let after = &rest[i + 2..];
            let Some(end) = after.find(')') else { break };
            let target = after[..end]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            out.push((ln + 1, target));
            rest = &after[end..];
        }
    }
    out
}

#[test]
fn relative_markdown_links_and_anchors_resolve() {
    let mut files: Vec<PathBuf> = ["README.md", "CONTRIBUTING.md", "CHANGELOG.md"]
        .iter()
        .map(|f| root().join(f))
        .collect();
    // docs/*.md only; historical report subdirectories (docs/audit-*, archive) are not checked.
    for e in fs::read_dir(root().join("docs")).unwrap().flatten() {
        if e.path().extension().is_some_and(|x| x == "md") {
            files.push(e.path());
        }
    }
    let mut cache: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    let mut checked = 0;
    for f in &files {
        let rel = f.strip_prefix(root()).unwrap().display().to_string();
        let text = fs::read_to_string(f).unwrap();
        for (ln, target) in link_targets(&text) {
            if target.is_empty() || target.contains("://") || target.starts_with("mailto:") {
                continue;
            }
            let (path_part, anchor) = match target.split_once('#') {
                Some((p, a)) => (p, Some(a)),
                None => (target.as_str(), None),
            };
            let dest = if path_part.is_empty() {
                f.clone()
            } else {
                f.parent().unwrap().join(path_part)
            };
            checked += 1;
            assert!(
                dest.exists(),
                "{rel}:{ln}: link target `{target}` does not exist; fix the link in {rel}"
            );
            if let Some(a) = anchor {
                if dest.extension().is_some_and(|e| e == "md") {
                    let set = cache
                        .entry(dest.clone())
                        .or_insert_with(|| anchors(&fs::read_to_string(&dest).unwrap()));
                    assert!(
                        set.contains(a),
                        "{rel}:{ln}: anchor `#{a}` not found in {}; fix the link in {rel} \
                         (or the heading it points at)",
                        dest.strip_prefix(root()).unwrap().display()
                    );
                }
            }
        }
    }
    assert!(
        checked > 50,
        "only {checked} links checked; extractor broken"
    );
}
