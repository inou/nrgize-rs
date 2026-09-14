//! Durable metadata events. An unfinished run may still be running; never auto-resume it.
use super::diagnostics::StepRecord;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Serialize, Deserialize)]
pub struct Event {
    pub run_id: String,
    pub timestamp_ms: u128,
    pub event: String,
    pub step_id: Option<u64>,
    pub step: Option<JournalStep>,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalStep {
    pub name: String,
    pub host: String,
    pub operation: String,
    pub exit_code: Option<i64>,
    pub excerpt: String,
}

pub struct Journal {
    pub run_id: String,
    file: File,
    next: u64,
}

impl Journal {
    pub fn open(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(root.join(".energize"))?;
        let mut opts = OpenOptions::new();
        opts.create(true).append(true).read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut file = opts.open(root.join(".energize/runs.jsonl"))?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("run journal is not a regular file"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        if file.metadata()?.len() > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                file.write_all(b"\n")?;
            }
        }
        let mut j = Self {
            run_id: format!("{:x}-{}", now().as_nanos(), std::process::id()),
            file,
            next: 0,
        };
        j.write("run_started", None, None, None)?;
        Ok(j)
    }

    fn write(
        &mut self,
        event: &str,
        step_id: Option<u64>,
        step: Option<JournalStep>,
        exit_code: Option<i32>,
    ) -> std::io::Result<()> {
        let e = Event {
            run_id: self.run_id.clone(),
            timestamp_ms: now().as_millis(),
            event: event.into(),
            step_id,
            step,
            exit_code,
        };
        let mut line = serde_json::to_vec(&e)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.sync_data()
    }
    fn append(
        &mut self,
        event: &str,
        id: Option<u64>,
        step: Option<JournalStep>,
        code: Option<i32>,
    ) {
        if let Err(e) = self.write(event, id, step, code) {
            eprintln!("Warning: cannot persist run journal event: {e}");
        }
    }
    pub fn begin(&mut self, name: &str, host: &str, operation: &str) -> u64 {
        self.next += 1;
        self.append(
            "step_started",
            Some(self.next),
            Some(JournalStep {
                name: name.chars().take(256).collect(),
                host: host.chars().take(256).collect(),
                operation: operation.into(),
                exit_code: None,
                excerpt: String::new(),
            }),
            None,
        );
        self.next
    }
    pub fn finish_step(&mut self, id: Option<u64>, step: &StepRecord) {
        self.append(
            "step_finished",
            id,
            Some(JournalStep {
                name: step.name.clone(),
                host: step.host.clone(),
                operation: step.operation.clone(),
                exit_code: Some(step.exit_code),
                excerpt: step.excerpt.clone(),
            }),
            None,
        );
    }
    pub fn finish_run(&mut self, code: i32) {
        self.append("run_finished", None, None, Some(code));
    }
}

fn now() -> std::time::Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

pub fn read(root: &Path) -> Vec<Event> {
    let content = match std::fs::read_to_string(root.join(".energize/runs.jsonl")) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return vec![],
        Err(e) => {
            eprintln!("Warning: cannot read run journal: {e}");
            return vec![];
        }
    };
    content
        .lines()
        .filter_map(|line| match serde_json::from_str(line) {
            Ok(e) => Some(e),
            Err(_) => {
                eprintln!("Warning: malformed run event omitted");
                None
            }
        })
        .collect()
}
