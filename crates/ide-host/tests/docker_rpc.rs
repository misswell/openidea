//! Docker commands through the real headless dispatcher and an isolated CLI stub.
#![cfg(unix)]
use ide_host::rpc::{self, ChannelMessage, Dispatcher, Invoke};
use ide_host::{Host, HostConfig};
use ide_jsonrpc::{Handler, Peer};
use ide_ssh::secrets::testing::{files_containing, use_memory_store};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[derive(Default)]
struct Client(Mutex<Vec<ChannelMessage>>);
impl Handler for Client {
    fn on_notification(&self, method: &str, params: Value) {
        if method == "channel" {
            self.0.lock().push(serde_json::from_value(params).unwrap());
        }
    }
    fn on_request(&self, method: &str, _: Value) -> Result<Value, (i64, String)> {
        Err((ide_jsonrpc::codes::METHOD_NOT_FOUND, method.into()))
    }
}
async fn invoke(peer: &Peer, cmd: &str, args: Value) -> Result<Value, ide_core::IdeError> {
    peer.request::<_, Value>("invoke", Invoke { window: "main".into(), cmd: cmd.into(), args }, None)
        .await
        .map_err(rpc::error_from_reply)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn docker_dispatcher_contract() {
    use_memory_store();
    let tmp = tempfile::tempdir().unwrap();
    let executable = tmp.path().join("docker");
    std::fs::write(&executable, "#!/bin/sh\ncase \"$*\" in\n  *logs*) echo hello; exec sleep 30;;\n  *events*) echo '{\"Type\":\"container\",\"Action\":\"start\",\"Actor\":{\"ID\":\"abc\",\"Attributes\":{}},\"time\":1}'; exec sleep 30;;\n  *) echo stub; exit 0;;\nesac\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    // This integration test is its own process and has only one test.
    unsafe { std::env::set_var("OPENIDEA_DOCKER", &executable) };
    let slot = Arc::new(OnceLock::new());
    let sink_slot = slot.clone();
    let data = tmp.path().join("isolated-data");
    let host = Arc::new(Host::new(HostConfig {
        config_dir: tmp.path().join("config"),
        data_dir: data.clone(),
        local_data_dir: data.clone(),
        languages: Arc::new(ide_language::LanguageRegistry::load(None).unwrap()),
        syntax: Arc::default(),
        sink_for: Box::new(move |w| rpc::event_sink(sink_slot.clone(), Some(w.to_owned()))),
        owner: |w| w,
        runtime: tokio::runtime::Handle::current(),
        project_environment: false,
    }));
    let (client_io, server_io) = tokio::io::duplex(4096);
    let (sr, sw) = tokio::io::split(server_io);
    let server = tokio::spawn(rpc::serve(host, Dispatcher::standard(), slot, sr, sw));
    let (cr, cw) = tokio::io::split(client_io);
    let client = Arc::new(Client::default());
    let peer = Peer::connect(cr, cw, client.clone());
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    invoke(&peer, "project_trust_set", json!({"path":project,"trusted":true,"folder":false})).await.unwrap();
    invoke(&peer, "open_project", json!({"path":project})).await.unwrap();
    let settings =
        json!({"connections":[],"registries":[{"id":"r","name":"Test","address":"example.com","username":"u"}]});
    assert_eq!(invoke(&peer, "docker_save_settings", json!({"settings":settings})).await.unwrap(), Value::Null);
    assert_eq!(invoke(&peer, "docker_settings", json!({})).await.unwrap(), settings);
    invoke(&peer, "docker_set_registry_password", json!({"id":"r","password":"test-secret"})).await.unwrap();
    assert!(data.join("docker-passwords.json").exists());
    assert_eq!(files_containing(&data, "test-secret"), Vec::<std::path::PathBuf>::new());
    assert_eq!(
        invoke(
            &peer,
            "docker_registry_login",
            json!({"endpoint":{"kind":"default"},"registry":settings["registries"][0],"password":null})
        )
        .await
        .unwrap(),
        "stub"
    );
    invoke(&peer, "docker_save_settings", json!({"settings":{"connections":[],"registries":[]}})).await.unwrap();
    assert!(
        invoke(
            &peer,
            "docker_registry_login",
            json!({"endpoint":{"kind":"default"},"registry":settings["registries"][0],"password":null})
        )
        .await
        .unwrap_err()
        .message
        .contains("password")
    );
    assert!(
        invoke(
            &peer,
            "docker_logs",
            json!({"request":1,"endpoint":{"kind":"default"},"id":"x","tail":1,"follow":true,"timestamps":false})
        )
        .await
        .unwrap_err()
        .message
        .contains("onLine")
    );
    for (cmd, request, channel, field) in [("docker_logs", 21, 31, "onLine"), ("docker_events", 22, 32, "onEvent")] {
        let mut args =
            json!({"request":request,"endpoint":{"kind":"default"},"id":"x","tail":1,"follow":true,"timestamps":false});
        args[field] = json!({"$channel":channel});
        let p = peer.clone();
        let task = tokio::spawn(async move { invoke(&p, cmd, args).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !client.0.lock().iter().any(|v| v.id == channel) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        invoke(&peer, "cancel_request", json!({"id":request})).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap().is_err());
    }
    drop(peer);
    tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
}
