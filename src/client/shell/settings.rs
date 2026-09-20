use super::*;
use crossterm::event::{KeyCode, KeyModifiers};

pub(super) fn normalized_theme_name(name: &str) -> String {
    name.to_lowercase().replace([' ', '_'], "-")
}

fn theme_index(name: &str) -> usize {
    let normalized = normalized_theme_name(name);
    crate::config::THEME_NAMES
        .iter()
        .position(|candidate| normalized_theme_name(candidate) == normalized)
        .unwrap_or(0)
}

fn indicator_index(style: crate::config::StatusIndicatorStyle) -> usize {
    usize::from(style == crate::config::StatusIndicatorStyle::Symbols)
}

fn toast_index(delivery: crate::config::ToastDelivery) -> usize {
    match delivery {
        crate::config::ToastDelivery::Off => 0,
        crate::config::ToastDelivery::Herdr => 1,
        crate::config::ToastDelivery::Terminal => 2,
        crate::config::ToastDelivery::System => 3,
    }
}

pub(super) fn integration_needs_install(info: &crate::api::schema::IntegrationInfo) -> bool {
    info.state == crate::api::schema::IntegrationState::Outdated
        || info.available && info.state == crate::api::schema::IntegrationState::NotInstalled
}

impl ClientShellState {
    pub(super) fn open_settings_overlay(&mut self) {
        if matches!(self.overlay, Some(ClientShellOverlay::Settings(_))) {
            return;
        }
        self.overlay = Some(ClientShellOverlay::Settings(ClientSettingsOverlay {
            focus: super::page::PageFocus::Content,
            current: theme_index(&self.config.theme_name),
            scroll: 0,
            reveal: true,
            section: ClientSettingsSection::Theme,
            selected: theme_index(&self.config.theme_name),
            original_theme_name: self.config.theme_name.clone(),
            original_palette: self.config.palette.clone(),
            original_components: self.config.components.clone(),
            integrations: Vec::new(),
            integration_messages: Vec::new(),
            integration_notice: None,
            loading_integrations: false,
            installing_integrations: false,
        }));
    }

    fn selected_index_for_settings_section(&self, section: ClientSettingsSection) -> usize {
        match section {
            ClientSettingsSection::Language => {
                usize::from(crate::i18n::lang() == crate::i18n::Lang::En)
            }
            ClientSettingsSection::Theme => theme_index(&self.config.theme_name),
            ClientSettingsSection::Indicators => indicator_index(self.config.status_indicators),
            ClientSettingsSection::Sound => usize::from(!self.config.sound_enabled),
            ClientSettingsSection::Toast => toast_index(self.config.toast_delivery),
            ClientSettingsSection::Integrations => 0,
        }
    }

    pub(super) fn select_settings_section(
        &mut self,
        section: ClientSettingsSection,
        outcome: &mut ClientShellInput,
    ) {
        let selected = self.selected_index_for_settings_section(section);
        let request_integrations = matches!(section, ClientSettingsSection::Integrations)
            && matches!(
                self.overlay,
                Some(ClientShellOverlay::Settings(ClientSettingsOverlay {
                    loading_integrations: false,
                    installing_integrations: false,
                    ..
                }))
            );
        if let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() {
            if settings.section == ClientSettingsSection::Theme {
                self.config
                    .theme_name
                    .clone_from(&settings.original_theme_name);
                self.config.palette = settings.original_palette.clone();
                self.config.components = settings.original_components.clone();
            }
            settings.section = section;
            settings.selected = selected;
            settings.current = selected;
            settings.scroll = 0;
            settings.reveal = true;
            settings.focus = super::page::PageFocus::Navigation;
        }
        if request_integrations {
            self.queue_integration_list(outcome, true);
        }
        outcome.repaint = true;
    }

    fn move_settings_section(&mut self, delta: isize, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_ref() else {
            return;
        };
        let current = ClientSettingsSection::ALL
            .iter()
            .position(|section| *section == settings.section)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(ClientSettingsSection::ALL.len() as isize)
            as usize;
        self.select_settings_section(ClientSettingsSection::ALL[next], outcome);
    }

    fn settings_choice_count(&self) -> usize {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::Settings(settings)) => match settings.section {
                ClientSettingsSection::Language => 2,
                ClientSettingsSection::Theme => crate::config::THEME_NAMES.len(),
                ClientSettingsSection::Indicators | ClientSettingsSection::Sound => 2,
                ClientSettingsSection::Toast => 4,
                ClientSettingsSection::Integrations => {
                    settings.integrations.len() + settings.integration_messages.len()
                }
            },
            _ => 0,
        }
    }

    pub(super) fn move_settings_selection(&mut self, delta: isize) {
        let count = self.settings_choice_count();
        let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() else {
            return;
        };
        if count == 0 {
            settings.selected = 0;
            return;
        }
        settings.focus = super::page::PageFocus::Content;
        settings.reveal = true;
        settings.selected = (settings.selected as isize + delta)
            .clamp(0, count.saturating_sub(1) as isize) as usize;
        if settings.section == ClientSettingsSection::Theme {
            self.preview_selected_theme();
        }
    }

    pub(super) fn select_settings_choice(&mut self, index: usize) {
        let count = self.settings_choice_count();
        if let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() {
            settings.reveal = true;
            settings.focus = super::page::PageFocus::Content;
            if count > 0 {
                settings.selected = index.min(count - 1);
            }
        }
        if matches!(
            self.overlay,
            Some(ClientShellOverlay::Settings(ClientSettingsOverlay {
                section: ClientSettingsSection::Theme,
                ..
            }))
        ) {
            self.preview_selected_theme();
        }
    }

    fn preview_selected_theme(&mut self) {
        let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_ref() else {
            return;
        };
        let Some(name) = crate::config::THEME_NAMES.get(settings.selected) else {
            return;
        };
        // Preview through the same resolution seam as startup: manual name,
        // host-detected depth, and component tokens all follow the candidate.
        let mut runtime = self.config.theme_runtime.clone();
        runtime.manual_name = (*name).to_owned();
        runtime.auto_switch = false;
        let resolved =
            crate::app::client_resolved_theme(&runtime, None, self.config.host_color_depth);
        self.config.theme_name = (*name).to_owned();
        self.config.palette = resolved.palette;
        self.config.components = resolved.components;
    }

    pub(super) fn cancel_settings_overlay(&mut self) {
        let Some(ClientShellOverlay::Settings(settings)) = self.overlay.take() else {
            return;
        };
        self.config.theme_name = settings.original_theme_name;
        self.config.palette = settings.original_palette;
        self.config.components = settings.original_components;
    }

    fn save_settings_edit(
        &mut self,
        edit: crate::config::ConfigEdit<'_>,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if let Err(error) = crate::config::write_edit(edit) {
            self.set_endpoint_error(error);
            outcome.repaint = true;
            return false;
        }
        self.reload_client_config();
        if let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() {
            settings.current = settings.selected;
        }
        self.push_endpoint_method_with_kind(
            crate::api::schema::Method::ServerReloadConfig(
                crate::api::schema::EmptyParams::default(),
            ),
            PendingEndpointKind::ReloadConfig,
            outcome,
        );
        outcome.repaint = true;
        true
    }

    pub(super) fn apply_settings_choice(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_ref() else {
            return;
        };
        let section = settings.section;
        let selected = settings.selected;
        match section {
            ClientSettingsSection::Language => {
                let lang = if selected == 0 {
                    crate::i18n::Lang::ZhCn
                } else {
                    crate::i18n::Lang::En
                };
                crate::i18n::set_lang(lang);
                self.save_settings_edit(crate::config::ConfigEdit::Language(lang), outcome);
            }
            ClientSettingsSection::Theme => {
                let Some(name) = crate::config::THEME_NAMES.get(selected).copied() else {
                    return;
                };
                if self.save_settings_edit(crate::config::ConfigEdit::Theme(name), outcome) {
                    self.overlay = None;
                }
            }
            ClientSettingsSection::Indicators => {
                let style = if selected == 0 {
                    crate::config::StatusIndicatorStyle::Dots
                } else {
                    crate::config::StatusIndicatorStyle::Symbols
                };
                self.save_settings_edit(
                    crate::config::ConfigEdit::StatusIndicators(style),
                    outcome,
                );
            }
            ClientSettingsSection::Sound => {
                self.save_settings_edit(crate::config::ConfigEdit::Sound(selected == 0), outcome);
            }
            ClientSettingsSection::Toast => {
                let delivery = match selected {
                    0 => crate::config::ToastDelivery::Off,
                    1 => crate::config::ToastDelivery::Herdr,
                    2 => crate::config::ToastDelivery::Terminal,
                    _ => crate::config::ToastDelivery::System,
                };
                self.save_settings_edit(
                    crate::config::ConfigEdit::ToastDelivery(delivery),
                    outcome,
                );
            }
            // TOOL-04：Enter 只安装当前选中行；批量安装走显式的 `a` 键。
            ClientSettingsSection::Integrations => self.install_selected_integration(outcome),
        }
    }

    fn queue_integration_list(&mut self, outcome: &mut ClientShellInput, clear_messages: bool) {
        if let Some(ClientShellOverlay::Settings(settings)) = self.content_page_mut() {
            settings.loading_integrations = true;
            settings.integration_notice = None;
            if clear_messages {
                settings.integration_messages.clear();
            }
        }
        if !self.push_endpoint_method_with_kind(
            crate::api::schema::Method::IntegrationList(crate::api::schema::EmptyParams::default()),
            PendingEndpointKind::IntegrationList,
            outcome,
        ) {
            if let Some(ClientShellOverlay::Settings(settings)) = self.content_page_mut() {
                settings.loading_integrations = false;
            }
        }
    }

    /// Enter / 主按钮：只安装选中行。批量安装会写多个 agent 的宿主配置文件，
    /// 不能由逐行选中语义的同一个键顺带触发（TOOL-04）。
    fn install_selected_integration(&mut self, outcome: &mut ClientShellInput) {
        if self.pending_integration_installs > 0 {
            return;
        }
        let target = match self.content_page() {
            Some(ClientShellOverlay::Settings(settings)) => settings
                .integrations
                .get(settings.selected)
                .filter(|integration| integration_needs_install(integration))
                .map(|integration| integration.target),
            _ => return,
        };
        let Some(target) = target else {
            // 选中的是已安装行、不可用行，或落在消息区：给一行反馈，
            // 绝不退化成「安装全部」。反馈写独立的提示字段——`integration_messages`
            // 装的是上一次安装由服务端返回的结果，用户可能正在读它。
            if let Some(ClientShellOverlay::Settings(settings)) = self.content_page_mut() {
                settings.integration_notice =
                    Some(crate::i18n::texts().settings.nothing_to_install.to_owned());
            }
            outcome.repaint = true;
            return;
        };
        self.install_integration_targets(vec![target], outcome);
    }

    fn install_recommended_integrations(&mut self, outcome: &mut ClientShellInput) {
        if self.pending_integration_installs > 0 {
            return;
        }
        let targets = match self.content_page() {
            Some(ClientShellOverlay::Settings(settings)) => settings
                .integrations
                .iter()
                .filter(|integration| integration_needs_install(integration))
                .map(|integration| integration.target)
                .collect::<Vec<_>>(),
            _ => return,
        };
        self.install_integration_targets(targets, outcome);
    }

    /// 共享派发：把一批目标排进 `integration.install`，并维护在途计数。
    fn install_integration_targets(
        &mut self,
        targets: Vec<crate::api::schema::IntegrationTarget>,
        outcome: &mut ClientShellInput,
    ) {
        if targets.is_empty() {
            return;
        }
        if let Some(ClientShellOverlay::Settings(settings)) = self.content_page_mut() {
            settings.installing_integrations = true;
            settings.integration_messages.clear();
            settings.integration_notice = None;
        }
        self.pending_integration_installs = 0;
        for target in targets {
            if self.push_endpoint_method_with_kind(
                crate::api::schema::Method::IntegrationInstall(
                    crate::api::schema::IntegrationInstallParams { target },
                ),
                PendingEndpointKind::IntegrationInstall,
                outcome,
            ) {
                self.pending_integration_installs += 1;
            }
        }
        if self.pending_integration_installs == 0 {
            if let Some(ClientShellOverlay::Settings(settings)) = self.content_page_mut() {
                settings.installing_integrations = false;
            }
        }
        outcome.repaint = true;
    }

    pub(super) fn handle_settings_endpoint_result(
        &mut self,
        kind: PendingEndpointKind,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        match kind {
            PendingEndpointKind::IntegrationList => {
                if let Some(ClientShellOverlay::Settings(settings)) = self.content_page_mut() {
                    settings.loading_integrations = false;
                    match result {
                        Ok(crate::api::schema::ResponseResult::IntegrationList {
                            integrations,
                        }) => {
                            settings.integrations = integrations;
                            settings.selected = settings
                                .selected
                                .min(settings.integrations.len().saturating_sub(1));
                        }
                        Ok(_) => {
                            self.set_endpoint_error(
                                crate::i18n::texts().settings.unexpected_list_result,
                            );
                        }
                        Err(_) => {}
                    }
                }
                (true, Vec::new())
            }
            PendingEndpointKind::IntegrationInstall => {
                let cancelled = result
                    .as_ref()
                    .is_err_and(|error| error.code.as_deref() == Some("endpoint_cancelled"));
                self.pending_integration_installs =
                    self.pending_integration_installs.saturating_sub(1);
                let installing = self.pending_integration_installs > 0;
                if let Some(ClientShellOverlay::Settings(settings)) = self.content_page_mut() {
                    match result {
                        Ok(crate::api::schema::ResponseResult::IntegrationInstall {
                            details,
                            ..
                        }) => settings.integration_messages.extend(details.messages),
                        Ok(_) => settings.integration_messages.push(
                            crate::i18n::texts()
                                .settings
                                .unexpected_install_result
                                .into(),
                        ),
                        Err(error) => settings.integration_messages.push(error.message),
                    }
                    settings.installing_integrations = installing;
                }
                let actions = if !cancelled
                    && self.pending_integration_installs == 0
                    && matches!(self.content_page(), Some(ClientShellOverlay::Settings(_)))
                {
                    let mut deferred = ClientShellInput::default();
                    self.queue_integration_list(&mut deferred, false);
                    deferred.actions
                } else {
                    Vec::new()
                };
                (true, actions)
            }
            _ => (false, Vec::new()),
        }
    }

    pub(super) fn scroll_settings(&mut self, delta: isize) {
        if let Some(ClientShellOverlay::Settings(settings)) = self.content_page_mut() {
            settings.scroll = settings.scroll.saturating_add_signed(delta);
            settings.reveal = false;
        }
    }

    pub(super) fn route_settings_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::Settings(_))) {
            return false;
        }
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        if code == KeyCode::Esc {
            if !matches!(
                self.overlay,
                Some(ClientShellOverlay::Settings(ClientSettingsOverlay {
                    installing_integrations: true,
                    ..
                }))
            ) {
                self.cancel_settings_overlay();
                outcome.repaint = true;
            }
            return true;
        }
        if matches!(code, KeyCode::Tab | KeyCode::Right | KeyCode::Char('l'))
            && modifiers.is_empty()
        {
            self.move_settings_section(1, outcome);
            return true;
        }
        if matches!(code, KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h'))
            && modifiers.difference(KeyModifiers::SHIFT).is_empty()
        {
            self.move_settings_section(-1, outcome);
            return true;
        }
        if matches!(code, KeyCode::Up | KeyCode::Char('k')) && modifiers.is_empty() {
            self.move_settings_selection(-1);
            outcome.repaint = true;
            return true;
        }
        if matches!(code, KeyCode::Down | KeyCode::Char('j')) && modifiers.is_empty() {
            self.move_settings_selection(1);
            outcome.repaint = true;
            return true;
        }
        if matches!(code, KeyCode::PageUp | KeyCode::PageDown) {
            self.move_settings_selection(if code == KeyCode::PageUp { -8 } else { 8 });
            outcome.repaint = true;
            return true;
        }
        // TOOL-04：批量安装的显式入口，只在 Integrations 分区生效。
        if code == KeyCode::Char('a')
            && modifiers.is_empty()
            && matches!(
                self.overlay,
                Some(ClientShellOverlay::Settings(ClientSettingsOverlay {
                    section: ClientSettingsSection::Integrations,
                    ..
                }))
            )
        {
            self.install_recommended_integrations(outcome);
            outcome.repaint = true;
            return true;
        }
        if matches!(code, KeyCode::Enter | KeyCode::Char(' ')) && modifiers.is_empty() {
            if let Some(ClientShellOverlay::Settings(settings)) = self.overlay.as_mut() {
                settings.focus = super::page::PageFocus::Actions;
            }
            self.apply_settings_choice(outcome);
            return true;
        }
        true
    }
}
