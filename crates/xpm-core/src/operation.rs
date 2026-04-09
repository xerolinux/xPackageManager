use crate::package::{Package, PackageBackend};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OperationKind {
    Install,
    Remove,
    RemoveWithDeps,
    Update,
    SystemUpgrade,
    SyncDatabases,
    CleanCache,
    RemoveOrphans,
}

impl fmt::Display for OperationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OperationKind::Install => write!(f, "Install"),
            OperationKind::Remove => write!(f, "Remove"),
            OperationKind::RemoveWithDeps => write!(f, "Remove with dependencies"),
            OperationKind::Update => write!(f, "Update"),
            OperationKind::SystemUpgrade => write!(f, "System upgrade"),
            OperationKind::SyncDatabases => write!(f, "Sync databases"),
            OperationKind::CleanCache => write!(f, "Clean cache"),
            OperationKind::RemoveOrphans => write!(f, "Remove orphans"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub kind: OperationKind,
    pub packages: Vec<String>,
    pub backend: PackageBackend,
    pub options: OperationOptions,
}

impl Operation {
    pub fn install(packages: Vec<String>, backend: PackageBackend) -> Self {
        Self {
            kind: OperationKind::Install,
            packages,
            backend,
            options: OperationOptions::default(),
        }
    }

    pub fn remove(packages: Vec<String>, backend: PackageBackend) -> Self {
        Self {
            kind: OperationKind::Remove,
            packages,
            backend,
            options: OperationOptions::default(),
        }
    }

    pub fn update(packages: Vec<String>, backend: PackageBackend) -> Self {
        Self {
            kind: OperationKind::Update,
            packages,
            backend,
            options: OperationOptions::default(),
        }
    }

    pub fn system_upgrade(backend: PackageBackend) -> Self {
        Self {
            kind: OperationKind::SystemUpgrade,
            packages: Vec::new(),
            backend,
            options: OperationOptions::default(),
        }
    }

    pub fn sync_databases(backend: PackageBackend) -> Self {
        Self {
            kind: OperationKind::SyncDatabases,
            packages: Vec::new(),
            backend,
            options: OperationOptions::default(),
        }
    }

    pub fn with_options(mut self, options: OperationOptions) -> Self {
        self.options = options;
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OperationOptions {
    pub no_confirm: bool,
    pub download_only: bool,
    pub force: bool,
    pub recursive: bool,
    pub keep_config: bool,
    pub no_deps: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationStatus {
    Pending,
    ResolvingDeps,
    Downloading,
    Verifying,
    Processing,
    RunningHooks,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationResult {
    pub operation: Operation,
    pub status: OperationStatus,
    pub affected_packages: Vec<Package>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
    pub duration_ms: u64,
}

impl OperationResult {
    pub fn success(operation: Operation, affected: Vec<Package>, duration_ms: u64) -> Self {
        Self {
            operation,
            status: OperationStatus::Completed,
            affected_packages: affected,
            warnings: Vec::new(),
            error: None,
            duration_ms,
        }
    }

    pub fn failure(operation: Operation, error: impl Into<String>, duration_ms: u64) -> Self {
        Self {
            operation,
            status: OperationStatus::Failed,
            affected_packages: Vec::new(),
            warnings: Vec::new(),
            error: Some(error.into()),
            duration_ms,
        }
    }

    pub fn is_success(&self) -> bool {
        self.status == OperationStatus::Completed
    }

    pub fn with_warning(mut self, warning: impl Into<String>) -> Self {
        self.warnings.push(warning.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationProgress {
    pub status: OperationStatus,
    pub current_package: Option<String>,
    pub total_packages: usize,
    pub completed_packages: usize,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub message: String,
}

impl OperationProgress {
    pub fn new(total_packages: usize, total_bytes: u64) -> Self {
        Self {
            status: OperationStatus::Pending,
            current_package: None,
            total_packages,
            completed_packages: 0,
            total_bytes,
            downloaded_bytes: 0,
            message: String::new(),
        }
    }

    // avoid divide by zero when theres nothing to download
    pub fn download_percent(&self) -> u8 {
        if self.total_bytes == 0 {
            return 100;
        }
        ((self.downloaded_bytes as f64 / self.total_bytes as f64) * 100.0) as u8
    }

    pub fn package_percent(&self) -> u8 {
        if self.total_packages == 0 {
            return 100;
        }
        ((self.completed_packages as f64 / self.total_packages as f64) * 100.0) as u8
    }
}
