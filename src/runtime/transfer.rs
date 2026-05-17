//! Starlark built-in functions for file transfer to remote hosts.

use starlark::environment::GlobalsBuilder;
use starlark::values::none::NoneType;
use std::process::Command;

/// Register file transfer built-in functions into the Starlark global environment.
#[starlark::starlark_module]
pub fn transfer_builtins(builder: &mut GlobalsBuilder) {
    /// Upload a local file to a remote host via scp.
    ///
    /// Example:
    ///   upload("10.0.0.1", "./deploy.tar.gz", "/opt/app/deploy.tar.gz")
    fn upload(
        host: &str,
        local_path: &str,
        remote_path: &str,
    ) -> anyhow::Result<NoneType> {
        let trace = std::env::var("NRG_TRACE").is_ok();
        if trace {
            eprintln!(
                "[nrg] upload {} -> {}:{}",
                local_path, host, remote_path
            );
        }

        let dest = format!("{}:{}", host, remote_path);
        let output = Command::new("scp")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=accept-new")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg(local_path)
            .arg(&dest)
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to spawn scp: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!(
                "scp to {}:{} failed (exit {}): {}",
                host,
                remote_path,
                output.status.code().unwrap_or(-1),
                stderr.trim()
            ));
        }

        Ok(NoneType)
    }

    /// Write a string directly to a file on a remote host.
    ///
    /// This is useful for pushing templated configuration files without needing
    /// a local temporary file. Under the hood it pipes the content through SSH.
    ///
    /// Example:
    ///   conf = "upstream app { server 127.0.0.1:3001; }"
    ///   write_remote("10.0.0.1", conf, "/etc/nginx/conf.d/app.conf")
    ///   ssh_exec("10.0.0.1", "nginx -s reload")
    fn write_remote(
        host: &str,
        content: &str,
        remote_path: &str,
    ) -> anyhow::Result<NoneType> {
        let trace = std::env::var("NRG_TRACE").is_ok();
        if trace {
            eprintln!(
                "[nrg] write_remote {}:{} (content_len={})",
                host, remote_path, content.len()
            );
        }

        // Use ssh with stdin piped to `cat > remote_path`.
        // This avoids the need for a local temp file and handles arbitrary content
        // (including special characters) safely via stdin rather than command-line args.
        let cmd = format!("cat > '{}'", remote_path.replace('\'', "'\\''"));

        let mut child = Command::new("ssh")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=accept-new")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg(host)
            .arg(&cmd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn ssh for write_remote: {}", e))?;

        // Write content to stdin
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            stdin
                .write_all(content.as_bytes())
                .map_err(|e| anyhow::anyhow!("Failed to write to ssh stdin: {}", e))?;
            // stdin is dropped here, closing the pipe
        }

        let output = child
            .wait_with_output()
            .map_err(|e| anyhow::anyhow!("Failed to wait for ssh: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!(
                "write_remote to {}:{} failed (exit {}): {}",
                host,
                remote_path,
                output.status.code().unwrap_or(-1),
                stderr.trim()
            ));
        }

        Ok(NoneType)
    }
}
