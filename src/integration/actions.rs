use std::io;

use super::codex_trust::codex_hooks_trust_summary;
use super::registry::{integration_target_label, retired_integration_error};
use super::targets::{
    codex_managed_hooks, install_claude, install_codex, install_kimi, install_opencode, install_pi,
    uninstall_claude, uninstall_codex, uninstall_kimi, uninstall_opencode, uninstall_pi,
};
use super::version::{agent_version_requirement, enforce_agent_version};
use super::KIMI_MIN_VERSION;

/// codex 还没信任（或信任后改过）herdr 的钩子时的安装提示，逐条作为安装消息输出：
/// 第一条是要做的选择，第二条是不选的后果。设置页集成页把安装消息逐条单行显示、
/// 按宽度硬截断（80 列终端下消息区只有 74 列），所以动作放在最前、每条不超过 70 列。
pub(crate) const CODEX_HOOKS_REVIEW_HINTS: [&str; 2] = [
    "codex: at \"Hooks need review\" choose \"Trust all and continue\"",
    "codex: \"Continue without trusting\" leaves the herdr hooks disabled",
];
/// 用户在 codex 里停用了 herdr 的钩子时的安装提示（同样不超过 70 列）。
pub(crate) const CODEX_HOOKS_DISABLED_HINT: &str =
    "codex has disabled the herdr hooks; enable them in codex with /hooks";

pub(crate) fn install_target(
    target: crate::api::schema::IntegrationTarget,
) -> io::Result<Vec<String>> {
    let result = install_target_inner(target);
    let outcome = if result.is_ok() { "ok" } else { "error" };
    crate::logging::integration_action("install", integration_target_label(target), outcome);
    result
}

fn install_target_inner(target: crate::api::schema::IntegrationTarget) -> io::Result<Vec<String>> {
    // 退役门是唯一的不可用判断：官方六家集成在所有平台都受支持
    // （`registry::integration_target_supported` 恒等于 `!target.is_retired()`），
    // 因此退役分支之后不再需要平台支持检查。
    if target.is_retired() {
        return Err(retired_integration_error("install", target));
    }

    let version_warning = match agent_version_requirement(target) {
        Some(requirement) => enforce_agent_version(&requirement)?,
        None => None,
    };

    let mut messages = match target {
        crate::api::schema::IntegrationTarget::Pi => {
            let path = install_pi()?;
            vec![format!("installed pi integration to {}", path.display())]
        }
        crate::api::schema::IntegrationTarget::Claude => {
            let installed = install_claude()?;
            vec![
                format!(
                    "installed claude integration hook to {}",
                    installed.hook_path.display()
                ),
                format!(
                    "ensured claude settings at {}",
                    installed.settings_path.display()
                ),
            ]
        }
        crate::api::schema::IntegrationTarget::Codex => {
            let installed = install_codex()?;
            let mut messages = vec![
                format!(
                    "installed codex integration hook to {}",
                    installed.hook_path.display()
                ),
                format!("ensured codex hooks at {}", installed.hooks_path.display()),
                format!(
                    "ensured codex config at {}",
                    installed.config_path.display()
                ),
            ];
            // codex 只运行用户信任过的钩子：新装或改过的钩子要在它下次启动时的
            // 「Hooks need review」里信任，否则静默不跑。
            let trust = codex_hooks_trust_summary(
                &installed.hooks_path,
                &installed.config_path,
                &codex_managed_hooks(&installed.hook_path),
            );
            if trust.needs_review {
                messages.extend(CODEX_HOOKS_REVIEW_HINTS.map(str::to_owned));
            } else if trust.disabled {
                messages.push(CODEX_HOOKS_DISABLED_HINT.to_string());
            }
            messages
        }
        crate::api::schema::IntegrationTarget::Kimi => {
            let installed = install_kimi()?;
            vec![
                format!(
                    "installed kimi integration hook to {}",
                    installed.hook_path.display()
                ),
                format!("ensured kimi config at {}", installed.config_path.display()),
                format!("requires kimi code {KIMI_MIN_VERSION} or newer"),
            ]
        }
        crate::api::schema::IntegrationTarget::Opencode => {
            let installed = install_opencode()?;
            let mut messages = vec![
                format!(
                    "installed opencode integration plugin to {}",
                    installed.plugin_path.display()
                ),
                format!(
                    "installed opencode tui integration plugin to {}",
                    installed.tui_plugin_path.display()
                ),
                format!(
                    "ensured opencode tui plugin config at {}",
                    installed.tui_config_path.display()
                ),
            ];
            if installed.cli_config_path.is_none() {
                messages.push(
                    "to enable OpenCode V2, start opencode2 once, then reinstall this integration"
                        .to_string(),
                );
            }
            messages
        }
        // 统一退役分支：入口已拦下退役变体，这里只为穷尽性兜底，保持同一错误。
        _ => return Err(retired_integration_error("install", target)),
    };

    if let Some(warning) = version_warning {
        messages.push(warning);
    }

    Ok(messages)
}

pub(crate) fn uninstall_target(
    target: crate::api::schema::IntegrationTarget,
) -> io::Result<Vec<String>> {
    if target.is_retired() {
        return Err(retired_integration_error("uninstall", target));
    }

    let messages = match target {
        crate::api::schema::IntegrationTarget::Pi => {
            let result = uninstall_pi()?;
            if result.removed_extension {
                vec![format!(
                    "removed pi integration extension at {}",
                    result.extension_path.display()
                )]
            } else {
                vec![format!(
                    "no pi integration extension found at {}",
                    result.extension_path.display()
                )]
            }
        }
        crate::api::schema::IntegrationTarget::Claude => {
            let result = uninstall_claude()?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed claude hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no claude hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_settings {
                messages.push(format!(
                    "removed herdr claude hook entries from {}",
                    result.settings_path.display()
                ));
            } else {
                messages.push(format!(
                    "no herdr claude hook entries found in {}",
                    result.settings_path.display()
                ));
            }
            messages
        }
        crate::api::schema::IntegrationTarget::Codex => {
            let result = uninstall_codex()?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed codex hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no codex hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_hooks {
                messages.push(format!(
                    "removed herdr codex hook entries from {}",
                    result.hooks_path.display()
                ));
            } else {
                messages.push(format!(
                    "no herdr codex hook entries found in {}",
                    result.hooks_path.display()
                ));
            }
            messages.push(format!(
                "left codex config unchanged at {}",
                result.config_path.display()
            ));
            messages
        }
        crate::api::schema::IntegrationTarget::Kimi => {
            let result = uninstall_kimi()?;
            let mut messages = Vec::new();
            if result.removed_hook_file {
                messages.push(format!(
                    "removed kimi hook at {}",
                    result.hook_path.display()
                ));
            } else {
                messages.push(format!(
                    "no kimi hook found at {}",
                    result.hook_path.display()
                ));
            }
            if result.updated_config {
                messages.push(format!(
                    "removed herdr kimi hook entries from {}",
                    result.config_path.display()
                ));
            } else {
                messages.push(format!(
                    "no herdr kimi hook entries found in {}",
                    result.config_path.display()
                ));
            }
            messages
        }
        crate::api::schema::IntegrationTarget::Opencode => {
            let result = uninstall_opencode()?;
            let mut messages = vec![if result.removed_plugin {
                format!(
                    "removed opencode integration plugin at {}",
                    result.plugin_path.display()
                )
            } else {
                format!(
                    "no opencode integration plugin found at {}",
                    result.plugin_path.display()
                )
            }];
            messages.push(if result.removed_tui_plugin {
                format!(
                    "removed opencode tui integration plugin at {}",
                    result.tui_plugin_path.display()
                )
            } else {
                format!(
                    "no opencode tui integration plugin found at {}",
                    result.tui_plugin_path.display()
                )
            });
            for path in result.updated_tui_configs {
                messages.push(format!(
                    "removed herdr opencode plugin entry from {}",
                    path.display()
                ));
            }
            messages
        }
        // 统一退役分支：同上，入口已拦下，这里只为穷尽性兜底。
        _ => return Err(retired_integration_error("uninstall", target)),
    };

    crate::logging::integration_action("uninstall", integration_target_label(target), "ok");
    Ok(messages)
}
