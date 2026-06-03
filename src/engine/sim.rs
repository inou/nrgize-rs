//! The dry-run container SIM overlay.
//!
//! `SimState` is the single source of truth for "what the hosts look like" during a dry run.
//! The ported `.rhai` stdlib never inlines a raw `docker inspect` / `nc -z` via `ssh_exec`;
//! instead it reads and mutates this model through the typed `sim_*` builtins (see
//! `builtins/sim.rs`). That keeps a read-after-write self-consistent: a stubbed
//! `sim_docker_run` of the NEW container makes `sim_container_running(new)` and
//! `sim_container_healthy(new)` true, so the deploy dry-run takes the same branches a real
//! run would.
//!
//! Reads are seeded LAZILY from exactly ONE real probe per (host, name) entity, on first
//! access in dry-run, then never re-read — they only change via a stubbed mutating builtin.

use std::collections::{HashMap, HashSet};

/// Simulated state of one container.
#[derive(Debug, Clone)]
pub struct ContainerSim {
    pub running: bool,
    /// Image tag/ref the container was started from (best-effort; not a real digest).
    pub image: String,
    /// Whether the container's healthcheck is passing in the sim.
    pub health_ok: bool,
}

/// The simulated container / port / proxy world for a dry run.
///
/// Keyed by `(host, name)` for containers, `(host, port)` for occupancy, and
/// `(host, service)` for proxy targets.
#[derive(Default)]
pub struct SimState {
    /// (host, name) -> container.
    containers: HashMap<(String, String), ContainerSim>,
    /// One-real-read guard: a (host, name) entity that has been seeded already.
    seeded: HashSet<(String, String)>,
    /// host -> how many ports have been picked so far (for deterministic offsets).
    port_picks: HashMap<String, u16>,
    /// (host, port) marked occupied by a sim_docker_run / sim_pick_port.
    occupied: HashSet<(String, u16)>,
    /// (host, service) -> current proxy target, for read-back / rollback snapshots.
    proxy: HashMap<(String, String), String>,
}

impl SimState {
    /// Whether `(host, name)` has already been seeded from a real probe.
    pub fn is_seeded(&self, host: &str, name: &str) -> bool {
        self.seeded.contains(&(host.to_string(), name.to_string()))
    }

    /// Seed a container's running flag from ONE real read, on first access only, then return
    /// the sim's current running value. `real_running` is the realized probe result the caller
    /// obtained OUTSIDE any lock; it is only consulted on the very first access for this entity.
    pub fn seed_running(&mut self, host: &str, name: &str, real_running: bool) -> bool {
        let key = (host.to_string(), name.to_string());
        if self.seeded.insert(key.clone()) {
            // First access: record the pre-deploy reality.
            self.containers.entry(key.clone()).or_insert(ContainerSim {
                running: real_running,
                image: String::new(),
                health_ok: real_running,
            });
        }
        self.containers.get(&key).map(|c| c.running).unwrap_or(false)
    }

    /// Mark `(host, name)` as a freshly started, healthy container running `image`.
    pub fn set_running(&mut self, host: &str, name: &str, image: &str) {
        self.containers.insert(
            (host.to_string(), name.to_string()),
            ContainerSim {
                running: true,
                image: image.to_string(),
                health_ok: true,
            },
        );
        // A just-started container is also marked seeded so a later read doesn't re-probe.
        self.seeded.insert((host.to_string(), name.to_string()));
    }

    /// Mark `(host, name)` stopped (no longer running / healthy).
    pub fn set_stopped(&mut self, host: &str, name: &str) {
        if let Some(c) = self.containers.get_mut(&(host.to_string(), name.to_string())) {
            c.running = false;
            c.health_ok = false;
        }
    }

    /// Move a container from `old` to `new` on `host` (promotion). The seeded marker moves too.
    pub fn rename(&mut self, host: &str, old: &str, new: &str) {
        let old_key = (host.to_string(), old.to_string());
        let new_key = (host.to_string(), new.to_string());
        if let Some(c) = self.containers.remove(&old_key) {
            self.containers.insert(new_key.clone(), c);
        }
        self.seeded.remove(&old_key);
        self.seeded.insert(new_key);
    }

    /// Remove `(host, name)` entirely.
    pub fn remove(&mut self, host: &str, name: &str) {
        self.containers.remove(&(host.to_string(), name.to_string()));
    }

    /// Sim running flag for `(host, name)` (false if unknown).
    pub fn is_running(&self, host: &str, name: &str) -> bool {
        self.containers
            .get(&(host.to_string(), name.to_string()))
            .map(|c| c.running)
            .unwrap_or(false)
    }

    /// Set the image id/tag for an existing-or-new `(host, name)` entity without changing its
    /// running state (used to cache a seeded real image id).
    pub fn set_image(&mut self, host: &str, name: &str, image: &str) {
        self.containers
            .entry((host.to_string(), name.to_string()))
            .or_insert(ContainerSim {
                running: false,
                image: String::new(),
                health_ok: false,
            })
            .image = image.to_string();
    }

    /// Sim image id/tag for `(host, name)` (empty if unknown).
    pub fn image_id(&self, host: &str, name: &str) -> String {
        self.containers
            .get(&(host.to_string(), name.to_string()))
            .map(|c| c.image.clone())
            .unwrap_or_default()
    }

    /// Sim health for `(host, name)`: running AND healthy.
    pub fn is_healthy(&self, host: &str, name: &str) -> bool {
        self.containers
            .get(&(host.to_string(), name.to_string()))
            .map(|c| c.running && c.health_ok)
            .unwrap_or(false)
    }

    /// Deterministically pick the next free port on `host`: `base + 10000 + Nth-pick`. Marks it
    /// occupied so a 2nd pick on the same host differs. Pure function of (host, base, call-count)
    /// so repeated dry-runs print identical plans. Never probes.
    pub fn pick_port(&mut self, host: &str, base: u16) -> u16 {
        let n = self.port_picks.entry(host.to_string()).or_insert(0);
        let port = base.saturating_add(10000).saturating_add(*n);
        *n += 1;
        self.occupied.insert((host.to_string(), port));
        port
    }

    /// Whether `(host, port)` is marked occupied in the sim (i.e. a sim_docker_run /
    /// sim_pick_port put something there).
    pub fn port_open(&self, host: &str, port: u16) -> bool {
        self.occupied.contains(&(host.to_string(), port))
    }

    /// Record a proxy target switch for `(host, service)`.
    pub fn proxy_switch(&mut self, host: &str, service: &str, target: &str) {
        self.proxy
            .insert((host.to_string(), service.to_string()), target.to_string());
    }

    /// Current proxy target for `(host, service)`, if any. Read-back for the deploy rollback
    /// snapshot ported in P5c.
    #[allow(dead_code)] // consumed by deploy.rhai's rollback wiring (P5c)
    pub fn proxy_target(&self, host: &str, service: &str) -> Option<String> {
        self.proxy
            .get(&(host.to_string(), service.to_string()))
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_running_caches_one_real_read() {
        let mut s = SimState::default();
        assert!(!s.is_seeded("web1", "app"));
        // First access seeds from the real value (true).
        assert!(s.seed_running("web1", "app", true));
        assert!(s.is_seeded("web1", "app"));
        // A later "real=false" must NOT overwrite the seeded value.
        assert!(s.seed_running("web1", "app", false));
    }

    #[test]
    fn set_running_makes_running_and_healthy() {
        let mut s = SimState::default();
        assert!(!s.is_running("web1", "app-new"));
        assert!(!s.is_healthy("web1", "app-new"));
        s.set_running("web1", "app-new", "img:abc");
        assert!(s.is_running("web1", "app-new"));
        assert!(s.is_healthy("web1", "app-new"));
        assert_eq!(s.image_id("web1", "app-new"), "img:abc");
    }

    #[test]
    fn set_stopped_clears_running_and_health() {
        let mut s = SimState::default();
        s.set_running("web1", "app", "img");
        s.set_stopped("web1", "app");
        assert!(!s.is_running("web1", "app"));
        assert!(!s.is_healthy("web1", "app"));
    }

    #[test]
    fn rename_promotes_to_canonical() {
        let mut s = SimState::default();
        s.set_running("web1", "app-new", "img");
        s.rename("web1", "app-new", "app");
        assert!(!s.is_running("web1", "app-new"));
        assert!(s.is_running("web1", "app"));
        assert!(s.is_healthy("web1", "app"));
        // Seeded marker moved to the new name.
        assert!(s.is_seeded("web1", "app"));
        assert!(!s.is_seeded("web1", "app-new"));
    }

    #[test]
    fn remove_deletes_container() {
        let mut s = SimState::default();
        s.set_running("web1", "old", "img");
        s.remove("web1", "old");
        assert!(!s.is_running("web1", "old"));
    }

    #[test]
    fn pick_port_is_deterministic_and_increments() {
        let mut s = SimState::default();
        assert_eq!(s.pick_port("web1", 3000), 13000);
        assert_eq!(s.pick_port("web1", 3000), 13001);
        // Per-host counter — a different host restarts at the base offset.
        assert_eq!(s.pick_port("web2", 3000), 13000);
        assert!(s.port_open("web1", 13000));
        assert!(s.port_open("web1", 13001));
        assert!(!s.port_open("web1", 13002));
    }

    #[test]
    fn proxy_switch_stores_target() {
        let mut s = SimState::default();
        assert_eq!(s.proxy_target("web1", "app"), None);
        s.proxy_switch("web1", "app", "localhost:13000");
        assert_eq!(
            s.proxy_target("web1", "app"),
            Some("localhost:13000".to_string())
        );
        s.proxy_switch("web1", "app", "localhost:13001");
        assert_eq!(
            s.proxy_target("web1", "app"),
            Some("localhost:13001".to_string())
        );
    }
}
