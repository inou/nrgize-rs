//! Result types exposed to Rhai scripts.

/// Result of running a command (locally or over SSH).
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i64,
    pub host: String,
}

/// Result of an HTTP request.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: i64,
    pub body: String,
}

/// Register both types (with read-only getters) into a Rhai engine.
pub fn register_types(engine: &mut rhai::Engine) {
    engine
        .register_type_with_name::<ExecResult>("ExecResult")
        .register_get("stdout", |r: &mut ExecResult| r.stdout.clone())
        .register_get("stderr", |r: &mut ExecResult| r.stderr.clone())
        .register_get("exit_code", |r: &mut ExecResult| r.exit_code)
        .register_get("host", |r: &mut ExecResult| r.host.clone())
        .register_get("ok", |r: &mut ExecResult| r.exit_code == 0);
    engine
        .register_type_with_name::<HttpResponse>("HttpResponse")
        .register_get("status", |r: &mut HttpResponse| r.status)
        .register_get("body", |r: &mut HttpResponse| r.body.clone())
        .register_get("ok", |r: &mut HttpResponse| (200..300).contains(&r.status));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_result_getters_readable_in_script() {
        let mut engine = rhai::Engine::new();
        register_types(&mut engine);
        engine.register_fn("make", || ExecResult {
            stdout: "hi".into(),
            stderr: String::new(),
            exit_code: 0,
            host: "web1".into(),
        });
        let out: bool = engine
            .eval(r#"let r = make(); r.ok && r.stdout == "hi" && r.host == "web1""#)
            .unwrap();
        assert!(out);
    }

    #[test]
    fn http_response_ok_is_2xx() {
        let mut engine = rhai::Engine::new();
        register_types(&mut engine);
        engine.register_fn("make404", || HttpResponse {
            status: 404,
            body: "x".into(),
        });
        engine.register_fn("make200", || HttpResponse {
            status: 200,
            body: "x".into(),
        });
        let bad: bool = engine.eval("make404().ok").unwrap();
        let good: bool = engine.eval("make200().ok").unwrap();
        assert!(!bad && good);
    }
}
