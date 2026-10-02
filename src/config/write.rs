#[derive(Clone, Copy)]
pub(crate) enum ConfigEdit<'a> {
    Theme(&'a str),
    Language(crate::i18n::Lang),
    StatusIndicators(super::StatusIndicatorStyle),
    Sound(bool),
    ToastDelivery(super::ToastDelivery),
}

impl ConfigEdit<'_> {
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Theme(_) => "theme",
            Self::Language(_) => "language setting",
            Self::StatusIndicators(_) => "status indicators",
            Self::Sound(_) => "sound setting",
            Self::ToastDelivery(_) => "toast setting",
        }
    }

    pub(crate) fn apply(self, content: &str) -> String {
        match self {
            Self::Theme(name) => {
                let content =
                    super::upsert_section_value(content, "theme", "name", &format!("\"{name}\""));
                super::upsert_section_bool(&content, "theme", "auto_switch", false)
            }
            Self::Language(lang) => super::upsert_top_level_value(
                content,
                "language",
                &format!("\"{}\"", lang.as_str()),
            ),
            Self::StatusIndicators(style) => super::upsert_section_value(
                content,
                "ui",
                "status_indicators",
                &format!("\"{}\"", style.as_str()),
            ),
            Self::Sound(enabled) => {
                super::upsert_section_bool(content, "ui.sound", "enabled", enabled)
            }
            Self::ToastDelivery(delivery) => {
                let value = match delivery {
                    super::ToastDelivery::Off => "\"off\"",
                    super::ToastDelivery::Herdr => "\"herdr\"",
                    super::ToastDelivery::Terminal => "\"terminal\"",
                    super::ToastDelivery::System => "\"system\"",
                };
                let content = super::upsert_section_value(content, "ui.toast", "delivery", value);
                super::remove_section_key(&content, "ui.toast", "enabled")
            }
        }
    }
}

static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Apply a config edit through a staged replacement, never by truncating the
/// live file in place.
///
/// The target is resolved through at most 40 symlink hops, a unique temporary is
/// created beside the resolved file, existing permissions are copied, and the
/// bytes are written and synced before `platform::replace_file` swaps the file.
/// The parent directory is synced after the swap. Failures before replacement
/// remove the temporary; a post-replacement directory-sync error is reported
/// without undoing the already completed swap.
pub(crate) fn update_file_at(
    path: &std::path::Path,
    description: &str,
    update: impl FnOnce(&str) -> String,
) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create config directory: {error}"))?;
    }
    let content = match super::io::read_optional_config(path) {
        Ok(Some(content)) => content,
        Ok(None) => String::new(),
        Err(error) => {
            return Err(format!(
                "failed to read config before saving {description}: {error}"
            ));
        }
    };
    let updated = update(&content);
    let target = resolve_target(path).map_err(|error| {
        format!("failed to resolve config before saving {description}: {error}")
    })?;
    let existing = match std::fs::metadata(&target) {
        Ok(metadata) if metadata.is_file() => Some(metadata),
        Ok(_) => {
            return Err(format!(
                "failed to save {description}: config target is not a regular file"
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("failed to save {description}: {error}")),
    };
    if existing.is_some() {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&target)
            .map_err(|error| format!("failed to save {description}: {error}"))?;
    }

    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let (temporary, output) = create_temporary(parent, existing.is_some())
        .map_err(|error| format!("failed to save {description}: {error}"))?;
    let mut output = Some(output);
    let result = (|| {
        if let Some(metadata) = existing {
            std::fs::set_permissions(&temporary, metadata.permissions())?;
        }
        use std::io::Write;
        let output_ref = output
            .as_mut()
            .ok_or_else(|| std::io::Error::other("config temporary file was closed"))?;
        output_ref.write_all(updated.as_bytes())?;
        output_ref.sync_all()?;
        drop(output.take());
        crate::platform::replace_file(&temporary, &target)?;
        crate::platform::sync_parent_directory(parent)
    })();
    drop(output);
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("failed to save {description}: {error}"));
    }
    Ok(())
}

fn create_temporary(
    parent: &std::path::Path,
    private: bool,
) -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    for _ in 0..128 {
        let sequence = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".herdr-config-{}-{sequence}.tmp",
            std::process::id()
        ));
        match crate::platform::create_config_temporary(&temporary, private) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique config temporary file",
    ))
}

fn resolve_target(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = std::fs::read_link(&current)?;
                current = if link.is_absolute() {
                    link
                } else {
                    current
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new("."))
                        .join(link)
                };
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(std::io::Error::other("config target is not a regular file"));
            }
            Ok(_) => return Ok(current),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other("too many symbolic links"))
}

pub(crate) fn write_edit(edit: ConfigEdit<'_>) -> Result<(), String> {
    update_file_at(&super::config_path(), edit.description(), |content| {
        edit.apply(content)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_file_at_does_not_move_a_leading_bom_into_the_file() {
        let dir = std::env::temp_dir().join(format!("herdr-config-bom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            b"\xEF\xBB\xBF[terminal]\ndefault_shell = \"pwsh.exe\"\n",
        )
        .unwrap();

        update_file_at(&path, "onboarding setting", |content| {
            crate::config::upsert_top_level_bool(content, "onboarding", false)
        })
        .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(dir);

        assert!(
            !written.contains('\u{feff}'),
            "unexpected BOM in {written:?}"
        );
        assert!(
            toml::from_str::<toml::Value>(&written).is_ok(),
            "written config is not valid TOML: {written:?}"
        );
    }

    #[test]
    fn update_file_at_replaces_existing_file_without_leaving_a_temporary() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-config-atomic-{}-{}",
            std::process::id(),
            crate::config::test_dirs::unique_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, b"old").unwrap();

        update_file_at(&path, "atomic setting", |_| "new".to_string()).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn update_file_at_updates_a_symlink_target_without_replacing_the_link() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-config-symlink-{}-{}",
            std::process::id(),
            crate::config::test_dirs::unique_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.toml");
        let link = dir.join("config.toml");
        std::fs::write(&target, b"old").unwrap();
        std::os::unix::fs::symlink("target.toml", &link).unwrap();

        update_file_at(&link, "symlink setting", |_| "new".to_string()).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            std::path::Path::new("target.toml")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
