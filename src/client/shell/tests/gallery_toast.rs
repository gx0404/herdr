//! 吐司画廊：需要输入（告警）、失败（错误）、完成（成功）、更新（信息）、端点通知与
//! 语义通知堆叠、复制反馈与 60x20 窄屏，深浅两套主题。

use super::*;

fn toast_event(
    kind: SemanticNotificationKind,
    title: &str,
    body: &str,
    sound: Option<SemanticNotificationSound>,
) -> ClientVisibleNotification {
    ClientVisibleNotification {
        endpoint_id: ClientEndpointId::Local,
        event: SemanticNotification {
            kind,
            title: title.into(),
            body: (!body.is_empty()).then(|| body.into()),
            sound,
            agent: Some("codex".into()),
            workspace_id: Some("ws1".into()),
            tab_id: Some("ws1-t2".into()),
            pane_id: Some("p3".into()),
            position: None,
        },
        deadline: Instant::now() + Duration::from_secs(60),
    }
}

fn toast_notice(
    kind: ClientEndpointNoticeKind,
    title: &str,
    body: &str,
) -> ClientVisibleEndpointNotice {
    ClientVisibleEndpointNotice {
        key: ClientEndpointNoticeKey {
            endpoint_id: ClientEndpointId::Local,
            boot_id: "boot-1".into(),
            kind,
            code: "gallery".into(),
        },
        title: title.into(),
        body: body.into(),
        deadline: Instant::now() + Duration::from_secs(60),
    }
}

type ToastScene = fn(&mut ClientShellState);

#[test]
#[ignore = "manual visual gallery; run `just ui-gallery`"]
fn ui_gallery_toast() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let scenes: [(&str, ToastScene); 9] = [
        ("needsinput", |state| {
            state.visible_notification = Some(toast_event(
                SemanticNotificationKind::NeedsAttention,
                "codex 需要你的输入",
                "允许执行 cargo nextest 并修改 src/client/shell/state.rs？远端智能体正在等待本次操作。",
                Some(SemanticNotificationSound::Request),
            ));
        }),
        ("needsinput focused", |state| {
            state.visible_notification = Some(toast_event(
                SemanticNotificationKind::NeedsAttention,
                "codex 需要你的输入",
                "允许执行 cargo nextest 并修改 src/client/shell/state.rs？",
                Some(SemanticNotificationSound::Request),
            ));
            state.notification_focused = true;
        }),
        ("failed", |state| {
            state.visible_notification = Some(toast_event(
                SemanticNotificationKind::Custom,
                "codex 运行失败",
                "远端工具运行失败：无法写入 src/client/shell/state.rs，请检查文件权限。",
                Some(SemanticNotificationSound::Request),
            ));
        }),
        ("finished", |state| {
            state.visible_notification = Some(toast_event(
                SemanticNotificationKind::Finished,
                "codex 已完成",
                "重构 Agent 活动窗口并补齐 Codex 适配",
                Some(SemanticNotificationSound::Done),
            ));
        }),
        ("update", |state| {
            let mut event = toast_event(
                SemanticNotificationKind::UpdateInstalled,
                "herdr 已更新到 0.9.6",
                "重启 server 后生效",
                None,
            );
            event.event.agent = None;
            event.event.pane_id = None;
            state.visible_notification = Some(event);
        }),
        ("stacked", |state| {
            state.visible_notification = Some(toast_event(
                SemanticNotificationKind::NeedsAttention,
                "codex 需要你的输入",
                "允许执行 cargo nextest？",
                None,
            ));
            state.visible_endpoint_notice = Some(toast_notice(
                ClientEndpointNoticeKind::Rejected,
                "工作区切换失败",
                "目标工作区已不存在：ws-removed",
            ));
        }),
        ("endpoint warning", |state| {
            state.visible_endpoint_notice = Some(toast_notice(
                ClientEndpointNoticeKind::Warning,
                "磁盘空间不足",
                "Remote 剩余 3% 空间，构建可能失败",
            ));
        }),
        ("over settings", |state| {
            state.open_settings_overlay();
            state.visible_endpoint_notice = Some(toast_notice(
                ClientEndpointNoticeKind::Success,
                "已保存设置",
                "主题与吐司位置已写入 config.toml",
            ));
        }),
        ("copy feedback", |state| {
            state.copy_feedback = Some(crate::app::state::CopyFeedback {
                message: "已复制 42 行到剪贴板".into(),
            });
        }),
    ];
    let mut shots = Vec::new();
    let mut step = 0;
    for theme in ["graphite-dark", "graphite-light"] {
        let host = if theme == "graphite-light" {
            LIGHT_HOST
        } else {
            DARK_HOST
        };
        let mut themed = Config::default();
        themed.theme.name = Some(theme.into());
        for (name, scene) in scenes {
            for (cols, rows) in [(160, 45), (60, 20)] {
                let mut state = workbench_state(ClientShellConfig::from_config(&themed));
                let _ = shoot_workbench(&mut state, cols, rows, &mut step);
                state.config.feedback.animations = false;
                state.toast_since = None;
                scene(&mut state);
                shots.push(Shot {
                    title: format!("toast {name} {theme} {cols}x{rows}"),
                    frame: state
                        .compose(cols, rows)
                        .expect("toast frame")
                        .frame
                        .clone(),
                    host,
                });
            }
        }
        // 经典布局：模式条与底部之上，两张吐司堆叠；顶部位置从标签栏下开始。
        for (position, label) in [
            (crate::config::ToastHerdrPosition::BottomRight, "bottom"),
            (crate::config::ToastHerdrPosition::TopLeft, "top"),
        ] {
            let mut state = classic_state(ClientShellConfig::from_config(&themed));
            state.config.toast_position = position;
            state.config.feedback.animations = false;
            state.mode = ClientShellMode::Prefix;
            state.config.which_key = false;
            (scenes[5].1)(&mut state);
            shots.push(Shot {
                title: format!("toast classic stacked {label} {theme} 80x24"),
                frame: shoot_classic(&mut state, 80, 24),
                host,
            });
        }
    }
    write_shots("toast", &shots);
}
