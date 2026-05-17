//! Custom Starlark types for runtime primitives.
//!
//! These types are returned by built-in functions (ssh_exec, local_exec, http_get, etc.)
//! and are fully accessible from Starlark code with attribute access.

use allocative::Allocative;
use starlark::starlark_simple_value;
use starlark::values::none::NoneType;
use starlark::values::Heap;
use starlark::values::ProvidesStaticType;
use starlark::values::StarlarkValue;
use starlark::values::Value;
use starlark_derive::starlark_value;
use std::fmt;

// ---------------------------------------------------------------------------
// ExecResult — returned by ssh_exec, local_exec, ssh_exec_all
// ---------------------------------------------------------------------------

/// Result of executing a command, either locally or remotely via SSH.
///
/// Starlark attributes:
///   - `stdout`    (string)  — captured standard output
///   - `stderr`    (string)  — captured standard error
///   - `exit_code` (int)     — process exit code (-1 if signal/unknown)
///   - `host`      (string|None) — remote hostname, or None for local execution
///   - `ok`        (bool)    — convenience: true when exit_code == 0
#[derive(Debug, Clone, ProvidesStaticType, Allocative)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub host: Option<String>,
}

impl fmt::Display for ExecResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.host {
            Some(h) => write!(
                f,
                "ExecResult(host=\"{}\", exit_code={}, stdout_len={})",
                h,
                self.exit_code,
                self.stdout.len()
            ),
            None => write!(
                f,
                "ExecResult(local, exit_code={}, stdout_len={})",
                self.exit_code,
                self.stdout.len()
            ),
        }
    }
}

impl serde::Serialize for ExecResult {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

starlark_simple_value!(ExecResult);

#[starlark_value(type = "ExecResult")]
impl<'v> StarlarkValue<'v> for ExecResult {
    type Canonical = Self;

    fn get_attr(&self, attribute: &str, heap: &'v Heap) -> Option<Value<'v>> {
        match attribute {
            "stdout" => Some(heap.alloc(self.stdout.as_str())),
            "stderr" => Some(heap.alloc(self.stderr.as_str())),
            "exit_code" => Some(heap.alloc(self.exit_code)),
            "host" => Some(match &self.host {
                Some(h) => heap.alloc(h.as_str()),
                None => heap.alloc(NoneType),
            }),
            "ok" => Some(heap.alloc(self.exit_code == 0)),
            _ => None,
        }
    }

    fn has_attr(&self, attribute: &str, _heap: &'v Heap) -> bool {
        matches!(attribute, "stdout" | "stderr" | "exit_code" | "host" | "ok")
    }

    fn dir_attr(&self) -> Vec<String> {
        vec![
            "stdout".into(),
            "stderr".into(),
            "exit_code".into(),
            "host".into(),
            "ok".into(),
        ]
    }
}

impl ExecResult {
    /// Create a new ExecResult for a remote command.
    pub fn remote(host: impl Into<String>, stdout: String, stderr: String, exit_code: i32) -> Self {
        Self {
            stdout,
            stderr,
            exit_code,
            host: Some(host.into()),
        }
    }

    /// Create a new ExecResult for a local command.
    pub fn local(stdout: String, stderr: String, exit_code: i32) -> Self {
        Self {
            stdout,
            stderr,
            exit_code,
            host: None,
        }
    }
}

// ---------------------------------------------------------------------------
// HttpResponse — returned by http_get, http_post
// ---------------------------------------------------------------------------

/// Result of an HTTP request.
///
/// Starlark attributes:
///   - `status` (int)    — HTTP status code (200, 404, 500, etc.)
///   - `body`   (string) — response body as text
///   - `ok`     (bool)   — convenience: true when 200 <= status < 300
#[derive(Debug, Clone, ProvidesStaticType, Allocative)]
pub struct HttpResponse {
    pub status: i32,
    pub body: String,
}

impl fmt::Display for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "HttpResponse(status={}, body_len={})",
            self.status,
            self.body.len()
        )
    }
}

impl serde::Serialize for HttpResponse {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

starlark_simple_value!(HttpResponse);

#[starlark_value(type = "HttpResponse")]
impl<'v> StarlarkValue<'v> for HttpResponse {
    type Canonical = Self;

    fn get_attr(&self, attribute: &str, heap: &'v Heap) -> Option<Value<'v>> {
        match attribute {
            "status" => Some(heap.alloc(self.status)),
            "body" => Some(heap.alloc(self.body.as_str())),
            "ok" => Some(heap.alloc((200..300).contains(&self.status))),
            _ => None,
        }
    }

    fn has_attr(&self, attribute: &str, _heap: &'v Heap) -> bool {
        matches!(attribute, "status" | "body" | "ok")
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["status".into(), "body".into(), "ok".into()]
    }
}
