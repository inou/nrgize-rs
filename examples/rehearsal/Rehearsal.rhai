// A real local HTTP service, artifact replacement, restart and recovery checks.
// nrg rehearse examples/rehearsal/Rehearsal.rhai
// nrg rehearse examples/rehearsal/Rehearsal.rhai --execute --faults
import "std/contracts" as c;

let demo = "python3 \"$NRG_SOURCE/service.py\" ";
let healthy = [c::step("serves the original release over HTTP", demo + "check")];
c::service(#{
    name: "local HTTP recipe",
    setup: [c::step("record actual tools", "python3 --version; /bin/sh -c 'command -v python3'")],
    deploy: c::step("install v1 and start service", demo + "deploy v1"),
    restart: c::step("restart service", demo + "restart"),
    checks: healthy,
    cleanup: [c::step("stop owned service", demo + "stop")],
    faults: [
        c::rejects("unhealthy candidate restores v1", demo + "deploy broken", 42, healthy),
        c::rejects("interrupted upload preserves v1", demo + "interrupt-upload", 43, [
            c::step("no partial artifact remains", "test ! -e .upload-partial"),
            c::step("v1 still serves requests", demo + "check")
        ]),
        c::fault("killed service can restart", [
            c::step("kill owned service", demo + "kill"),
            c::step("restart after process failure", demo + "start")
        ], healthy)
    ]
})
