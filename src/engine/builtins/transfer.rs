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

fn parse_mode(mode: &str) -> Result<u32, Box<EvalAltResult>> {
    if !(mode.len() == 3 || mode.len() == 4 && mode.starts_with('0'))
        || !mode.bytes().all(|b| (b'0'..=b'7').contains(&b))
    {
        return Err("permissions must be an octal string, e.g. 0600 (no special bits)".into());
    }
    Ok(u32::from_str_radix(mode, 8).unwrap())
}

pub fn register(engine: &mut Engine, ctx: SharedCtx) {
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
                let result = to_result(
                    host,
                    ctx.runner.transfer_file(host, source, dest, mode, upload),
                );
                let message = diagnostics::record(&ctx, &detail, name, &result);
                if result.exit_code != 0 {
                    return Err(message.into());
                }
                Ok(result)
            },
        );
    }
}
