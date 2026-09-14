//! Bounded step records and incremental byte redaction, before truncation or UTF-8 decoding.
use crate::engine::{context::RunCtx, types::ExecResult};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepRecord {
    pub name: String,
    pub host: String,
    pub operation: String,
    pub exit_code: i64,
    pub excerpt: String,
}

pub fn record(ctx: &RunCtx, name: &str, operation: &str, result: &ExecResult) -> String {
    let excerpt = if result.stderr.contains("command output exceeded limit") {
        "command output exceeded limit (incomplete output omitted)".into()
    } else if result.exit_code == 0 {
        String::new()
    } else {
        // Redact BEFORE bounding: cutting a secret in half first would expose its prefix.
        let tail = |s: &str, limit: usize| {
            let text = ctx.redacted(s);
            text.chars()
                .skip(text.chars().count().saturating_sub(limit))
                .collect::<String>()
        };
        format!(
            "{}\n{}",
            tail(&result.stderr, 1535),
            tail(&result.stdout, 512)
        )
    };
    let step = StepRecord {
        name: ctx.redacted(name).chars().take(256).collect(),
        host: ctx.redacted(&result.host).chars().take(256).collect(),
        operation: operation.to_string(),
        exit_code: result.exit_code,
        excerpt,
    };
    let message = format!(
        "step '{}' on {}: {} exited {}: {}",
        step.name,
        if step.host.is_empty() {
            "local"
        } else {
            &step.host
        },
        step.operation,
        step.exit_code,
        step.excerpt
    );
    let mut steps = ctx.steps.lock().unwrap();
    if steps.len() == 128 {
        steps.remove(0);
    }
    steps.push(step);
    message
}

/// Retain a potential secret prefix across reads. Each stream has its own decoder.
/// Match longest-first, including overlapping values. EOF flushes incomplete prefixes.
pub struct Redactor {
    secrets: Vec<Vec<u8>>,
    pending: Vec<u8>,
}

impl Redactor {
    pub fn new(secrets: &[String]) -> Self {
        let mut secrets: Vec<_> = secrets
            .iter()
            .filter(|s| !s.is_empty())
            .flat_map(|s| {
                let json = serde_json::to_string(s).expect("string serialization");
                vec![
                    s.as_bytes().to_vec(),
                    json.as_bytes()[1..json.len() - 1].to_vec(),
                    crate::engine::secret::posix_quote(s).into_bytes(),
                ]
            })
            .collect();
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Self {
            secrets,
            pending: Vec::new(),
        }
    }

    pub fn push(&mut self, bytes: &[u8], eof: bool) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut at = 0;
        while at < self.pending.len() {
            let remaining = &self.pending[at..];
            if !eof
                && self
                    .secrets
                    .iter()
                    .any(|s| s.len() > remaining.len() && s.starts_with(remaining))
            {
                break;
            }
            if let Some(secret) = self.secrets.iter().find(|s| remaining.starts_with(s)) {
                out.extend_from_slice(b"***");
                at += secret.len();
            } else {
                out.push(self.pending[at]);
                at += 1;
            }
        }
        self.pending.drain(..at);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escaped_secrets_and_unicode_are_redacted_at_every_boundary() {
        let secret = "private'秘密\nvalue".to_string();
        let json = serde_json::to_string(&secret).unwrap();
        for form in [
            secret.clone(),
            json[1..json.len() - 1].to_string(),
            crate::engine::secret::posix_quote(&secret),
        ] {
            for split in 0..=form.len() {
                let mut r = Redactor::new(std::slice::from_ref(&secret));
                let mut out = r.push(&form.as_bytes()[..split], false);
                out.extend(r.push(&form.as_bytes()[split..], true));
                assert_eq!(out, b"***");
            }
        }
    }
    #[test]
    fn every_chunk_boundary_and_overlapping_secrets() {
        let input = b"before password-long after password!";
        for split in 0..=input.len() {
            let mut r = Redactor::new(&["password".into(), "password-long".into()]);
            let mut out = r.push(&input[..split], false);
            out.extend(r.push(&input[split..], true));
            assert_eq!(out, b"before *** after ***!");
        }
    }
}
