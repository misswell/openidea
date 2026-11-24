//! HTTP Client commands shared by the desktop shell and the remote server.
use crate::rpc::Dispatcher;
use ide_core::{IdeError, IpcPath, RequestId};
use ide_http::{HistoryEntry, HttpExchange};

pub(crate) fn register(d: &mut Dispatcher) {
    d.add("http_requests", |c| async move {
        let text = c.arg::<String>("text")?;
        Ok(ide_http::parse::requests(&text))
    })
    .add("http_environments", |c| async move {
        let path = c.arg::<Option<IpcPath>>("path")?;
        let root = c.host.project(&c.window).ok().map(|project| project.root.clone());
        c.host.blocking(move || ide_http::environments(path.as_ref().map(|path| path.as_ref()), root.as_deref())).await
    })
    .add("http_execute", |c| async move {
        let (request, path, text, index, env) = (
            c.arg::<RequestId>("request")?,
            c.arg::<Option<IpcPath>>("path")?,
            c.arg::<String>("text")?,
            c.arg::<u32>("index")?,
            c.arg::<Option<String>>("env")?,
        );
        let project = c.host.project(&c.window)?;
        let cancel = c.host.begin_request(request);
        let input = ide_http::RunInput {
            text: &text,
            index,
            file: path.as_ref().map(|path| path.as_ref()),
            root: Some(project.root.as_path()),
            env: env.as_deref(),
        };
        let result = ide_http::run(input, &cancel).await;
        c.host.end_request(request);
        let exchange = result?;
        let saved = exchange.clone();
        let history = project.http_history.clone();
        c.host.blocking(move || history.add(&saved)).await?;
        Ok::<_, IdeError>(exchange)
    })
    .add("http_history", |c| async move {
        Ok::<Vec<HistoryEntry>, IdeError>(c.host.project(&c.window)?.http_history.list())
    })
    .add("http_history_get", |c| async move {
        let id = c.arg::<String>("id")?;
        Ok::<HttpExchange, IdeError>(c.host.project(&c.window)?.http_history.get(&id)?)
    })
    .add("http_history_clear", |c| async move {
        c.host.project(&c.window)?.http_history.clear()?;
        Ok::<(), IdeError>(())
    })
    .add("http_curl", |c| async move {
        let (path, text, index, env) = (
            c.arg::<Option<IpcPath>>("path")?,
            c.arg::<String>("text")?,
            c.arg::<u32>("index")?,
            c.arg::<Option<String>>("env")?,
        );
        let root = c.host.project(&c.window).ok().map(|project| project.root.clone());
        c.host
            .blocking(move || {
                ide_http::curl(&ide_http::RunInput {
                    text: &text,
                    index,
                    file: path.as_ref().map(|path| path.as_ref()),
                    root: root.as_deref(),
                    env: env.as_deref(),
                })
            })
            .await
    })
    .add("http_from_curl", |c| async move {
        let command = c.arg::<String>("command")?;
        Ok::<Option<String>, IdeError>(ide_http::curl::from_curl(&command))
    })
    .add("http_environment_file", |c| async move {
        let (path, private) = (c.arg::<IpcPath>("path")?, c.arg::<bool>("private")?);
        let root = c.host.project(&c.window).ok().map(|project| project.root.clone());
        c.host
            .blocking(move || {
                ide_http::env::environment_file(path.as_ref(), root.as_deref(), private).map(IpcPath::from)
            })
            .await
    })
    .add("http_variable_definitions", |c| async move {
        let (path, name) = (c.arg::<IpcPath>("path")?, c.arg::<String>("name")?);
        let root = c.host.project(&c.window).ok().map(|project| project.root.clone());
        c.host.blocking(move || Ok(ide_http::env::variable_definitions(path.as_ref(), root.as_deref(), &name))).await
    })
    .add("spring_route_sources", |c| async move { crate::ops::routes::spring_route_sources(&c.host, &c.window) });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_parser_registers_a_remote_command() {
        let mut dispatcher = Dispatcher::default();
        register(&mut dispatcher);
        let commands = dispatcher.commands();
        assert!(commands.contains(&"http_execute"));
        assert!(commands.contains(&"http_history"));
        assert!(commands.contains(&"spring_route_sources"));
    }
}
