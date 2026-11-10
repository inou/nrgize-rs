//! ssh tests for `engine::eval`.

use super::super::*;
use crate::engine::context::shared;
use crate::engine::runner::{FakeRunner, RawOutput};
use std::fs;

#[test]
fn ssh_exec_all_attributes_a_panicking_host_thread_and_keeps_the_rest() {
    // Issue #27: if run_ssh PANICS on one host's fan-out thread, ssh_exec_all must still
    // return one result per host, in host order, with the panicked host reported as
    // exit_code -1 / "thread panicked" under its own name, and the others untouched.
    use crate::engine::runner::CommandRunner;
    struct PanicOn(FakeRunner, &'static str);
    impl CommandRunner for PanicOn {
        fn run_ssh(&self, host: &str, cmd: &str) -> RawOutput {
            if host == self.1 {
                panic!("simulated runner panic on {host}");
            }
            self.0.run_ssh(host, cmd)
        }
        fn run_local(&self, cmd: &str) -> RawOutput {
            self.0.run_local(cmd)
        }
        fn run_ssh_stdin(&self, host: &str, cmd: &str, stdin: &str) -> RawOutput {
            self.0.run_ssh_stdin(host, cmd, stdin)
        }
        fn run_local_stdin(&self, cmd: &str, stdin: &str) -> RawOutput {
            self.0.run_local_stdin(cmd, stdin)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("Energize.rhai");
    fs::write(
        &main,
        r#"let r = ssh_exec_all(["a", "b", "c"], "do thing");
           if r.len() != 3 { throw "len " + r.len(); }
           let got = "";
           for x in r { got += x.host + ":" + x.exit_code + ":" + x.stderr + ";"; }
           if got != "a:0:;b:-1:thread panicked;c:0:;" { throw "got " + got; }"#,
    )
    .unwrap();
    let ctx = shared(Arc::new(PanicOn(FakeRunner::new(), "b")));
    run_file(&main, ctx).unwrap();
}
