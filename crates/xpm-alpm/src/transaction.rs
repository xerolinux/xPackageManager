use xpm_core::{
    error::{Error, Result},
    operation::OperationOptions,
    package::Package,
    source::ProgressCallback,
};

// stub - needs root privs to actually do anything
pub struct TransactionHandler;

impl TransactionHandler {
    pub fn new() -> Self {
        Self
    }

    pub fn install(
        &self,
        _packages: &[String],
        _options: &OperationOptions,
        _progress: ProgressCallback,
    ) -> Result<Vec<Package>> {
        Err(Error::PermissionDenied(
            "Package installation requires root privileges".into(),
        ))
    }

    pub fn remove(
        &self,
        _packages: &[String],
        _options: &OperationOptions,
        _progress: ProgressCallback,
    ) -> Result<Vec<Package>> {
        Err(Error::PermissionDenied(
            "Package removal requires root privileges".into(),
        ))
    }

    pub fn upgrade(
        &self,
        _packages: &[String],
        _options: &OperationOptions,
        _progress: ProgressCallback,
    ) -> Result<Vec<Package>> {
        Err(Error::PermissionDenied(
            "Package upgrade requires root privileges".into(),
        ))
    }

    pub fn sysupgrade(
        &self,
        _options: &OperationOptions,
        _progress: ProgressCallback,
    ) -> Result<Vec<Package>> {
        Err(Error::PermissionDenied(
            "System upgrade requires root privileges".into(),
        ))
    }

    pub fn sync_dbs(&self, _progress: ProgressCallback) -> Result<Vec<Package>> {
        Err(Error::PermissionDenied(
            "Database sync requires root privileges".into(),
        ))
    }
}

impl Default for TransactionHandler {
    fn default() -> Self {
        Self::new()
    }
}
