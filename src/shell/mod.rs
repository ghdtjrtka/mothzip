pub mod error;
pub mod installer;
pub mod manifest;
pub mod model;
pub mod path;
pub mod registry;
pub mod shortcut;
pub mod status;
pub mod system;

#[allow(unused_imports)]
pub use error::ShellError;
pub use installer::{install, register, uninstall, unregister};
#[allow(unused_imports)]
pub use model::{
    InstallOptions, InstallReport, RegisterOptions, RegisterReport, StatusReport, UninstallOptions,
    UninstallReport, UnregisterReport,
};
pub use status::get_status;

#[cfg(test)]
mod tests;
