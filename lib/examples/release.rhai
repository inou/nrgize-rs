// Generic release directories: customize these hooks for your artifact and supervisor.
// nrg run deploy <version> / nrg run rollback <version>
import "std/release" as release;

fn config() {
    #{
        prepare: "cp -R /srv/staged/myapp/. .",
        activate: "sudo -n systemctl restart myapp",
        health: "curl --fail --silent --show-error http://127.0.0.1:8080/health",
        deactivate: "sudo -n systemctl stop myapp",
        timeout_secs: 120
    }
}
fn deploy(version) {
    release::deploy("deploy@web1", "/srv/myapp", version, config());
}
fn rollback(version) {
    release::rollback("deploy@web1", "/srv/myapp", version, config());
}
