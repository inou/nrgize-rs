//! Owned remote locks. Drop runs on normal errors and Rhai interruption alike.
use super::runner::CommandRunner;
use super::secret::posix_quote;
use std::sync::Arc;

pub struct RemoteLock {
    pub host: String,
    pub directory: String,
    token: String,
    runner: Arc<dyn CommandRunner>,
}
impl RemoteLock {
    pub fn acquire(
        runner: Arc<dyn CommandRunner>,
        host: &str,
        directory: &str,
    ) -> Result<Self, String> {
        // OS-created random identity; the local file itself holds no sensitive material.
        let identity = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
        let token = identity
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let lock = Self {
            host: host.into(),
            directory: directory.into(),
            token,
            runner,
        };
        let cmd = format!(
            "umask 077; mkdir {} && printf '%s' {} > {}",
            posix_quote(directory),
            posix_quote(&lock.token),
            posix_quote(&format!("{directory}/holder"))
        );
        let result = lock.runner.run_ssh(host, &cmd);
        if result.exit_code != 0 {
            return Err(format!(
                "could not acquire deploy lock on {host}: {}{}",
                result.stdout, result.stderr
            ));
        }
        Ok(lock)
    }
}
impl Drop for RemoteLock {
    fn drop(&mut self) {
        let holder = posix_quote(&format!("{}/holder", self.directory));
        let cmd = format!(
            "if [ \"$(cat {holder} 2>/dev/null)\" = {} ]; then rm -f {holder} && rmdir {}; fi",
            posix_quote(&self.token),
            posix_quote(&self.directory)
        );
        let out = self.runner.run_ssh(&self.host, &cmd);
        if out.exit_code != 0 {
            eprintln!(
                "Warning: remote lock release failed; inspect the service lock before retrying"
            );
        }
    }
}
