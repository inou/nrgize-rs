use super::exec::{synthetic_ok, to_result};
use crate::engine::{
    context::SharedCtx,
    diagnostics,
    secret::{assert_no_secret_leak, posix_quote},
    types::ExecResult,
};
use rhai::{Engine, EvalAltResult};

pub fn atomic_write_command(path: &str, mode: u32, expected_bytes: u64) -> String {
    let path = if path.starts_with('-') {
        format!("./{path}")
    } else {
        path.to_string()
    };
    format!("umask 077; dest={}; [ ! -L \"$dest\" ] && [ ! -d \"$dest\" ] || exit 1; tmp=$(mktemp \"${{dest}}.XXXXXXXXXX\") || exit 1; trap 'rm -f \"$tmp\"' 0; trap 'exit 1' HUP INT TERM; cat > \"$tmp\" && [ \"$(wc -c < \"$tmp\")\" -eq {expected_bytes} ] && chmod {mode:03o} \"$tmp\" && mv -f \"$tmp\" \"$dest\"", posix_quote(&path))
}

const READ_FILE_LIMIT: u64 = 16 * 1024 * 1024;

fn read_text(path: &str) -> Result<String, Box<EvalAltResult>> {
    let meta = std::fs::metadata(path).map_err(|e| format!("read_file {path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("read_file {path}: not a regular file").into());
    }
    if meta.len() > READ_FILE_LIMIT {
        return Err(format!("read_file {path}: larger than 16 MiB").into());
    }
    std::fs::read_to_string(path).map_err(|e| format!("read_file {path}: {e}").into())
}

pub(crate) fn parse_mode(mode: &str) -> Result<u32, Box<EvalAltResult>> {
    if !(mode.len() == 3 || mode.len() == 4 && mode.starts_with('0'))
        || !mode.bytes().all(|b| (b'0'..=b'7').contains(&b))
    {
        return Err("permissions must be an octal string, e.g. 0600 (no special bits)".into());
    }
    Ok(u32::from_str_radix(mode, 8).unwrap())
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
    // read_file(path) — READ-ONLY; a local UTF-8 text file's contents, for configs a script
    // renders or writes with write_remote. Runs in dry-run too, so plans use the real input.
    // Relative paths resolve against the working directory. Throws if missing, not a regular
    // file (a FIFO would block), larger than READ_FILE_LIMIT, or not UTF-8.
    engine.register_fn(
        "read_file",
        |path: &str| -> Result<String, Box<EvalAltResult>> { read_text(path) },
    );

    for (name, upload) in [("upload_file", true), ("download_file", false)] {
        let ctx = ctx.clone();
        engine.register_fn(
            name,
            move |host: &str,
                  source: &str,
                  dest: &str,
                  permissions: &str|
                  -> Result<ExecResult, Box<EvalAltResult>> {
                let mode = parse_mode(permissions)?;
                for value in [host, source, dest] {
                    assert_no_secret_leak(value)?;
                }
                let detail = format!("{source} -> {dest} mode {mode:03o}");
                if ctx.is_dry_run() {
                    ctx.record(name, Some(host), detail);
                    return Ok(synthetic_ok(host));
                }
                let started = diagnostics::begin(&ctx, &detail, host, name);
                let result = to_result(
                    host,
                    ctx.runner.transfer_file(host, source, dest, mode, upload),
                );
                let message = diagnostics::record_started(&ctx, &detail, name, &result, started);
                ctx.check_interrupt()?;
                if result.exit_code != 0 {
                    return Err(message.into());
                }
                Ok(result)
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::shared_dry;
    use crate::engine::runner::FakeRunner;

    fn engine() -> (Engine, std::sync::Arc<FakeRunner>) {
        let fake = FakeRunner::shared();
        let mut e = Engine::new();
        register(&mut e, shared_dry(fake.clone()));
        (e, fake)
    }

    #[test]
    fn read_file_returns_contents_even_in_dry_run() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("providers.env");
        std::fs::write(&file, "S3_BUCKET=b\n").unwrap();
        let (e, fake) = engine();
        let body: String = e
            .eval(&format!(r#"read_file("{}")"#, file.display()))
            .unwrap();
        assert_eq!(body, "S3_BUCKET=b\n");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn read_file_rejects_missing_directories_fifos_and_non_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("blob");
        std::fs::write(&binary, [0xffu8, 0xfe]).unwrap();
        let fifo = dir.path().join("fifo");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let (e, _) = engine();
        for (path, expected) in [
            (dir.path().join("missing"), "No such file"),
            (dir.path().to_path_buf(), "not a regular file"),
            (fifo, "not a regular file"),
            (binary, "read_file"),
        ] {
            let err = e
                .run(&format!(r#"read_file("{}");"#, path.display()))
                .unwrap_err();
            assert!(err.to_string().contains(expected), "{path:?}: {err}");
        }
    }
}
