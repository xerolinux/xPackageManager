use xpm_core::{
    operation::OperationResult,
    package::{Package, PackageBackend, SearchResult, UpdateInfo},
};

// default view on startup
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewState {
    #[default]
    Installed,
    Updates,
    Search,
    Flatpak,
    Settings,
    Maintenance,
}

#[derive(Debug, Clone, Default)]
pub struct FilterOptions {
    pub search_text: String,
    pub backend: Option<PackageBackend>,
    pub explicit_only: bool,
    pub updates_only: bool,
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub view: ViewState,
    pub filter: FilterOptions,
    pub selected_package: Option<String>,
    pub installed_packages: Vec<Package>,
    pub updates: Vec<UpdateInfo>,
    pub search_results: Vec<SearchResult>,
    pub last_operation: Option<OperationResult>,
    pub operation_in_progress: bool,
    pub error_message: Option<String>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            view: ViewState::Installed,
            filter: FilterOptions::default(),
            selected_package: None,
            installed_packages: Vec::new(),
            updates: Vec::new(),
            search_results: Vec::new(),
            last_operation: None,
            operation_in_progress: false,
            error_message: None,
        }
    }

    pub fn set_view(&mut self, view: ViewState) {
        self.view = view;
        self.selected_package = None;
    }

    pub fn set_search(&mut self, text: String) {
        self.filter.search_text = text;
    }

    pub fn clear_error(&mut self) {
        self.error_message = None;
    }

    pub fn set_error(&mut self, message: String) {
        self.error_message = Some(message);
    }

    // filters by text and backend, chain style
    pub fn filtered_installed(&self) -> Vec<&Package> {
        self.installed_packages
            .iter()
            .filter(|p| {
                if !self.filter.search_text.is_empty() {
                    let search = self.filter.search_text.to_lowercase();
                    if !p.name.to_lowercase().contains(&search)
                        && !p.description.to_lowercase().contains(&search)
                    {
                        return false;
                    }
                }

                if let Some(backend) = self.filter.backend {
                    if p.backend != backend {
                        return false;
                    }
                }

                true
            })
            .collect()
    }

    pub fn installed_count_by_backend(&self, backend: PackageBackend) -> usize {
        self.installed_packages
            .iter()
            .filter(|p| p.backend == backend)
            .count()
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
