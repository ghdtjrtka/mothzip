use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub dir: Option<PathBuf>,
    pub no_path: bool,
    pub no_shortcut: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub exe_exists: bool,
    pub version: String,
    pub registry_registered: bool,
    pub shortcut_valid: bool,
    pub path_configured: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallReport {
    pub action: String,
    pub status: String,
    pub version: String,
    pub install_dir: PathBuf,
    pub exe_path: PathBuf,
    pub installed_files: Vec<String>,
    pub registered_registry: bool,
    pub start_menu_shortcut: Option<PathBuf>,
    pub user_path_updated: bool,
    pub verified: VerificationReport,
}

#[derive(Debug, Clone, Default)]
pub struct UninstallOptions {
    pub keep_path: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UninstallVerificationReport {
    pub files_removed: bool,
    pub registry_removed: bool,
    pub shortcut_removed: bool,
    pub path_cleaned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UninstallReport {
    pub action: String,
    pub status: String,
    pub install_dir: PathBuf,
    pub removed_files: Vec<String>,
    pub deferred_delete_scheduled: bool,
    pub cleanup_result_path: Option<PathBuf>,
    pub unregistered_registry: bool,
    pub removed_shortcut: bool,
    pub user_path_cleaned: bool,
    pub verified: UninstallVerificationReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryStatusDetails {
    pub uninstall_entry: bool,
    pub uninstall_location: Option<String>,
    pub uninstall_string: Option<String>,
    pub app_paths_entry: bool,
    pub context_menu_verbs: bool,
    pub registered_applications: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    pub is_installed: bool,
    pub install_type: String,
    pub current_exe: PathBuf,
    pub install_dir: Option<PathBuf>,
    pub version: String,
    pub registered_in_registry: bool,
    pub registry_details: RegistryStatusDetails,
    pub start_menu_shortcut_exists: bool,
    pub in_user_path: bool,
    pub bandizip_detected: bool,
    pub supported_extensions: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RegisterOptions {
    pub no_shortcut: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterReport {
    pub action: String,
    pub status: String,
    pub exe_path: PathBuf,
    pub reg_file: Option<PathBuf>,
    pub start_menu_shortcut: Option<PathBuf>,
    pub verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnregisterReport {
    pub action: String,
    pub status: String,
    pub unregistered_keys: Vec<String>,
    pub removed_shortcut: bool,
    pub verified: bool,
}
