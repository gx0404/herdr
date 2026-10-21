use super::*;

fn settings_state(config: ClientShellConfig) -> ClientShellState {
    let mut state = workbench_state(config);
    state.set_endpoint_methods(Some(vec![
        "client.views.set".into(),
        "tab.focus".into(),
        "agent.activity.read".into(),
        "integration.list".into(),
    ]));
    state
}

#[test]
#[ignore = "manual visual gallery; run `just ui-gallery`"]
fn ui_gallery_settings() {
    let _lang = crate::i18n::lang_guard(crate::i18n::Lang::ZhCn);
    let mut shots = Vec::new();
    let mut step = 1;
    for theme in ["graphite-dark", "graphite-light"] {
        let mut config = Config::default();
        config.theme.name = Some(theme.into());
        let host = if theme == "graphite-light" {
            LIGHT_HOST
        } else {
            DARK_HOST
        };
        for (cols, rows) in [(80, 24), (160, 45)] {
            for section in ClientSettingsSection::ALL {
                let mut state = settings_state(ClientShellConfig::from_config(&config));
                let _ = shoot_workbench(&mut state, cols, rows, &mut step);
                state.open_settings_overlay();
                state.select_settings_section(*section, &mut ClientShellInput::default());
                assert!(
                    state.visible_endpoint_notice.is_none(),
                    "settings fixture must not report unsupported methods"
                );
                if let Some(ClientShellOverlay::Settings(settings)) = state.overlay.as_mut() {
                    settings.focus = crate::client::shell::page::PageFocus::Content;
                    settings.loading_integrations = false;
                    if *section == ClientSettingsSection::Integrations {
                        settings.integrations = vec![
                            crate::api::schema::IntegrationInfo {
                                target: crate::api::schema::IntegrationTarget::Codex,
                                label: "codex".into(),
                                command: "codex".into(),
                                available: true,
                                state: crate::api::schema::IntegrationState::Outdated,
                            },
                            crate::api::schema::IntegrationInfo {
                                target: crate::api::schema::IntegrationTarget::Claude,
                                label: "claude".into(),
                                command: "claude".into(),
                                available: true,
                                state: crate::api::schema::IntegrationState::Current,
                            },
                        ];
                        settings.integration_notice = Some("选中项无需安装".into());
                        settings.integration_messages = vec!["codex: installation complete; restart the agent to activate updated hooks.".into()];
                    }
                }
                shots.push(Shot {
                    title: format!("{theme} {section:?} {cols}x{rows}"),
                    frame: state
                        .compose(cols, rows)
                        .expect("settings frame")
                        .frame
                        .clone(),
                    host,
                });
                if *section == ClientSettingsSection::Theme {
                    assert!(
                        state.visible_endpoint_notice.is_none(),
                        "settings fixture must not report unsupported methods"
                    );
                    if let Some(ClientShellOverlay::Settings(settings)) = state.overlay.as_mut() {
                        settings.selected = crate::config::THEME_NAMES.len() / 2;
                        settings.scroll = 6;
                        settings.reveal = true;
                    }
                    shots.push(Shot {
                        title: format!("{theme} theme scrolled {cols}x{rows}"),
                        frame: state
                            .compose(cols, rows)
                            .expect("scrolled frame")
                            .frame
                            .clone(),
                        host,
                    });
                }
            }
        }
        for loading in [true, false] {
            let mut state = settings_state(ClientShellConfig::from_config(&config));
            let _ = shoot_workbench(&mut state, 80, 24, &mut step);
            state.open_settings_overlay();
            state.select_settings_section(
                ClientSettingsSection::Integrations,
                &mut ClientShellInput::default(),
            );
            assert!(
                state.visible_endpoint_notice.is_none(),
                "settings fixture must not report unsupported methods"
            );
            if let Some(ClientShellOverlay::Settings(settings)) = state.overlay.as_mut() {
                settings.loading_integrations = loading;
            }
            shots.push(Shot {
                title: format!("{theme} integrations loading-{loading} 80x24"),
                frame: state.compose(80, 24).expect("empty frame").frame.clone(),
                host,
            });
        }
        for (cols, rows) in [(80, 24), (160, 45)] {
            let mut state = settings_state(ClientShellConfig::from_config(&config));
            let _ = shoot_workbench(&mut state, cols, rows, &mut step);
            state.open_navigator_overlay();
            if let Some(ClientShellOverlay::Navigator(nav)) = state.overlay.as_mut() {
                nav.overview.grouping = crate::client::shell::chrome::overview::Grouping::Location;
            }
            shots.push(Shot {
                title: format!("{theme} navigator names {cols}x{rows}"),
                frame: state
                    .compose(cols, rows)
                    .expect("navigator frame")
                    .frame
                    .clone(),
                host,
            });
            let mut snapshot = state.snapshot.as_deref().expect("snapshot").clone();
            for pane in &mut snapshot.panes {
                if pane.pane_id == "p3" {
                    pane.label = Some("codex 长名称 e\u{301} 中文会话 review-navigation-label-width-and-truncation".into());
                }
            }
            state.set_snapshot(Box::new(snapshot));
            shots.push(Shot {
                title: format!("{theme} navigator long names {cols}x{rows}"),
                frame: state.compose(cols, rows).expect("long names").frame.clone(),
                host,
            });
            if let Some(ClientShellOverlay::Navigator(nav)) = state.overlay.as_mut() {
                nav.overview.grouping = crate::client::shell::chrome::overview::Grouping::Status;
            }
            shots.push(Shot {
                title: format!("{theme} overview keys {cols}x{rows}"),
                frame: state
                    .compose(cols, rows)
                    .expect("overview keys")
                    .frame
                    .clone(),
                host,
            });
        }
    }
    write_shots("settings", &shots);
}
