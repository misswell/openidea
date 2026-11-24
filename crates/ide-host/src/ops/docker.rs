//! Docker operations shared with the desktop commands.
use crate::Host;
use crate::rpc::Dispatcher;
use ide_core::{CancellationToken, IdeError, IdeResult, RequestId};
use ide_docker::{DockerEndpoint, DockerRegistry, DockerSettings, RegistryAuth};
use std::collections::BTreeMap;

pub async fn run_env(host: &Host, endpoint: DockerEndpoint) -> IdeResult<BTreeMap<String, String>> {
    endpoint.validate()?;
    host.blocking(move || Ok(ide_docker::run_env(&endpoint))).await
}

async fn cancellable<T: Send + 'static>(
    host: &Host,
    request: RequestId,
    f: impl FnOnce(&CancellationToken) -> IdeResult<T> + Send + 'static,
) -> IdeResult<T> {
    let cancel = host.begin_request(request);
    let result = host.blocking(move || f(&cancel)).await;
    host.end_request(request);
    result
}

pub(crate) fn register(d: &mut Dispatcher) {
    d.add("docker_run_env", |c| async move { run_env(&c.host, c.arg("endpoint")?).await })
        .add("docker_snapshot", |c| async move {
            let ep = c.arg("endpoint")?;
            c.host.blocking(move || Ok(ide_docker::snapshot(&ep))).await
        })
        .add("docker_container_op", |c| async move {
            let (ep, ids, op) = (c.arg("endpoint")?, c.arg::<Vec<String>>("ids")?, c.arg("op")?);
            c.host.blocking(move || ide_docker::container_op(&ep, &ids, op)).await
        })
        .add("docker_remove", |c| async move {
            let (ep, object, ids, force) =
                (c.arg("endpoint")?, c.arg("object")?, c.arg::<Vec<String>>("ids")?, c.arg("force")?);
            c.host.blocking(move || ide_docker::remove(&ep, object, &ids, force)).await
        })
        .add("docker_inspect", |c| async move {
            let (ep, object, id) = (c.arg("endpoint")?, c.arg("object")?, c.arg::<String>("id")?);
            c.host.blocking(move || ide_docker::inspect(&ep, object, &id)).await
        })
        .add("docker_image_history", |c| async move {
            let (ep, id) = (c.arg("endpoint")?, c.arg::<String>("id")?);
            c.host.blocking(move || ide_docker::image_history(&ep, &id)).await
        })
        .add("docker_container_top", |c| async move {
            let (ep, id) = (c.arg("endpoint")?, c.arg::<String>("id")?);
            c.host.blocking(move || ide_docker::container_top(&ep, &id)).await
        })
        .add("docker_container_stats", |c| async move {
            let (ep, id) = (c.arg("endpoint")?, c.arg::<String>("id")?);
            c.host.blocking(move || ide_docker::container_stats(&ep, &id)).await
        })
        .add("docker_logs", |c| async move {
            let (request, ep, id, tail, follow, timestamps) = (
                c.arg("request")?,
                c.arg("endpoint")?,
                c.arg::<String>("id")?,
                c.arg("tail")?,
                c.arg("follow")?,
                c.arg("timestamps")?,
            );
            let emit = c.channel("onLine")?;
            cancellable(&c.host, request, move |cancel| {
                ide_docker::logs(&ep, &id, tail, follow, timestamps, cancel, &|line| emit(line))
            })
            .await
        })
        .add("docker_events", |c| async move {
            let (request, ep) = (c.arg("request")?, c.arg("endpoint")?);
            let emit = c.channel("onEvent")?;
            cancellable(&c.host, request, move |cancel| ide_docker::watch_events(&ep, cancel, &|event| emit(event)))
                .await
        })
        .add("docker_pull", |c| async move {
            let (request, ep, reference) = (c.arg("request")?, c.arg("endpoint")?, c.arg::<String>("reference")?);
            let emit = c.channel("onOutput")?;
            let data_dir = c.host.data_dir.clone();
            cancellable(&c.host, request, move |cancel| {
                let auth = ide_docker::auth_for(&data_dir, &reference);
                ide_docker::pull(&ep, &reference, auth.as_ref(), cancel, &|line| emit(line))
            })
            .await
        })
        .add("docker_prune", |c| async move {
            let (request, ep, target) = (c.arg("request")?, c.arg("endpoint")?, c.arg("target")?);
            let emit = c.channel("onOutput")?;
            cancellable(&c.host, request, move |cancel| ide_docker::prune(&ep, target, cancel, &|line| emit(line)))
                .await
        })
        .add("docker_compose_op", |c| async move {
            let (request, ep, project, files, working_dir, op) = (
                c.arg("request")?,
                c.arg("endpoint")?,
                c.arg::<String>("project")?,
                c.arg::<Vec<String>>("files")?,
                c.arg::<Option<String>>("workingDir")?,
                c.arg("op")?,
            );
            let emit = c.channel("onOutput")?;
            cancellable(&c.host, request, move |cancel| {
                ide_docker::compose_op(&ep, &project, &files, working_dir.as_deref(), op, cancel, &|line| emit(line))
            })
            .await
        })
        .add("docker_settings", |c| async move {
            let data_dir = c.host.data_dir.clone();
            c.host.blocking(move || Ok(ide_docker::load_settings(&data_dir))).await
        })
        .add("docker_save_settings", |c| async move {
            let settings: DockerSettings = c.arg("settings")?;
            let data_dir = c.host.data_dir.clone();
            c.host.blocking(move || ide_docker::save_settings(&data_dir, &settings)).await
        })
        .add("docker_set_registry_password", |c| async move {
            let (id, password) = (c.arg::<String>("id")?, c.arg::<Option<String>>("password")?);
            let data_dir = c.host.data_dir.clone();
            c.host.blocking(move || ide_docker::set_registry_password(&data_dir, &id, password.as_deref())).await
        })
        .add("docker_registry_login", |c| async move {
            let (ep, registry, password) =
                (c.arg("endpoint")?, c.arg::<DockerRegistry>("registry")?, c.arg::<Option<String>>("password")?);
            let data_dir = c.host.data_dir.clone();
            c.host
                .blocking(move || {
                    let password = password
                        .or_else(|| ide_docker::registry_password(&data_dir, &registry.id))
                        .ok_or_else(|| IdeError::invalid("enter the registry password"))?;
                    ide_docker::login(
                        &ep,
                        &RegistryAuth { address: registry.address, username: registry.username, password },
                    )
                })
                .await
        });
}
