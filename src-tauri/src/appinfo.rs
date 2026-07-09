//! Standardized app metadata for startup banners and About UI.
//! Aligns with Apps/Docs/razorcore-api-spec.md `appinfo` contract.

use serde::Serialize;

pub const APP_NAME: &str = "Rusty";
pub const LICENSE_TEXT: &str = "2026 RazorBackRoar";
pub const COPYRIGHT_FULL: &str = "© 2026 RazorBackRoar. All rights reserved.";
pub const ORGANIZATION: &str = "RazorBackRoar";
pub const ARCHITECTURE: &str = "ARM64 (Apple Silicon)";

#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    pub name: &'static str,
    pub version: String,
    pub license: &'static str,
    pub copyright: &'static str,
    pub organization: &'static str,
    pub architecture: &'static str,
}

impl AppInfo {
    pub fn current() -> Self {
        Self {
            name: APP_NAME,
            version: env!("CARGO_PKG_VERSION").to_string(),
            license: LICENSE_TEXT,
            copyright: COPYRIGHT_FULL,
            organization: ORGANIZATION,
            architecture: ARCHITECTURE,
        }
    }

    pub fn to_console_output(&self) -> String {
        format!(
            "\n\
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\
  {name}\n\
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\
  Version:  {version}\n\
  License:  {license}\n\
  Arch:     {architecture}\n\
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n",
            name = self.name,
            version = self.version,
            license = self.license,
            architecture = self.architecture,
        )
    }
}

/// Print the standardized startup banner to stdout (mirrors Python razorcore).
pub fn print_startup_info() {
    print!("{}", AppInfo::current().to_console_output());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_uses_cargo_pkg_version() {
        let info = AppInfo::current();
        assert_eq!(info.name, "Rusty");
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.to_console_output().contains("Rusty"));
    }
}
