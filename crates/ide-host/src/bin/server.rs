//! `openidea-server --stdio`: the IDE backend for remote development, serving
//! one desktop client over stdin / stdout (started by the client over SSH).

use ide_host::plugins::remote::{RemotePluginLanguages, SYNC_COMMAND};
use ide_host::plugins::remote_host::RemotePlugins;
use ide_host::rpc::{self, Dispatcher};
use ide_host::{Host, HostConfig};
use ide_language::LanguageRegistry;
use parking_lot::Mutex;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

struct McpService {
    host: Arc<Host>,
    data_dir: PathBuf,
    server: Mutex<Option<ide_mcp::McpServer>>,
    error: Mutex<Option<String>>,
}

impl McpService {
    fn new(host: Arc<Host>, data_dir: PathBuf) -> Self {
        Self { host, data_dir, server: Mutex::new(None), error: Mutex::new(None) }
    }

    fn apply(&self) {
        let enabled = self.host.global_settings.effective().mcp_server.enabled;
        let mut server = self.server.lock();
        if !enabled {
            if let Some(running) = server.take() {
                running.stop();
            }
            self.host.set_mcp_executable(None);
            *self.error.lock() = None;
            return;
        }
        if server.is_some() {
            return;
        }
        match ide_mcp::McpServer::start(
            Arc::new(ide_host::mcp::Registry::new(self.host.clone())),
            Some(ide_mcp::discovery_path(&self.data_dir)),
            &self.host.runtime,
        ) {
            Ok(started) => {
                self.host.set_mcp_executable(std::env::current_exe().ok());
                *server = Some(started);
                *self.error.lock() = None;
            }
            Err(e) => {
                tracing::warn!("MCP server: {e}");
                *self.error.lock() = Some(e.to_string());
            }
        }
    }

    fn status(&self, window: &str) -> McpServiceStatus {
        let root = self.host.project(window).ok().map(|project| project.root.clone());
        let server = self.server.lock();
        let exe = std::env::current_exe().unwrap_or_default();
        McpServiceStatus {
            running: server.is_some(),
            error: self.error.lock().clone(),
            port: server.as_ref().map(ide_mcp::McpServer::port),
            project: root.clone().map(ide_core::IpcPath::from),
            url: server.as_ref().zip(root.as_ref()).map(|(server, root)| server.url(root)),
            token: server.as_ref().map(|server| server.token().to_owned()),
            stdio_command: root.as_ref().map(|root| ide_mcp::stdio_command(&exe, root)).unwrap_or_default(),
        }
    }

    fn stop(&self) {
        self.host.set_mcp_executable(None);
        if let Some(server) = self.server.lock().take() {
            server.stop();
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct McpServiceStatus {
    running: bool,
    error: Option<String>,
    port: Option<u16>,
    project: Option<ide_core::IpcPath>,
    url: Option<String>,
    token: Option<String>,
    stdio_command: Vec<String>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version") {
        println!("openidea-server {} (protocol {})", env!("CARGO_PKG_VERSION"), rpc::PROTOCOL_VERSION);
        return;
    }
    if args.iter().any(|a| a == ide_mcp::STDIO_FLAG) {
        let home = dirs::home_dir().unwrap_or_default();
        let base =
            std::env::var_os("OPENIDEA_SERVER_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".openidea"));
        let mut process_args = vec!["openidea-server".to_owned()];
        process_args.extend(args.iter().cloned());
        let project = ide_mcp::stdio_request(&process_args).flatten();
        let discovery = ide_mcp::discovery_path(&base.join("data"));
        std::process::exit(ide_mcp::stdio::run(&discovery, project));
    }
    if args.first().is_some_and(|a| a == "--connect") {
        // One forwarded connection (see `ide_host::relay`): stdout carries its bytes.
        let code = match ide_host::relay::parse_args(&args[1..]).and_then(|(host, port)| {
            ide_host::relay::relay(&host, port, std::io::stdin(), std::io::stdout().lock())
                .map_err(|e| format!("cannot connect to {host}:{port}: {e}"))
        }) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{e}");
                1
            }
        };
        std::process::exit(code);
    }
    if !args.iter().any(|a| a == "--stdio") {
        eprintln!("usage: openidea-server --stdio | --mcp-stdio | --connect <host> <port> | --version");
        std::process::exit(2);
    }
    // stdout carries the protocol: logs go to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("OPENIDEA_LOG").unwrap_or_else(|_| "info".into()))
        .init();
    let home = dirs::home_dir().unwrap_or_default();
    let base = std::env::var_os("OPENIDEA_SERVER_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".openidea"));
    // An SSH exec channel gets a minimal environment: take the login shell's, read
    // afresh on every start, plus the IDE-managed tools. Before any other thread starts.
    ide_host::shell_env::inherit_login_shell_env("openidea-server");
    let data_dir = base.join("data");
    ide_tools::extend_process_path(&data_dir);
    ide_dap::adapter::set_tools_root(ide_tools::tools_dir(&data_dir));
    ide_sdk::java::add_jdk_root(&ide_tools::jdk::jdks_dir(&data_dir));
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let code = runtime.block_on(run(base));
    std::process::exit(code);
}

async fn run(base: PathBuf) -> i32 {
    let (config_dir, data_dir) = (base.join("config"), base.join("data"));
    for dir in [&config_dir, &data_dir] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("cannot create {}: {e}", dir.display());
            return 1;
        }
    }
    let languages = match LanguageRegistry::load(Some(&config_dir.join("languages"))) {
        Ok(l) => Arc::new(l),
        Err(e) => {
            eprintln!("language definitions: {e}");
            return 1;
        }
    };
    // The desktop's plugin languages (`plugin_sync_languages`); programs pinned on this machine.
    let plugin_languages = RemotePluginLanguages::new(Some(&data_dir));
    languages.attach_plugins(plugin_languages.store().clone());
    let peer = Arc::new(OnceLock::new());
    let sink_peer = peer.clone();
    let plugins_data = data_dir.clone();
    let mcp_data = data_dir.clone();
    let host = Arc::new(Host::new(HostConfig {
        config_dir,
        local_data_dir: data_dir.clone(),
        data_dir,
        languages,
        syntax: Arc::default(),
        sink_for: Box::new(move |window| rpc::event_sink(sink_peer.clone(), Some(window.to_owned()))),
        owner: |w| w,
        runtime: tokio::runtime::Handle::current(),
        project_environment: true,
    }));
    // The desktop's code plugins run here, next to the project (`plugin_sync_packages`).
    let plugins = match RemotePlugins::start(&plugins_data, &host, peer.clone()) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!("plugins are unavailable: {e}");
            None
        }
    };
    if let Some(root) = std::env::var_os(ide_host::trust::TRUSTED_ROOT_ENV)
        && let Err(e) = host.trust.inherit(std::path::Path::new(&root))
    {
        tracing::warn!("cannot trust {}: {e}", root.to_string_lossy());
    }
    tracing::info!("openidea-server {} ready", env!("CARGO_PKG_VERSION"));
    let mcp = Arc::new(McpService::new(host.clone(), mcp_data));
    mcp.apply();
    let mut dispatcher = Dispatcher::standard();
    if let Some(plugins) = &plugins {
        plugins.register(&mut dispatcher);
    }
    dispatcher.add(SYNC_COMMAND, move |c| {
        let remote = plugin_languages.clone();
        async move {
            let report = remote.sync(c.arg("languages")?);
            c.host.plugin_languages_changed();
            Ok(report)
        }
    });
    let settings_mcp = mcp.clone();
    dispatcher.add("settings_set", move |c| {
        let mcp = settings_mcp.clone();
        async move {
            let key: String = c.arg("key")?;
            let settings = rpc::set_settings(&c)?;
            if key == "mcpServer" || key.starts_with("mcpServer.") {
                mcp.apply();
            }
            Ok(settings)
        }
    });
    let status_mcp = mcp.clone();
    dispatcher.add("mcp_server_status", move |c| {
        let mcp = status_mcp.clone();
        async move { Ok(mcp.status(&c.window)) }
    });
    rpc::serve(host.clone(), dispatcher, peer, tokio::io::stdin(), tokio::io::stdout()).await;
    mcp.stop();
    if let Some(plugins) = &plugins {
        plugins.host().shutdown();
    }
    // `process::exit` skips destructors: nothing started for the client may outlive it.
    if tokio::time::timeout(std::time::Duration::from_secs(10), host.shutdown()).await.is_err() {
        tracing::warn!("language servers did not shut down in time");
    }
    0
}
