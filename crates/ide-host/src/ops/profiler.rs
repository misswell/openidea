//! Launch plans and profiles collected by Run with Profiler.
use crate::Host;
use crate::ops::coverage::KEEP_RUNS;
use crate::rpc::Dispatcher;
use ide_core::{IdeResult, RunId};
use ide_profiler::{Profile, ProfileMode, ProfilePrepared, ProfileSpec};
use ide_run::{RunConfig, RunInfo};

pub async fn prepare(host: &Host, window: &str, config: RunConfig, mode: ProfileMode) -> IdeResult<ProfilePrepared> {
    host.require_trust(window)?;
    let project = host.project(window)?;
    let (root, cache) = (project.root.clone(), project.cache_dir.join("profiles"));
    host.blocking(move || {
        let out = ide_run::wrap::new_output_dir(&cache, KEEP_RUNS)?;
        Ok(ide_profiler::prepare(&root, &config, &out, mode))
    })
    .await
}

pub async fn collect(host: &Host, window: &str, spec: ProfileSpec) -> IdeResult<Profile> {
    host.require_trust(window)?;
    host.blocking(move || ide_profiler::collect(&spec)).await
}

pub async fn gc_before_stop(host: &Host, window: &str, run: RunId) -> IdeResult<u32> {
    host.require_trust(window)?;
    let remote = host.remote.runs.contains(run);
    let Some(pid) = local_pid(remote, &host.processes.list(), run) else {
        return Ok(0);
    };
    gc_process_tree(pid).await
}

fn local_pid(remote: bool, local: &[RunInfo], run: RunId) -> Option<u32> {
    if remote {
        return None;
    }
    local.iter().find(|info| info.id == run && info.running).and_then(|info| info.pid)
}

#[cfg(unix)]
async fn gc_process_tree(pid: u32) -> IdeResult<u32> {
    let Ok(pid) = i32::try_from(pid) else {
        return Ok(0);
    };
    tokio::task::spawn_blocking(move || Ok(ide_profiler::gc_before_stop(&ide_core::process::process_tree(pid))))
        .await
        .map_err(|error| ide_core::IdeError::internal(error.to_string()))?
}

#[cfg(not(unix))]
async fn gc_process_tree(_pid: u32) -> IdeResult<u32> {
    Ok(0)
}

pub(crate) fn register(d: &mut Dispatcher) {
    d.add("profiler_prepare", |c| async move { prepare(&c.host, &c.window, c.arg("config")?, c.arg("mode")?).await });
    d.add("profiler_collect", |c| async move { collect(&c.host, &c.window, c.arg("spec")?).await });
    d.add("profiler_gc_before_stop", |c| async move { gc_before_stop(&c.host, &c.window, c.arg("run")?).await });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: u32, pid: Option<u32>, running: bool) -> RunInfo {
        RunInfo { id: RunId(id), config_id: String::new(), name: String::new(), pid, running, pty: false }
    }

    #[test]
    fn local_pid_only_uses_a_live_local_process() {
        let runs = [info(1, Some(100), true), info(2, Some(200), false), info(3, None, true)];
        assert_eq!(local_pid(false, &runs, RunId(1)), Some(100));
        assert_eq!(local_pid(false, &runs, RunId(2)), None);
        assert_eq!(local_pid(false, &runs, RunId(3)), None);
        assert_eq!(local_pid(true, &runs, RunId(1)), None);
    }
}
