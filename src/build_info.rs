//! Build identity helpers.

pub const BASE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    WindowsInstaller,
    Deb,
}

impl PackageManager {
    pub fn id(self) -> &'static str {
        match self {
            Self::WindowsInstaller => "windows-installer",
            Self::Deb => "deb",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageIdentity<'a> {
    pub manager: PackageManager,
    pub source_commit: &'a str,
}

pub fn validate_package_identity<'a>(
    manager: Option<&str>,
    source_commit: Option<&'a str>,
) -> Result<Option<PackageIdentity<'a>>, &'static str> {
    let manager = match manager {
        None => return Ok(None),
        Some("windows-installer") => PackageManager::WindowsInstaller,
        Some("deb") => PackageManager::Deb,
        Some(_) => return Err("HERDR_PACKAGE_MANAGER must be windows-installer or deb"),
    };
    let source_commit = source_commit
        .filter(|commit| commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or("package builds require HERDR_BUILD_COMMIT to be a full 40-character Git SHA")?;
    Ok(Some(PackageIdentity {
        manager,
        source_commit,
    }))
}

pub fn package_identity() -> Option<PackageIdentity<'static>> {
    validate_package_identity(
        option_env!("HERDR_PACKAGE_MANAGER"),
        option_env!("HERDR_BUILD_COMMIT"),
    )
    .expect("package identity was validated by build.rs")
}

pub fn package_manager() -> Option<PackageManager> {
    package_identity().map(|identity| identity.manager)
}

pub fn channel() -> &'static str {
    non_empty(option_env!("HERDR_BUILD_CHANNEL")).unwrap_or("stable")
}

pub fn build_id() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_ID"))
}

pub fn version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| format_version(BASE_VERSION, channel(), build_id(), package_identity()))
}

fn format_version(
    base_version: &str,
    channel: &str,
    build_id: Option<&str>,
    package: Option<PackageIdentity<'_>>,
) -> String {
    if let Some(package) = package {
        return format!(
            "{base_version}-gx.{}.{}",
            package.manager.id(),
            package.source_commit
        );
    }
    match channel {
        "stable" => base_version.to_string(),
        channel => match build_id {
            Some(build_id) => format!("{base_version}-{channel}.{build_id}"),
            None => format!("{base_version}-{channel}"),
        },
    }
}

pub fn is_preview() -> bool {
    channel() == "preview"
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn package_identity_requires_known_manager_and_full_commit() {
        for manager in [PackageManager::WindowsInstaller, PackageManager::Deb] {
            assert_eq!(
                validate_package_identity(Some(manager.id()), Some(COMMIT)),
                Ok(Some(PackageIdentity {
                    manager,
                    source_commit: COMMIT,
                }))
            );
            for commit in [
                None,
                Some(""),
                Some("abc1234"),
                Some(" 0123456789abcdef0123456789abcdef01234567"),
                Some("g123456789abcdef0123456789abcdef01234567"),
            ] {
                assert!(validate_package_identity(Some(manager.id()), commit).is_err());
            }
        }
        for manager in ["", "apt", "nix", "DEB", " deb", "windows-installer "] {
            assert!(validate_package_identity(Some(manager), Some(COMMIT)).is_err());
        }
        assert_eq!(validate_package_identity(None, None), Ok(None));
        assert_eq!(validate_package_identity(None, Some("legacy")), Ok(None));
    }

    #[test]
    fn package_version_binds_manager_and_full_commit_independently_of_channel() {
        for manager in [PackageManager::WindowsInstaller, PackageManager::Deb] {
            let package = validate_package_identity(Some(manager.id()), Some(COMMIT)).unwrap();
            for channel in ["stable", "preview"] {
                assert_eq!(
                    format_version(BASE_VERSION, channel, Some("ignored"), package),
                    format!("{BASE_VERSION}-gx.{}.{COMMIT}", manager.id())
                );
            }
        }
    }

    #[test]
    fn non_package_versions_preserve_stable_and_preview_identity() {
        assert_eq!(
            format_version(BASE_VERSION, "stable", Some("ignored"), None),
            BASE_VERSION
        );
        assert_eq!(
            format_version(BASE_VERSION, "preview", Some("20260927"), None),
            format!("{BASE_VERSION}-preview.20260927")
        );
        assert_eq!(
            format_version(BASE_VERSION, "preview", None, None),
            format!("{BASE_VERSION}-preview")
        );
        assert_eq!(
            version(),
            format_version(BASE_VERSION, channel(), build_id(), package_identity())
        );
    }
}
