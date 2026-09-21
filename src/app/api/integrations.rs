use crate::api::schema::{
    IntegrationInfo, IntegrationInstallResult, IntegrationState, IntegrationUninstallResult,
    ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_integration_list(&self, id: String) -> String {
        let integrations = crate::integration::integration_recommendations()
            .into_iter()
            .map(|recommendation| IntegrationInfo {
                target: recommendation.target,
                label: recommendation.label.to_owned(),
                command: recommendation.command.to_owned(),
                available: recommendation.available,
                state: match recommendation.state {
                    crate::integration::IntegrationStatusKind::NotInstalled => {
                        IntegrationState::NotInstalled
                    }
                    crate::integration::IntegrationStatusKind::Current => IntegrationState::Current,
                    crate::integration::IntegrationStatusKind::Outdated => {
                        IntegrationState::Outdated
                    }
                },
            })
            .collect();
        encode_success(id, ResponseResult::IntegrationList { integrations })
    }

    pub(super) fn handle_integration_install(
        &mut self,
        id: String,
        params: crate::api::schema::IntegrationInstallParams,
    ) -> String {
        let target = params.target;
        if target.is_retired() {
            return encode_retired_integration(id, "install", target);
        }
        let messages = match crate::integration::install_target(target) {
            Ok(messages) => messages,
            Err(err) => return encode_error(id, "integration_install_failed", err.to_string()),
        };
        self.state.integration_recommendations = crate::integration::integration_recommendations();
        // integration_recommendations 进入 ClientShell 投影（HSR-05 写入点）。
        self.state.bump_projection_epoch();

        encode_success(
            id,
            ResponseResult::IntegrationInstall {
                target,
                details: IntegrationInstallResult { messages },
            },
        )
    }

    pub(super) fn handle_integration_uninstall(
        &mut self,
        id: String,
        params: crate::api::schema::IntegrationUninstallParams,
    ) -> String {
        let target = params.target;
        if target.is_retired() {
            return encode_retired_integration(id, "uninstall", target);
        }
        let messages = match crate::integration::uninstall_target(target) {
            Ok(messages) => messages,
            Err(err) => return encode_error(id, "integration_uninstall_failed", err.to_string()),
        };
        self.state.integration_recommendations = crate::integration::integration_recommendations();
        // integration_recommendations 进入 ClientShell 投影（HSR-05 写入点）。
        self.state.bump_projection_epoch();

        encode_success(
            id,
            ResponseResult::IntegrationUninstall {
                target,
                details: IntegrationUninstallResult { messages },
            },
        )
    }
}

/// 旧客户端仍可能按名字传来本 fork 已退役的 target：冻结枚举保证它能反序列化，
/// 这里回一个独立错误码，调用方据此区分「已退役」与普通安装失败。
fn encode_retired_integration(
    id: String,
    action: &'static str,
    target: crate::api::schema::IntegrationTarget,
) -> String {
    encode_error(
        id,
        "integration_retired",
        crate::integration::retired_integration_error(action, target).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use crate::app::App;

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    #[test]
    fn retired_target_from_an_old_client_gets_a_retired_error_not_a_parse_failure() {
        let _lang = crate::i18n::lang_guard(crate::i18n::Lang::En);
        let mut app = test_app();
        let epoch = app.state.projection_epoch;

        for method in ["integration.install", "integration.uninstall"] {
            // 旧客户端按名字发来 fork 已退役的 target：请求必须照常解析，再得到退役错误。
            let request: crate::api::schema::Request = serde_json::from_value(serde_json::json!({
                "id": "legacy",
                "method": method,
                "params": { "target": "cursor" },
            }))
            .expect("退役 target 仍须能反序列化");
            let response: serde_json::Value =
                serde_json::from_str(&app.handle_api_request(request)).unwrap();

            assert_eq!(response["id"], "legacy");
            assert_eq!(response["error"]["code"], "integration_retired", "{method}");
            let message = response["error"]["message"].as_str().unwrap();
            assert!(
                message.contains("cursor") && message.contains("retired"),
                "{message}"
            );
        }
        assert_eq!(
            app.state.projection_epoch, epoch,
            "退役错误不改动投影可见状态"
        );
    }
}
