use super::*;

pub(super) struct ClientLoopConfig {
    pub(super) sound_config: crate::config::SoundConfig,
    pub(super) mouse_scroll_lines: usize,
    pub(super) redraw_on_focus_gained: bool,
    pub(super) host_cursor: crate::config::HostCursorModeConfig,
    /// `ui.repeat_ime_cursor_anchor` 按宿主解析后的生效值，注入客户端 BlitEncoder。
    pub(super) repeat_ime_cursor_anchor: bool,
    pub(super) kitty_graphics_enabled: bool,
    pub(super) pixel_geometry_enabled: bool,
    pub(super) pixel_geometry_fallback: bool,
    pub(super) mouse_capture_active: bool,
    /// `[ui]` 的 stdin 成帧窗口（`input_sequence_timeout_ms` / `escape_after_mouse_timeout_ms`），
    /// 客户端启动时读取一次并交给 stdin 读线程。
    pub(super) stdin_flush_timeouts: input::StdinFlushTimeouts,
    pub(super) host_escape_disambiguation_active: bool,
    pub(super) initial_host_input: Vec<u8>,
    pub(super) endpoint_keybindings: bool,
    pub(super) remote_image_paste_key:
        Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)>,
    pub(super) shell_config: Option<shell::ClientShellConfig>,
}
