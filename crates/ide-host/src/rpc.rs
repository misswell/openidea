//! Remote development protocol: JSON-RPC 2.0 (`Content-Length` framing) between
//! the desktop app and `openidea-server`, over the server's stdio (an SSH exec
//! channel).
//!
//! * client → server request `initialize` `{ version }` → [`ServerInfo`];
//! * client → server request `invoke` `{ window, cmd, args }` → the command's
//!   result, `args` as the desktop command takes them (camelCase names). A
//!   streaming argument is `{ "$channel": <id> }`;
//! * server → client notification `event` `{ window, event }` (an `IdeEvent`
//!   for that window, or every window when `window` is null);
//! * server → client notification `channel` `{ id, data }`: one value of a
//!   streaming argument;
//! * server → client notification `plugin` `{ window, event }` and request
//!   `plugin_secret` `{ plugin, name }` → `string | null`: code plugins of the
//!   remote project (`plugins::remote_host`).

use crate::Host;
use crate::ops::{self, Emit};
use ide_core::{IdeError, IdeEvent, IdeResult, RequestId, TaskId};
use ide_jsonrpc::{Handler, Peer, codes};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub version: String,
    pub protocol: u32,
    pub os: String,
    pub arch: String,
    pub home: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invoke {
    pub window: String,
    pub cmd: String,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMessage {
    pub window: Option<String>,
    pub event: IdeEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMessage {
    pub id: u64,
    pub data: Value,
}

/// One `invoke`: its arguments and a way to stream values back.
pub struct Call {
    pub host: Arc<Host>,
    pub window: String,
    pub args: Value,
    peer: Arc<OnceLock<Peer>>,
}

impl Call {
    /// Argument `name` (missing = `null`, so `Option`s may be omitted).
    pub fn arg<T: DeserializeOwned>(&self, name: &str) -> IdeResult<T> {
        let v = self.args.get(name).cloned().unwrap_or(Value::Null);
        serde_json::from_value(v).map_err(|e| IdeError::invalid(format!("argument {name}: {e}")))
    }

    /// Streaming argument `name` as an [`ops::Channel`].
    pub fn sender<T: Serialize + 'static>(&self, name: &str) -> IdeResult<ops::Channel<T>> {
        let emit = self.channel(name)?;
        Ok(ops::Channel::new(move |v| emit(v)))
    }

    /// Streaming argument `name` (`{ "$channel": id }`).
    pub fn channel<T: Serialize + 'static>(&self, name: &str) -> IdeResult<Emit<T>> {
        let id = self
            .args
            .get(name)
            .and_then(|v| v.get("$channel"))
            .and_then(Value::as_u64)
            .ok_or_else(|| IdeError::invalid(format!("argument {name}: expected a channel")))?;
        let peer = self.peer.clone();
        Ok(Arc::new(move |data: T| {
            let Some(peer) = peer.get() else { return };
            match serde_json::to_value(&data) {
                Ok(data) => {
                    let _ = peer.notify("channel", ChannelMessage { id, data });
                }
                Err(e) => tracing::warn!("channel {id}: {e}"),
            }
        }))
    }
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type HandlerFn = Arc<dyn Fn(Call) -> BoxFuture<IdeResult<Value>> + Send + Sync>;

/// Commands the server answers, by name.
#[derive(Default, Clone)]
pub struct Dispatcher {
    handlers: HashMap<&'static str, HandlerFn>,
}

impl Dispatcher {
    pub fn add<F, Fut, T>(&mut self, name: &'static str, f: F) -> &mut Self
    where
        F: Fn(Call) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = IdeResult<T>> + Send + 'static,
        T: Serialize,
    {
        let f = Arc::new(f);
        self.handlers.insert(
            name,
            Arc::new(move |call| {
                let fut = f(call);
                Box::pin(async move {
                    let value = fut.await?;
                    serde_json::to_value(value).map_err(|e| IdeError::internal(e.to_string()))
                })
            }),
        );
        self
    }

    pub fn commands(&self) -> Vec<&'static str> {
        let mut out: Vec<_> = self.handlers.keys().copied().collect();
        out.sort_unstable();
        out
    }

    /// The commands of the core remote slice: project, files, documents, search,
    /// terminals, runs and VCS status.
    pub fn standard() -> Self {
        let mut d = Self::default();
        d.add("project_trust_status", |c| async move { c.host.trust.status(&c.arg::<PathBuf>("path")?) })
            .add("project_trust_list", |c| async move { c.host.trust.decisions() })
            .add("project_trust_add_location", |c| async move {
                c.host.trust.add_location(&c.arg::<PathBuf>("path")?)?;
                c.host.refresh_project_trust().await
            })
            .add("project_trust_set", |c| async move {
                c.host.set_project_trust(&c.arg::<PathBuf>("path")?, c.arg("trusted")?, c.arg("folder")?).await
            })
            .add("project_trust_remove_location", |c| async move {
                c.host.trust.remove_location(&c.arg::<PathBuf>("path")?)?;
                c.host.refresh_project_trust().await
            });
        d.add("open_project", |c| async move {
            let path: PathBuf = c.arg("path")?;
            let project = ops::project::open(&c.host, &c.window, &path).await?;
            Ok(ops::project::info(&project))
        })
        .add("project_current", |c| async move { Ok(ops::project::current(&c.host, &c.window)) })
        .add("close_project", |c| async move {
            c.host.close_project(&c.window);
            Ok(())
        })
        .add("jvm_file_presentations", |c| async move {
            ops::jvm_icons::jvm_file_presentations(&c.host, &c.window, c.arg("paths")?).await
        })
        .add("project_roots", |c| async move { ops::project::roots(&c.host, &c.window) })
        .add("workspace_create", |c| async move {
            let path: PathBuf = c.arg("path")?;
            let name: String = c.arg("name")?;
            let projects: Vec<PathBuf> = c.arg("projects")?;
            c.host
                .blocking(move || {
                    ide_workspace::workspace::Workspace::save(&path, &name, projects, true)?;
                    Ok(ide_core::IpcPath::from(dunce::canonicalize(path)?))
                })
                .await
        })
        .add("workspace_update", |c| async move {
            let host = c.host.clone();
            let window = c.window.clone();
            let name: String = c.arg("name")?;
            let projects: Vec<PathBuf> = c.arg("projects")?;
            let worker_host = host.clone();
            host.blocking(move || ops::project::save_workspace(&worker_host, &window, &name, projects)).await
        })
        .add("workspace_attach", |c| async move {
            let host = c.host.clone();
            let window = c.window.clone();
            let path: PathBuf = c.arg("path")?;
            let worker_host = host.clone();
            host.blocking(move || ops::project::attach_workspace(&worker_host, &window, &path)).await
        })
        .add("workspace_discover_projects", |c| async move {
            let request = c.arg::<RequestId>("request")?;
            let folders: Vec<PathBuf> = c.arg("folders")?;
            let cancel = c.host.begin_request(request);
            let result =
                tokio::task::spawn_blocking(move || ide_workspace::project_discovery::discover(folders, &cancel))
                    .await
                    .map_err(|e| IdeError::internal(e.to_string()))
                    .and_then(|result| result);
            c.host.end_request(request);
            result
        })
        .add("settings_get", |c| async move { Ok(ops::project::settings(&c.host, &c.window)) })
        .add("settings_get_global", |c| async move { Ok::<_, IdeError>(c.host.global_settings.effective()) })
        .add("settings_export", |c| async move { export_settings(&c).await })
        .add("settings_import", |c| async move { import_settings(&c).await })
        .add("settings_list_jetbrains_ides", |c| async move { c.host.blocking(jetbrains_settings_choices).await })
        .add("settings_read_jetbrains", |c| async move {
            let path: PathBuf = c.arg("path")?;
            c.host.blocking(move || ide_workspace::settings::read_jetbrains_settings(&path)).await
        })
        .add("settings_set", |c| async move { set_settings(&c) })
        .add("cancel_request", |c| async move {
            c.host.cancel_request(c.arg::<RequestId>("id")?);
            Ok(())
        })
        .add("cancel_task", |c| async move { Ok(ide_core::task::cancel(c.arg::<TaskId>("id")?)) })
        .add("fs_read_dir", |c| async move { ops::fs::read_dir(&c.host, &c.window, c.arg("path")?).await })
        .add("fs_entry_kind", |c| async move { ops::fs::entry_kind(&c.host, c.arg("path")?).await })
        .add("fs_create_file", |c| async move { ops::fs::create_file(&c.host, &c.window, c.arg("path")?) })
        .add("fs_create_file_bytes", |c| async move {
            ops::fs::create_file_bytes(&c.host, &c.window, c.arg("path")?, c.arg("data")?).await
        })
        .add("fs_create_dir", |c| async move { ide_vfs::create_dir(&c.arg::<PathBuf>("path")?) })
        .add("fs_create_dir_exclusive", |c| async move { ops::fs::create_dir_exclusive(&c.host, c.arg("path")?).await })
        .add("fs_append_file_bytes", |c| async move {
            ops::fs::append_file_bytes(&c.host, c.arg("path")?, c.arg("data")?, c.arg("request")?).await
        })
        .add("fs_create_symlink", |c| async move {
            ops::fs::create_symlink(&c.host, c.arg("path")?, c.arg("target")?).await
        })
        .add("fs_set_permissions", |c| async move {
            ops::fs::set_permissions(&c.host, c.arg("path")?, c.arg("mode")?).await
        })
        .add("fs_remove_file", |c| async move { ops::fs::remove_file(&c.host, c.arg("path")?).await })
        .add("fs_remove_tree", |c| async move { ops::fs::remove_tree(&c.host, c.arg("path")?).await })
        .add("fs_rename", |c| async move { ops::fs::rename(&c.host, &c.window, c.arg("from")?, c.arg("to")?).await })
        .add("fs_set_writable", |c| async move {
            ops::fs::set_writable(&c.host, &c.window, c.arg("path")?, c.arg("writable")?)
        })
        .add("fs_delete", |c| async move { ops::fs::delete(&c.host, &c.window, c.arg("path")?).await })
        .add("fs_copy", |c| async move { ops::fs::copy(&c.host, &c.window, c.arg("from")?, c.arg("to")?).await })
        .add("fs_copy_merge", |c| async move {
            ops::fs::copy_merge(
                &c.host,
                &c.window,
                c.arg("sourceRoot")?,
                c.arg("destinationRoot")?,
                c.arg("path")?,
                c.arg("overwrite")?,
            )
            .await
        })
        .add("fs_compare_directories", |c| async move {
            ops::fs::compare_directories(
                &c.host,
                &c.window,
                c.arg("left")?,
                c.arg("right")?,
                c.arg("request")?,
                c.sender("onProgress")?,
            )
            .await
        })
        .add("fs_read_text", |c| async move { ops::fs::read_text(&c.host, &c.window, c.arg("path")?).await })
        .add("plugin_workspace_read_text", |c| async move {
            ops::fs::plugin_workspace_read_text(&c.host, c.arg("root")?, c.arg("path")?, c.arg("request")?).await
        })
        .add("plugin_workspace_stat", |c| async move {
            ops::fs::plugin_workspace_stat(&c.host, c.arg("root")?, c.arg("path")?, c.arg("request")?).await
        })
        .add("plugin_workspace_write_text", |c| async move {
            ops::fs::plugin_workspace_write_text(
                &c.host,
                c.arg("root")?,
                c.arg("path")?,
                c.arg("text")?,
                c.arg("request")?,
            )
            .await
        })
        .add("plugin_workspace_find_files", |c| async move {
            ops::fs::plugin_workspace_find_files(
                &c.host,
                &c.window,
                c.arg("root")?,
                c.arg("glob")?,
                c.arg("limit")?,
                c.arg("request")?,
            )
            .await
        })
        .add("doc_open", |c| async move { ops::doc::open(&c.host, &c.window, c.arg("path")?).await })
        .add("doc_close", |c| async move {
            ops::doc::blocking(move || ops::doc::close(&c.host, &c.window, c.arg("doc")?)).await
        })
        .add("doc_edit", |c| async move {
            ops::doc::blocking(move || {
                ops::doc::edit(&c.host, &c.window, c.arg("doc")?, c.arg("baseVersion")?, c.arg("changes")?)
            })
            .await
        })
        .add("doc_save", |c| async move { ops::doc::save(&c.host, &c.window, c.arg("doc")?).await })
        .add("doc_save_all", |c| async move { ops::doc::save_all(&c.host, &c.window).await })
        .add("doc_sync_from_disk", |c| async move { ops::doc::sync_from_disk(&c.host, &c.window).await })
        .add("doc_snapshot", |c| async move { ops::doc::snapshot(&c.host, &c.window, c.arg("doc")?).await })
        .add("search_start", |c| async move {
            let on_event = c.channel("onEvent")?;
            ops::search::start(&c.host, &c.window, c.arg("request")?, c.arg("query")?, on_event).await
        })
        .add("term_spawn", |c| async move {
            let on_output = c.channel("onOutput")?;
            ops::term::spawn(&c.host, &c.window, c.arg("options")?, on_output)
        })
        .add("ssh_open_terminal", |c| async move {
            let on_output = c.channel("onOutput")?;
            let id: String = c.arg("id")?;
            ops::term::spawn_ssh(&c.host, &c.window, &id, c.arg("cols")?, c.arg("rows")?, on_output).await
        })
        .add("term_store_image", |c| async move {
            ops::term::store_image(&c.host, c.arg("id")?, c.arg("data")?, c.arg("mime")?).await
        })
        .add("term_write", |c| async move { ops::term::write(&c.host, c.arg("id")?, &c.arg::<String>("data")?) })
        .add("term_resize", |c| async move { ops::term::resize(&c.host, c.arg("id")?, c.arg("cols")?, c.arg("rows")?) })
        .add("term_kill", |c| async move { ops::term::kill(&c.host, c.arg("id")?) })
        .add("term_is_busy", |c| async move { ops::term::is_busy(&c.host, c.arg("id")?) })
        .add("term_default_shell", |_| async move { Ok(ide_terminal::default_shell_path()) })
        .add("term_available_shells", |_| async move { Ok(ide_terminal::available_shells()) })
        .add("run_configs", |c| async move { ops::doc::blocking(move || ops::run::configs(&c.host, &c.window)).await })
        .add("run_save_configs", |c| async move {
            ops::run::save_configs(
                &c.host,
                &c.window,
                &c.arg::<Vec<ide_run::RunConfig>>("configs")?,
                &c.arg::<Option<Vec<String>>>("removedDetectedIds")?.unwrap_or_default(),
            )
        })
        .add("run_without_derived", |c| async move { ops::run::without_derived(&c.host, &c.window, c.arg("configs")?) })
        .add("run_apply_settings", |c| async move { ops::run::apply_settings(&c.host, &c.window, c.arg("configs")?) })
        .add("run_system_env", |_| async move { Ok(ide_run::system_env()) })
        .add("run_start", |c| async move {
            let on_output = c.channel("onOutput")?;
            ops::run::start_request(&c.host, &c.window, c.arg("config")?, on_output, c.arg("request")?).await
        })
        .add("run_stop", |c| async move { ops::run::stop(&c.host, &c.window, c.arg("id")?) })
        .add("web_url", |c| async move { ops::web::url(&c.host, &c.window, &c.arg::<PathBuf>("path")?).await })
        .add("run_stdin", |c| async move { ops::run::stdin(&c.host, c.arg("id")?, &c.arg::<String>("data")?) })
        .add("run_resize", |c| async move { ops::run::resize(&c.host, c.arg("id")?, c.arg("cols")?, c.arg("rows")?) })
        .add("run_close_stdin", |c| async move { ops::run::close_stdin(&c.host, c.arg("id")?) })
        .add("run_list", |c| async move { Ok(ops::run::list(&c.host)) })
        .add("mcp_ui_ready", |c| async move {
            c.host.project(&c.window)?;
            c.host.mcp_requests.set_ready(&c.window, c.arg("ready")?);
            Ok(())
        })
        .add("mcp_respond", |c| async move {
            c.host.project(&c.window)?;
            c.host.mcp_requests.answer(&c.window, c.arg("id")?, c.arg("allow")?, c.arg("error")?)
        })
        .add("mcp_agent_run_closed", |c| async move {
            let project = c.host.project(&c.window)?;
            let name = c.arg::<String>("name")?;
            c.host.mcp_agent_runs.closed(&project.root, &name);
            Ok(())
        })
        .add("mcp_set_open_editors", |c| async move {
            c.host.project(&c.window)?;
            let paths = c.arg::<Vec<ide_core::IpcPath>>("paths")?;
            c.host.mcp_reports.set_open_editors(&c.window, paths.into_iter().map(PathBuf::from).collect());
            Ok(())
        })
        .add("mcp_set_focused_editor", |c| async move {
            c.host.project(&c.window)?;
            c.host.mcp_reports.set_focused_editor(
                &c.window,
                c.arg::<Option<crate::mcp::FocusedEditor>>("editor")?,
                c.arg("focused")?,
            );
            Ok(())
        })
        .add("mcp_report_problems", |c| async move {
            c.host.project(&c.window)?;
            c.host.mcp_reports.report_problems(
                &c.window,
                c.arg("reset")?,
                c.arg::<Vec<crate::mcp::FileProblems>>("files")?,
            );
            Ok(())
        })
        .add("vcs_status", |c| async move { ops::vcs::status(&c.host, &c.window).await });
        ops::register_all(&mut d);
        d
    }
}

/// Writes one settings key and tells the window's clients what changed.
pub fn set_settings(c: &Call) -> IdeResult<ide_core::settings::Settings> {
    let key: String = c.arg("key")?;
    let scope = c.arg("scope")?;
    let applied = ops::project::set_settings(&c.host, &c.window, scope, &key, &c.arg::<String>("value")?)?;
    let sink = c.host.window_sink(&c.window);
    sink.emit(IdeEvent::SettingsChanged { key });
    if let Some(root) = applied.roots_changed {
        sink.emit(IdeEvent::ProjectRootsChanged { root });
    }
    Ok(applied.settings)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsImportResult {
    settings: ide_core::settings::Settings,
    storage: BTreeMap<String, String>,
}

async fn export_settings(c: &Call) -> IdeResult<()> {
    let path: PathBuf = c.arg("path")?;
    let storage: BTreeMap<String, String> = c.arg("storage")?;
    let host = c.host.clone();
    c.host.blocking(move || host.global_settings.export_bundle(&path, storage)).await
}

async fn import_settings(c: &Call) -> IdeResult<SettingsImportResult> {
    let path: PathBuf = c.arg("path")?;
    let host = c.host.clone();
    c.host
        .blocking(move || {
            let (settings, storage) = host.global_settings.import_bundle(&path)?;
            Ok(SettingsImportResult { settings, storage })
        })
        .await
}

#[derive(Debug, Serialize)]
struct JetBrainsSettingsChoice {
    ide: String,
    path: PathBuf,
}

fn jetbrains_settings_choices() -> IdeResult<Vec<JetBrainsSettingsChoice>> {
    let mut roots = Vec::new();
    if cfg!(target_os = "macos") {
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join("Library/Application Support/JetBrains"));
        }
    } else if cfg!(windows) {
        if let Some(app_data) = std::env::var_os("APPDATA") {
            roots.push(PathBuf::from(app_data).join("JetBrains"));
        }
    } else {
        if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
            roots.push(PathBuf::from(config).join("JetBrains"));
        } else if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join(".config/JetBrains"));
        }
    }

    let mut choices = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_jetbrains_settings_folder(&path) {
                continue;
            }
            choices.push(JetBrainsSettingsChoice { ide: entry.file_name().to_string_lossy().into_owned(), path });
        }
    }
    choices.sort_by(|a, b| a.ide.cmp(&b.ide));
    Ok(choices)
}

fn is_jetbrains_settings_folder(path: &Path) -> bool {
    let options = path.join("options");
    path.is_dir() && options.is_dir()
}

/// Serves `host` over `reader` / `writer` until the client disconnects.
/// `peer` must be the one whose events `host` sends ([`event_sink`]).
pub async fn serve<R, W>(host: Arc<Host>, dispatcher: Dispatcher, peer: Arc<OnceLock<Peer>>, reader: R, writer: W)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let handler = Arc::new(ServerHandler { host: host.clone(), dispatcher, peer: peer.clone() });
    let p = Peer::connect(reader, writer, handler);
    let _ = peer.set(p.clone());
    p.closed().await;
    // Reports belong to the disconnected desktop, even while the host shuts down.
    for window in host.projects.read().keys() {
        host.mcp_reports.forget_window(window);
    }
}

/// Sends a window's events to the client as `event` notifications.
pub fn event_sink(peer: Arc<OnceLock<Peer>>, window: Option<String>) -> ide_core::SharedSink {
    Arc::new(PeerSink { peer, window })
}

/// Whether a desktop shows an `event` notification of its server in the window. A server reports
/// on its project; what this machine answers or owns itself (MCP approvals, app updates, window
/// groups) never comes from it.
pub fn client_accepts(event: &IdeEvent) -> bool {
    match event {
        IdeEvent::McpRequest { .. }
        | IdeEvent::McpRequestClosed { .. }
        | IdeEvent::WindowGroupsChanged
        | IdeEvent::WindowFullscreenChanged
        | IdeEvent::UpdateStatusChanged { .. }
        | IdeEvent::UpdateAvailable { .. } => false,
        IdeEvent::DbSourcesChanged
        | IdeEvent::ProjectTrustChanged { .. }
        | IdeEvent::FsChanged { .. }
        | IdeEvent::ExcludedDirsChanged { .. }
        | IdeEvent::DocPatched { .. }
        | IdeEvent::DocDirty { .. }
        | IdeEvent::WorkspaceEditApplied { .. }
        | IdeEvent::DocLanguage { .. }
        | IdeEvent::Diagnostics { .. }
        | IdeEvent::Task { .. }
        | IdeEvent::IndexState { .. }
        | IdeEvent::VcsChanged { .. }
        | IdeEvent::LanguageServer { .. }
        | IdeEvent::LanguageServerWarning { .. }
        | IdeEvent::LanguageServerRemoved { .. }
        | IdeEvent::LspRefresh { .. }
        | IdeEvent::TerminalExited { .. }
        | IdeEvent::RunExited { .. }
        | IdeEvent::Debug { .. }
        | IdeEvent::Notify { .. }
        | IdeEvent::SettingsChanged { .. }
        | IdeEvent::ProjectRootsChanged { .. }
        | IdeEvent::Askpass { .. }
        | IdeEvent::AskpassDone { .. }
        | IdeEvent::NotebookKernel { .. } => true,
    }
}

/// The commands a desktop routes to its server: what `openidea-server` of this build answers,
/// fixed here rather than taken from the server, so a server cannot move a command to this machine.
pub fn client_commands() -> Vec<&'static str> {
    let mut out = Dispatcher::standard().commands();
    out.extend(crate::plugins::remote_host::COMMANDS);
    out.push(crate::plugins::remote::SYNC_COMMAND);
    out.sort_unstable();
    out.dedup();
    out
}

struct PeerSink {
    peer: Arc<OnceLock<Peer>>,
    window: Option<String>,
}

impl ide_core::EventSink for PeerSink {
    fn emit(&self, event: IdeEvent) {
        if let Some(peer) = self.peer.get() {
            let _ = peer.notify("event", EventMessage { window: self.window.clone(), event });
        }
    }
}

struct ServerHandler {
    host: Arc<Host>,
    dispatcher: Dispatcher,
    peer: Arc<OnceLock<Peer>>,
}

pub fn server_info() -> ServerInfo {
    ServerInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: PROTOCOL_VERSION,
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        home: dirs::home_dir().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default(),
    }
}

impl Handler for ServerHandler {
    fn on_notification(&self, _method: &str, _params: Value) {}

    fn on_request(&self, method: &str, params: Value) -> Result<Value, (i64, String)> {
        let internal = |e: serde_json::Error| (codes::INTERNAL_ERROR, e.to_string());
        match method {
            "initialize" => serde_json::to_value(server_info()).map_err(internal),
            "commands" => serde_json::to_value(self.dispatcher.commands()).map_err(internal),
            "invoke" => {
                let call: Invoke =
                    serde_json::from_value(params).map_err(|e| (codes::INVALID_PARAMS, e.to_string()))?;
                let Some(handler) = self.dispatcher.handlers.get(call.cmd.as_str()).cloned() else {
                    return Err((codes::METHOD_NOT_FOUND, format!("command {} is not available remotely", call.cmd)));
                };
                self.host.authorize_command(&call.window, &call.cmd).map_err(error_reply)?;
                let c = Call { host: self.host.clone(), window: call.window, args: call.args, peer: self.peer.clone() };
                // Requests run on blocking-capable threads: wait for the command here.
                self.host.runtime.block_on(handler(c)).map_err(error_reply)
            }
            _ => Err((codes::METHOD_NOT_FOUND, format!("unknown method {method}"))),
        }
    }
}

/// An `IdeError` as a JSON-RPC error: the JSON of the error is the message.
fn error_reply(e: IdeError) -> (i64, String) {
    (codes::INTERNAL_ERROR, serde_json::to_string(&e).unwrap_or(e.message))
}

/// The `IdeError` of an `invoke` reply (see [`error_reply`]).
pub fn error_from_reply(e: ide_jsonrpc::RpcError) -> IdeError {
    match e {
        ide_jsonrpc::RpcError::Server { message, .. } => serde_json::from_str(&message)
            .unwrap_or_else(|_| IdeError::new(ide_core::error::ErrorKind::Protocol, message)),
        other => other.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_core::event::{AskpassKind, McpUiRequest, NotifyLevel};

    #[test]
    fn a_server_cannot_raise_desktop_prompts() {
        let confirm = McpUiRequest::Confirm {
            client: "agent".into(),
            tool: "run".into(),
            subject: "rm -rf".into(),
            project: "/srv/app".into(),
            detail: None,
        };
        let update = ide_core::updates::UpdateInfo {
            version: "9.9.9".into(),
            current_version: "0.1.0".into(),
            notes: None,
            date: None,
        };
        for event in [
            IdeEvent::McpRequest { id: 1, request: confirm },
            IdeEvent::McpRequestClosed { id: 1 },
            IdeEvent::UpdateAvailable { update },
            IdeEvent::UpdateStatusChanged {
                status: ide_core::updates::UpdateStatus { last_checked: None, checking: true },
            },
            IdeEvent::WindowGroupsChanged,
            IdeEvent::WindowFullscreenChanged,
        ] {
            assert!(!client_accepts(&event), "{event:?}");
        }
    }

    #[test]
    fn a_server_reports_its_project_to_the_desktop() {
        for event in [
            IdeEvent::FsChanged {
                created: vec!["/srv/app/a".into()],
                modified: vec![],
                removed: vec![],
                ignore_rules: vec![],
            },
            IdeEvent::VcsChanged { root: "/srv/app".into() },
            IdeEvent::Notify { level: NotifyLevel::Info, title: "Build".into(), body: None },
            IdeEvent::Askpass { id: 1, prompt: "Password".into(), kind: AskpassKind::Secret, remember: false },
            IdeEvent::ProjectTrustChanged { root: "/srv/app".into(), trusted: true },
        ] {
            assert!(client_accepts(&event), "{event:?}");
        }
    }

    #[test]
    fn jetbrains_settings_folders_require_options_directory() {
        let dir = tempfile::tempdir().unwrap();
        let incomplete = dir.path().join("Incomplete");
        std::fs::create_dir(&incomplete).unwrap();
        std::fs::write(incomplete.join("settings.xml"), "").unwrap();
        assert!(!is_jetbrains_settings_folder(&incomplete));

        let complete = dir.path().join("IntelliJIdea");
        std::fs::create_dir_all(complete.join("options")).unwrap();
        assert!(is_jetbrains_settings_folder(&complete));
    }
}
