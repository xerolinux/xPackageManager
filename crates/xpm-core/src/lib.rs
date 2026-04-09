pub mod error;
pub mod operation;
pub mod package;
pub mod source;

// re-export the main types so consumers dont have to dig around
pub use error::{Error, Result};
pub use operation::{Operation, OperationKind, OperationResult, OperationStatus};
pub use package::{Package, PackageInfo, PackageStatus, SearchResult, UpdateInfo, Version};
pub use source::PackageSource;
