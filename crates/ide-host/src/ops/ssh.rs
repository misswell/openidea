//! SSH credentials and session state for deployment and run targets.
//!
//! Secrets enter this module once and are never returned by a command. A
//! password or passphrase is retained for this process and optionally in the
//! SSH manager's saved-password file, using the exact configuration id or an
//! FTP key (`ftp:<deployment-server-id>`).

use ide_core::remote_settings::SshConfiguration;
use ide_core::{IdeError, IdeResult};

use crate::Host;

/// Tests one configuration, using `secret` when supplied or the stored one.
pub async fn test(host: &Host, config: SshConfiguration, secret: Option<String>) -> IdeResult<String> {
    host.ssh.test(&config, secret).await
}

/// Stores or forgets the credential for `id`.
pub fn set_secret(host: &Host, id: String, secret: Option<String>, save: bool) -> IdeResult<()> {
    let id = id.trim();
    if id.is_empty() {
        return Err(IdeError::invalid("the credential id is empty"));
    }
    if secret.as_deref().is_some_and(str::is_empty) {
        return Err(IdeError::invalid("the password or passphrase is empty"));
    }
    host.ssh.set_secret(id, secret, save);
    Ok(())
}

/// Whether connecting to the saved `id` needs a password first.
pub fn needs_secret(host: &Host, id: &str) -> IdeResult<bool> {
    Ok(host.ssh.needs_secret(&host.ssh.config(id)?))
}

/// Trusts the host key captured by the last failed connection of `id`.
///
/// `fingerprint` is optional for older callers. Callers that show a prompt
/// pass the exact fingerprint they displayed.
pub async fn trust_host(host: &Host, id: &str, fingerprint: Option<String>) -> IdeResult<()> {
    if id.trim().is_empty() {
        return Err(IdeError::invalid("the SSH host key id is empty"));
    }
    host.ssh.trust_host_matching(id, fingerprint.as_deref()).await
}

/// Ids of SSH configurations with a live connection.
pub fn connected(host: &Host) -> Vec<String> {
    let mut connected = host.ssh.connected();
    connected.sort();
    connected
}

/// Closes the shared session of one SSH configuration.
pub fn disconnect(host: &Host, id: &str) -> IdeResult<()> {
    if host.ssh.config(id).is_err() {
        return Err(IdeError::not_found(format!("no SSH configuration {id}")));
    }
    host.ssh.disconnect(id);
    Ok(())
}

pub(crate) fn register(d: &mut crate::rpc::Dispatcher) {
    d.add("ssh_test", |c| async move { test(&c.host, c.arg("config")?, c.arg("secret")?).await });
    d.add("ssh_set_secret", |c| async move { set_secret(&c.host, c.arg("id")?, c.arg("secret")?, c.arg("save")?) });
    d.add("ssh_needs_secret", |c| async move {
        let id: String = c.arg("id")?;
        needs_secret(&c.host, &id)
    });
    d.add("ssh_trust_host", |c| async move {
        let id: String = c.arg("id")?;
        let fingerprint: Option<String> = c.arg("fingerprint")?;
        trust_host(&c.host, &id, fingerprint).await
    });
    d.add("ssh_connected", |c| async move { Ok(connected(&c.host)) });
    d.add("ssh_disconnect", |c| async move {
        let id: String = c.arg("id")?;
        disconnect(&c.host, &id)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Host, HostConfig};
    use ide_core::remote_settings::SshAuth;
    use std::sync::Arc;

    fn host(tmp: &tempfile::TempDir) -> Host {
        Host::new(HostConfig {
            config_dir: tmp.path().join("config"),
            data_dir: tmp.path().join("data"),
            local_data_dir: tmp.path().join("data"),
            languages: Arc::new(ide_language::LanguageRegistry::load(None).unwrap()),
            syntax: Arc::default(),
            sink_for: Box::new(|_| Arc::new(ide_core::NullSink)),
            owner: |window| window,
            runtime: tokio::runtime::Handle::current(),
            project_environment: false,
        })
    }

    #[tokio::test]
    async fn empty_credential_ids_and_values_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let host = host(&tmp);
        assert!(set_secret(&host, " ".into(), Some("secret".into()), false).is_err());
        assert!(set_secret(&host, "ssh".into(), Some(String::new()), false).is_err());
    }

    #[tokio::test]
    async fn saved_password_status_never_returns_the_value() {
        let tmp = tempfile::tempdir().unwrap();
        let host = host(&tmp);
        host.ssh.set_configs(vec![SshConfiguration {
            id: "build".into(),
            name: "Build".into(),
            host: "build.example".into(),
            user: "me".into(),
            auth: SshAuth::Password,
            save_secret: true,
            ..Default::default()
        }]);
        assert!(needs_secret(&host, "build").unwrap());
        set_secret(&host, "build".into(), Some("not returned".into()), true).unwrap();
        assert!(!needs_secret(&host, "build").unwrap());
    }
}
