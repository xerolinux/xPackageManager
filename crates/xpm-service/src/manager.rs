use crate::progress::ProgressTracker;
use crate::state::AppState;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};
use tracing::{error, info};
use xpm_alpm::AlpmBackend;
use xpm_core::{
    error::{Error, Result},
    operation::{Operation, OperationProgress, OperationResult},
    package::{Package, PackageBackend, PackageInfo, SearchResult, UpdateInfo},
    source::PackageSource,
};
use xpm_flatpak::FlatpakBackend;

#[derive(Debug, Clone)]
pub enum ProgressMessage {
    Progress(OperationProgress),
    Completed(OperationResult),
    Error(String),
}

// orchestrate the backends
pub struct PackageManager {
    alpm: Option<Arc<AlpmBackend>>,
    flatpak: Option<Arc<FlatpakBackend>>,
    state: Arc<RwLock<AppState>>,
    _progress_tracker: Arc<Mutex<ProgressTracker>>,
    progress_tx: broadcast::Sender<ProgressMessage>,
}

impl PackageManager {
    pub fn new() -> Result<Self> {
        let (progress_tx, _) = broadcast::channel(100);

        let alpm = match AlpmBackend::new() {
            Ok(backend) => {
                info!("ALPM backend initialized");
                Some(Arc::new(backend))
            }
            Err(e) => {
                error!("Failed to initialize ALPM: {}", e);
                None
            }
        };

        let flatpak = match FlatpakBackend::new() {
            Ok(backend) => {
                info!("Flatpak backend initialized");
                Some(Arc::new(backend))
            }
            Err(e) => {
                error!("Failed to initialize Flatpak: {}", e);
                None
            }
        };

        Ok(Self {
            alpm,
            flatpak,
            state: Arc::new(RwLock::new(AppState::new())),
            _progress_tracker: Arc::new(Mutex::new(ProgressTracker::new())),
            progress_tx,
        })
    }

    pub fn subscribe_progress(&self) -> broadcast::Receiver<ProgressMessage> {
        self.progress_tx.subscribe()
    }

    pub async fn state(&self) -> AppState {
        self.state.read().await.clone()
    }

    fn get_backend(&self, backend: PackageBackend) -> Result<&dyn PackageSource> {
        match backend {
            PackageBackend::Pacman => self
                .alpm
                .as_ref()
                .map(|b| b.as_ref() as &dyn PackageSource)
                .ok_or_else(|| Error::BackendUnavailable("Pacman".into())),
            PackageBackend::Flatpak => self
                .flatpak
                .as_ref()
                .map(|b| b.as_ref() as &dyn PackageSource)
                .ok_or_else(|| Error::BackendUnavailable("Flatpak".into())),
        }
    }

    pub async fn available_backends(&self) -> Vec<PackageBackend> {
        let mut backends = Vec::new();

        if let Some(ref alpm) = self.alpm {
            if alpm.is_available().await {
                backends.push(PackageBackend::Pacman);
            }
        }

        if let Some(ref flatpak) = self.flatpak {
            if flatpak.is_available().await {
                backends.push(PackageBackend::Flatpak);
            }
        }

        backends
    }

    // search across all availble backends
    pub async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        let mut results = Vec::new();

        if let Some(ref alpm) = self.alpm {
            match alpm.search(query).await {
                Ok(r) => results.extend(r),
                Err(e) => error!("Pacman search failed: {}", e),
            }
        }

        if let Some(ref flatpak) = self.flatpak {
            match flatpak.search(query).await {
                Ok(r) => results.extend(r),
                Err(e) => error!("Flatpak search failed: {}", e),
            }
        }

        results.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(results)
    }

    pub async fn search_backend(
        &self,
        query: &str,
        backend: PackageBackend,
    ) -> Result<Vec<SearchResult>> {
        self.get_backend(backend)?.search(query).await
    }

    pub async fn list_installed(&self) -> Result<Vec<Package>> {
        let mut packages = Vec::new();

        if let Some(ref alpm) = self.alpm {
            match alpm.list_installed().await {
                Ok(p) => packages.extend(p),
                Err(e) => error!("Failed to list pacman packages: {}", e),
            }
        }

        if let Some(ref flatpak) = self.flatpak {
            match flatpak.list_installed().await {
                Ok(p) => packages.extend(p),
                Err(e) => error!("Failed to list flatpak packages: {}", e),
            }
        }

        packages.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(packages)
    }

    pub async fn list_installed_backend(&self, backend: PackageBackend) -> Result<Vec<Package>> {
        self.get_backend(backend)?.list_installed().await
    }

    pub async fn list_updates(&self) -> Result<Vec<UpdateInfo>> {
        let mut updates = Vec::new();

        if let Some(ref alpm) = self.alpm {
            match alpm.list_updates().await {
                Ok(u) => updates.extend(u),
                Err(e) => error!("Failed to check pacman updates: {}", e),
            }
        }

        if let Some(ref flatpak) = self.flatpak {
            match flatpak.list_updates().await {
                Ok(u) => updates.extend(u),
                Err(e) => error!("Failed to check flatpak updates: {}", e),
            }
        }

        Ok(updates)
    }

    pub async fn get_package_info(
        &self,
        name: &str,
        backend: PackageBackend,
    ) -> Result<PackageInfo> {
        self.get_backend(backend)?.get_package_info(name).await
    }

    // run operation and broadcast progress to subscribers
    pub async fn execute(&self, operation: Operation) -> Result<OperationResult> {
        let backend = self.get_backend(operation.backend)?;
        let tx = self.progress_tx.clone();

        let progress_callback = Box::new(move |progress: OperationProgress| {
            let _ = tx.send(ProgressMessage::Progress(progress));
        });

        let result = backend
            .execute_with_progress(operation, progress_callback)
            .await?;

        let _ = self
            .progress_tx
            .send(ProgressMessage::Completed(result.clone()));

        {
            let mut state = self.state.write().await;
            state.last_operation = Some(result.clone());
        }

        Ok(result)
    }

    pub async fn sync_databases(&self) -> Result<()> {
        if let Some(ref alpm) = self.alpm {
            alpm.sync_databases().await?;
        }
        Ok(())
    }

    pub async fn get_cache_size(&self) -> Result<u64> {
        let mut total = 0u64;

        if let Some(ref alpm) = self.alpm {
            total += alpm.get_cache_size().await.unwrap_or(0);
        }

        if let Some(ref flatpak) = self.flatpak {
            total += flatpak.get_cache_size().await.unwrap_or(0);
        }

        Ok(total)
    }

    pub async fn clean_caches(&self, keep_versions: usize) -> Result<u64> {
        let mut freed = 0u64;

        if let Some(ref alpm) = self.alpm {
            freed += alpm.clean_cache(keep_versions).await.unwrap_or(0);
        }

        if let Some(ref flatpak) = self.flatpak {
            freed += flatpak.clean_cache(keep_versions).await.unwrap_or(0);
        }

        Ok(freed)
    }

    pub async fn list_orphans(&self) -> Result<Vec<Package>> {
        let mut orphans = Vec::new();

        if let Some(ref alpm) = self.alpm {
            match alpm.list_orphans().await {
                Ok(o) => orphans.extend(o),
                Err(e) => error!("Failed to list orphans: {}", e),
            }
        }

        Ok(orphans)
    }

    pub async fn get_stats(&self) -> PackageStats {
        let mut stats = PackageStats::default();

        if let Some(ref alpm) = self.alpm {
            if let Ok(packages) = alpm.list_installed().await {
                stats.pacman_installed = packages.len();
            }
            if let Ok(updates) = alpm.list_updates().await {
                stats.pacman_updates = updates.len();
            }
            if let Ok(orphans) = alpm.list_orphans().await {
                stats.orphans = orphans.len();
            }
        }

        if let Some(ref flatpak) = self.flatpak {
            if let Ok(packages) = flatpak.list_installed().await {
                stats.flatpak_installed = packages.len();
            }
            if let Ok(updates) = flatpak.list_updates().await {
                stats.flatpak_updates = updates.len();
            }
        }

        stats
    }
}

#[derive(Debug, Clone, Default)]
pub struct PackageStats {
    pub pacman_installed: usize,
    pub pacman_updates: usize,
    pub flatpak_installed: usize,
    pub flatpak_updates: usize,
    pub orphans: usize,
}

impl PackageStats {
    pub fn total_installed(&self) -> usize {
        self.pacman_installed + self.flatpak_installed
    }

    pub fn total_updates(&self) -> usize {
        self.pacman_updates + self.flatpak_updates
    }
}
