//! Unit tests for `engine::eval`, split by topic; shared fakes live in `helpers`.

mod accessories;
mod deploy;
mod docker_env;
mod docker_prune;
mod dry_run_contract;
mod helpers;
mod hooks;
mod live_golden;
mod locks;
mod proxy_setup;
mod rollback;
mod scripts;
mod ssh;
mod wait_healthy;
