//! The standalone server's built-in MCP service, over real stdio and HTTP.

use ide_host::rpc::EventMessage;
use ide_jsonrpc::{Handler, Peer};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Default)]
struct Client {
    events: Mutex<Vec<EventMessage>>,
}

impl Handler for Client {
    fn on_notification(&self, method: &str, params: Value) {
        if method == "event" {
            self.events.lock().push(serde_json::from_value(params).expect("invalid event"));
        }
    }

    fn on_request(&self, method: &str, _: Value) -> Result<Value, (i64, String)> {
        Err((ide_jsonrpc::codes::METHOD_NOT_FOUND, method.to_owned()))
    }
}

async fn invoke(peer: &Peer, command: &str, args: Value) -> Result<Value, ide_core::IdeError> {
    let call = ide_host::rpc::Invoke { window: "main".to_owned(), cmd: command.to_owned(), args };
    peer.request::<_, Value>("invoke", call, None).await.map_err(ide_host::rpc::error_from_reply)
}

async fn read_json_line<R>(lines: &mut tokio::io::Lines<R>) -> Value
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
        .await
        .expect("timed out waiting for the stdio bridge")
        .expect("cannot read the stdio bridge")
        .expect("the stdio bridge closed early");
    serde_json::from_str(&line).expect("stdio bridge emitted invalid JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_start_http_server_and_stdio_bridge_and_stop_both() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("README.md"), "# runtime test\n").unwrap();
    let base = tmp.path().join("server-home");
    let exe = env!("CARGO_BIN_EXE_openidea-server");
    let env = vec![("OPENIDEA_SERVER_HOME".to_owned(), base.to_string_lossy().into_owned())];

    let client = Arc::new(Client::default());
    let peer = Peer::spawn(exe, &["--stdio".to_owned()], tmp.path(), &env, client.clone(), Box::new(|_| {}))
        .expect("cannot start openidea-server");
    peer.request::<_, Value>("initialize", json!({ "version": ide_host::rpc::PROTOCOL_VERSION }), None)
        .await
        .expect("initialize failed");
    invoke(&peer, "open_project", json!({ "path": project })).await.expect("open_project failed");
    invoke(
        &peer,
        "project_trust_set",
        json!({
            "path": project,
            "trusted": true,
            "folder": false
        }),
    )
    .await
    .expect("trusting the project failed");
    invoke(&peer, "mcp_ui_ready", json!({ "ready": true })).await.expect("making the MCP window ready failed");

    let disabled =
        invoke(&peer, "mcp_server_status", json!({})).await.expect("mcp_server_status failed before enabling");
    assert_eq!(disabled["running"], false, "{disabled}");
    assert_eq!(
        std::fs::metadata(ide_mcp::discovery_path(&base.join("data"))).is_err(),
        true,
        "a disabled server left its discovery file"
    );

    invoke(
        &peer,
        "settings_set",
        json!({
            "scope": "global",
            "key": "mcpServer.enabled",
            "value": "true"
        }),
    )
    .await
    .expect("enabling the MCP server failed");
    let status = invoke(&peer, "mcp_server_status", json!({})).await.expect("mcp_server_status failed after enabling");
    assert_eq!(status["running"], true, "{status}");
    let port = status["port"].as_u64().expect("running server has no port");
    assert!(port > 0);
    let url = status["url"].as_str().expect("running server has no URL");
    assert!(url.starts_with("http://127.0.0.1:") && url.contains("/mcp?project="), "{url}");
    assert!(status["token"].as_str().is_some_and(|token| token.len() == 64), "{status}");
    let command = status["stdioCommand"].as_array().expect("running server has no stdio command");
    assert_eq!(command.first().and_then(Value::as_str), Some(exe));
    assert!(command.iter().any(|arg| arg == "--mcp-stdio"), "{status}");
    let discovery = ide_mcp::Discovery::read(&ide_mcp::discovery_path(&base.join("data")))
        .expect("the running server did not write its discovery file");
    assert_eq!(discovery.port as u64, port);
    assert_eq!(discovery.token.as_str(), status["token"].as_str().unwrap());

    let mut bridge = tokio::process::Command::new(exe)
        .args(["--mcp-stdio", "--project"])
        .arg(&project)
        .env("OPENIDEA_SERVER_HOME", &base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("cannot start the MCP stdio bridge");
    let mut input = bridge.stdin.take().unwrap();
    let mut output = BufReader::new(bridge.stdout.take().unwrap()).lines();
    for message in [
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "runtime-test", "version": "1" }
            }
        }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    ] {
        input.write_all(format!("{message}\n").as_bytes()).await.unwrap();
        input.flush().await.unwrap();
    }
    let mut listed = None;
    for _ in 0..2 {
        let reply = read_json_line(&mut output).await;
        if reply["id"] == 2 {
            listed = Some(reply);
        }
    }
    let listed = listed.expect("the stdio bridge did not answer tools/list");
    let tools = listed["result"]["tools"].as_array().expect("tools/list returned no tools array");
    assert!(!tools.is_empty(), "{listed}");
    assert!(tools.iter().any(|tool| tool["name"].is_string()), "{listed}");

    for (id, allow) in [(3, true), (4, false)] {
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "open_file_in_editor",
                "arguments": { "filePath": project.join("README.md") }
            }
        });
        input.write_all(format!("{message}\n").as_bytes()).await.unwrap();
        input.flush().await.unwrap();

        let (request_id, ui_request) = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let found = client.events.lock().iter().find_map(|message| match &message.event {
                    ide_core::IdeEvent::McpRequest { id, request } => Some((*id, request.clone())),
                    _ => None,
                });
                if let Some(found) = found {
                    break found;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("MCP tool did not ask the project window");
        assert!(matches!(ui_request, ide_core::event::McpUiRequest::OpenFile { .. }));
        invoke(&peer, "mcp_respond", json!({ "id": request_id, "allow": allow, "error": null }))
            .await
            .expect("answering the MCP tool request failed");

        let reply = read_json_line(&mut output).await;
        assert_eq!(reply["id"], id, "{reply}");
        let is_error = reply["result"]["isError"].as_bool().unwrap_or(false);
        assert_eq!(is_error, !allow, "{reply}");
        client
            .events
            .lock()
            .retain(|message| !matches!(message.event, ide_core::IdeEvent::McpRequest { id, .. } if id == request_id));
    }
    bridge.kill().await.expect("cannot stop the stdio bridge");

    invoke(
        &peer,
        "settings_set",
        json!({
            "scope": "global",
            "key": "mcpServer.enabled",
            "value": "false"
        }),
    )
    .await
    .expect("disabling the MCP server failed");
    let stopped =
        invoke(&peer, "mcp_server_status", json!({})).await.expect("mcp_server_status failed after disabling");
    assert_eq!(stopped["running"], false, "{stopped}");
    assert!(
        ide_mcp::Discovery::read(&ide_mcp::discovery_path(&base.join("data"))).is_err(),
        "the disabled server left its discovery file"
    );
    invoke(&peer, "mcp_ui_ready", json!({ "ready": false })).await.expect("closing the MCP window failed");
}
