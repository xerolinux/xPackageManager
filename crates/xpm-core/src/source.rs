use crate::error::Result;
use crate::operation::{Operation, OperationProgress, OperationResult};
use crate::package::{Package, PackageInfo, SearchResult, UpdateInfo};
use async_trait::async_trait;

pub type ProgressCallback = Box<dyn Fn(OperationProgress) + Send + Sync>;

// main trait that every backend needs to implement
#[async_trait]
pub trait PackageSource: Send + Sync {
    fn source_id(&self) -> &str;
    fn display_name(&self) -> &str;
    async fn is_available(&self) -> bool;
    async fn search(&self, query: &str) -> Result<Vec<SearchResult>>;
    async fn list_installed(&self) -> Result<Vec<Package>>;
    async fn list_updates(&self) -> Result<Vec<UpdateInfo>>;
    async fn get_package_info(&self, name: &str) -> Result<PackageInfo>;
    async fn execute(&self, operation: Operation) -> Result<OperationResult>;
    async fn execute_with_progress(
        &self,
        operation: Operation,
        progress: ProgressCallback,
    ) -> Result<OperationResult>;
    async fn sync_databases(&self) -> Result<()>;
    async fn get_cache_size(&self) -> Result<u64>;
    async fn clean_cache(&self, keep_versions: usize) -> Result<u64>;
    async fn list_orphans(&self) -> Result<Vec<Package>>;
}

#[async_trait]
pub trait PackageSourceExt: PackageSource {
    async fn is_installed(&self, name: &str) -> Result<bool> {
        let installed = self.list_installed().await?;
        Ok(installed.iter().any(|p| p.name == name))
    }

    async fn installed_count(&self) -> Result<usize> {
        Ok(self.list_installed().await?.len())
    }

    async fn update_count(&self) -> Result<usize> {
        Ok(self.list_updates().await?.len())
    }
}

// blanket impl for all backends
impl<T: PackageSource> PackageSourceExt for T {}
