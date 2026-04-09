use libflatpak::{gio, prelude::*, Installation, Remote};
use tracing::{debug, info};
use xpm_core::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct RemoteInfo {
    pub name: String,
    pub title: String,
    pub url: String,
    pub enabled: bool,
    pub is_user: bool,
}

pub struct RemoteManager;

impl RemoteManager {
    pub fn new() -> Self {
        Self
    }

    pub fn list_remotes(&self, installation: &Installation) -> Result<Vec<RemoteInfo>> {
        let remotes = installation
            .list_remotes(gio::Cancellable::NONE)
            .map_err(|e| Error::DatabaseError(e.to_string()))?;

        let mut infos = Vec::new();

        for remote in remotes {
            let name = remote.name().map(|s| s.to_string()).unwrap_or_default();
            let title = remote.title().map(|s| s.to_string()).unwrap_or_default();
            let url = remote.url().map(|s| s.to_string()).unwrap_or_default();
            let enabled = !remote.is_disabled();

            infos.push(RemoteInfo {
                name,
                title,
                url,
                enabled,
                is_user: !installation.is_user(),
            });
        }

        Ok(infos)
    }

    pub fn add_remote(
        &self,
        installation: &Installation,
        name: &str,
        url: &str,
    ) -> Result<()> {
        let remote = Remote::new(name);
        remote.set_url(url);
        remote.set_gpg_verify(true);

        installation
            .add_remote(&remote, true, gio::Cancellable::NONE)
            .map_err(|e| Error::TransactionError(e.to_string()))?;

        info!("Added flatpak remote: {} ({})", name, url);
        Ok(())
    }

    pub fn remove_remote(&self, installation: &Installation, name: &str) -> Result<()> {
        installation
            .remove_remote(name, gio::Cancellable::NONE)
            .map_err(|e| Error::TransactionError(e.to_string()))?;

        info!("Removed flatpak remote: {}", name);
        Ok(())
    }

    // loops through all remotes to find the one we want, kinda wasteful
    pub fn set_remote_enabled(
        &self,
        installation: &Installation,
        name: &str,
        enabled: bool,
    ) -> Result<()> {
        let remotes = installation
            .list_remotes(gio::Cancellable::NONE)
            .map_err(|e| Error::DatabaseError(e.to_string()))?;

        for remote in remotes {
            let remote_name = remote.name().map(|s| s.to_string());
            if remote_name.as_deref() == Some(name) {
                remote.set_disabled(!enabled);

                installation
                    .modify_remote(&remote, gio::Cancellable::NONE)
                    .map_err(|e| Error::TransactionError(e.to_string()))?;

                info!(
                    "Flatpak remote {} {}",
                    name,
                    if enabled { "enabled" } else { "disabled" }
                );
                return Ok(());
            }
        }

        Err(Error::Other(format!("Remote not found: {}", name)))
    }

    pub fn update_remote(&self, installation: &Installation, name: &str) -> Result<()> {
        installation
            .update_remote_sync(name, gio::Cancellable::NONE)
            .map_err(|e| Error::NetworkError(e.to_string()))?;

        debug!("Updated remote metadata: {}", name);
        Ok(())
    }
}

impl Default for RemoteManager {
    fn default() -> Self {
        Self::new()
    }
}
