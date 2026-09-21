//! Scene snapshots: named captures of the client's current working scene —
//! the enabled machine set, the active machine with its workspace/tab (per
//! machine where a cached snapshot is available), and the sidebar chrome —
//! stored in a client-local `scene-snapshots.json` so a whole setup restores
//! with one action. Machine enable/disable goes through the endpoint catalog
//! exactly like the machines overlay: the catalog watcher reconciles live
//! connections afterwards, so no private socket behavior is added here.

use super::render::{
    modal_button, modal_button_row, modal_panel, put_right_text, put_text, render_key_hints,
    OverlayRender,
};
use super::*;
use crate::client::endpoint::{EndpointCatalog, ProfileId};
use crossterm::event::KeyModifiers;
use serde::{Deserialize, Serialize};

// ----- storage -----

const SCENE_SNAPSHOTS_VERSION: u32 = 1;
/// Cap on stored scenes; the oldest drop off the tail on save.
pub(super) const SCENE_SNAPSHOT_LIMIT: usize = 32;
const SCENE_NAME_MAX_BYTES: usize = 96;
const SCENE_NOTE_MAX_BYTES: usize = 300;
const MAX_SCENE_SNAPSHOTS_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ClientSceneSnapshotsFile {
    version: u32,
    #[serde(default)]
    scenes: Vec<ClientSceneSnapshot>,
}

/// One machine of a scene's enabled set. The label is kept so the restore
/// summary can name machines that no longer exist in the catalog.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ClientSceneMachine {
    pub(super) id: String,
    pub(super) label: String,
}

/// Focus of one machine at capture time (best effort across endpoints).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(super) struct ClientSceneMachineFocus {
    pub(super) machine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) tab_id: Option<String>,
}

/// Sidebar chrome captured with the scene.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(super) struct ClientSceneSidebar {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) width: Option<u16>,
    #[serde(default)]
    pub(super) collapsed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) collapsed_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) remote_collapsed_groups: Vec<preferences::ClientRemoteCollapsedGroups>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct ClientSceneSnapshot {
    pub(super) name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) note: Option<String>,
    pub(super) created_at: String,
    #[serde(default)]
    pub(super) machines: Vec<ClientSceneMachine>,
    /// Storage key of the active machine: `local` or `ssh:<profile-id>`.
    pub(super) active_machine: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) machine_focus: Vec<ClientSceneMachineFocus>,
    #[serde(default)]
    pub(super) sidebar: ClientSceneSidebar,
}

/// Client-local scene snapshot store: `client/scene-snapshots.json` under
/// the shared state directory, next to the endpoint catalog.
pub(super) fn scene_snapshots_path() -> std::path::PathBuf {
    crate::config::state_dir()
        .join("client")
        .join("scene-snapshots.json")
}

/// Strip control characters and truncate at a char boundary (catalog
/// precedent: stored display text never carries control bytes).
fn sanitize_scene_text(value: &str, max_bytes: usize) -> String {
    let mut output = String::new();
    for ch in value.chars().filter(|ch| !ch.is_control()) {
        if output.len() + ch.len_utf8() > max_bytes {
            break;
        }
        output.push(ch);
    }
    output
}

/// A scene is accepted from disk only with a clean name; every other free
/// text field is sanitized in place (mirrors the catalog validation split).
fn sanitize_scene(mut scene: ClientSceneSnapshot) -> Option<ClientSceneSnapshot> {
    scene.name = scene.name.trim().to_owned();
    if scene.name.is_empty()
        || scene.name.len() > SCENE_NAME_MAX_BYTES
        || scene.name.chars().any(char::is_control)
    {
        return None;
    }
    scene.note = scene
        .note
        .map(|note| sanitize_scene_text(&note, SCENE_NOTE_MAX_BYTES))
        .filter(|note| !note.is_empty());
    scene.created_at = sanitize_scene_text(&scene.created_at, 40);
    scene.machines.retain_mut(|machine| {
        machine.label = sanitize_scene_text(&machine.label, SCENE_NAME_MAX_BYTES);
        ProfileId::parse(machine.id.clone()).is_ok()
    });
    scene.machine_focus.retain_mut(|focus| {
        focus.workspace_id = focus
            .workspace_id
            .take()
            .map(|id| sanitize_scene_text(&id, 128))
            .filter(|id| !id.is_empty());
        focus.tab_id = focus
            .tab_id
            .take()
            .map(|id| sanitize_scene_text(&id, 128))
            .filter(|id| !id.is_empty());
        parse_scene_machine_key(&focus.machine).is_some()
            && (focus.workspace_id.is_some() || focus.tab_id.is_some())
    });
    Some(scene)
}

pub(super) fn load_scenes_from(path: &std::path::Path) -> Result<Vec<ClientSceneSnapshot>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("failed to read scene snapshots: {error}")),
    };
    if content.len() > MAX_SCENE_SNAPSHOTS_BYTES {
        return Err("scene snapshots exceed the storage limit".to_owned());
    }
    let file: ClientSceneSnapshotsFile = serde_json::from_str(&content)
        .map_err(|error| format!("failed to parse scene snapshots: {error}"))?;
    if file.version != SCENE_SNAPSHOTS_VERSION {
        return Err(format!(
            "unsupported scene snapshots version {}",
            file.version
        ));
    }
    Ok(file
        .scenes
        .into_iter()
        .filter_map(sanitize_scene)
        .take(SCENE_SNAPSHOT_LIMIT)
        .collect())
}

pub(super) fn store_scenes_to(
    path: &std::path::Path,
    scenes: &[ClientSceneSnapshot],
) -> Result<(), String> {
    let file = ClientSceneSnapshotsFile {
        version: SCENE_SNAPSHOTS_VERSION,
        scenes: scenes.iter().take(SCENE_SNAPSHOT_LIMIT).cloned().collect(),
    };
    let content = serde_json::to_vec_pretty(&file)
        .map_err(|error| format!("failed to encode scene snapshots: {error}"))?;
    if content.len() > MAX_SCENE_SNAPSHOTS_BYTES {
        return Err("scene snapshots exceed the storage limit".to_owned());
    }
    preferences::store_bytes(path, &content)
}

/// Display timestamp of a capture: local wall-clock when the platform can
/// report it, unix seconds otherwise.
fn scene_timestamp() -> String {
    if let Some(datetime) = crate::platform::local_datetime() {
        return format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            datetime.year(),
            u8::from(datetime.month()),
            datetime.day(),
            datetime.hour(),
            datetime.minute()
        );
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

fn parse_scene_machine_key(key: &str) -> Option<ClientEndpointId> {
    if key == "local" {
        return Some(ClientEndpointId::Local);
    }
    let hex = key.strip_prefix("ssh:")?;
    ProfileId::parse(hex).ok().map(ClientEndpointId::Ssh)
}

// ----- overlay state -----

#[derive(Debug)]
pub(super) struct ClientScenesOverlay {
    pub(super) view: ClientScenesView,
    pub(super) scenes: Vec<ClientSceneSnapshot>,
    pub(super) selected: usize,
    /// 指针悬浮行：只由 `Moved` 改写。`selected` 只由键盘与点击改写，否则指针
    /// 划过浮层就会把键盘选中拉到最后路过的那条现场上（MENU-01）。
    pub(super) hovered: Option<usize>,
    /// One-shot feedback line shown in the list view footer.
    pub(super) message: Option<String>,
    /// Load failure of the snapshot file (list stays empty but usable).
    pub(super) load_error: Option<String>,
    /// Restore option: disable every machine that is not part of the scene.
    /// Off by default: a restore must never silently drop live SSH endpoints
    /// (TOOL-02); turning it on additionally routes through a confirmation.
    pub(super) restore_disable_others: bool,
    /// 列表视图的点击痕迹（现场名 + 时刻）：单击只选中，同一条现场的二次
    /// 点击才执行恢复。记名称而非行号，列表重排后痕迹自然失效。
    pub(super) last_click: Option<(String, std::time::Instant)>,
}

impl ClientScenesOverlay {
    /// 切视图的唯一入口：顺带清掉指针悬浮行。`hovered` 是列表视图的行号，
    /// 换到 Save / Rename / ConfirmDelete / ConfirmRestore 再回来时旧行号
    /// 已经失效，不清就会留下一条指针并不在上面的弱底色（MENU-01）。
    fn set_view(&mut self, view: ClientScenesView) {
        self.view = view;
        self.hovered = None;
    }

    fn blank() -> Self {
        Self {
            view: ClientScenesView::List,
            scenes: Vec::new(),
            selected: 0,
            hovered: None,
            message: None,
            load_error: None,
            restore_disable_others: false,
            last_click: None,
        }
    }
}

#[derive(Debug)]
pub(super) enum ClientScenesView {
    List,
    Save(Box<ClientSceneForm>),
    Rename {
        index: usize,
        editor: TextEditor,
        error: Option<String>,
    },
    ConfirmDelete(usize),
    /// Restore would disable machines outside the scene: name them and wait
    /// for an explicit confirmation before writing the endpoint catalog.
    ConfirmRestore {
        index: usize,
        /// Machines the confirmation named, frozen at the moment the page
        /// opened. The confirmed restore may only disable these: the page
        /// blocks for an arbitrary time and snapshot events keep updating
        /// `saved_profiles`, so a freshly enabled machine must never be
        /// switched off without having been shown (TOOL-02).
        disable: Vec<ProfileId>,
        /// Display labels of `disable`, in the same order.
        labels: Vec<String>,
    },
}

impl ClientScenesView {
    /// 步进指纹，喂给 `ClientInputContext::overlay_step`：列表的 Enter 是
    /// 破坏性恢复，确认页的 Enter 是同一个键的下一步，两者必须不同值，
    /// 否则自动重复的 Repeat 会替用户按完确认。
    pub(super) fn step(&self) -> u32 {
        match self {
            Self::List => 0,
            Self::Save(_) => 1,
            Self::Rename { .. } => 2,
            Self::ConfirmDelete(_) => 3,
            Self::ConfirmRestore { .. } => 4,
        }
    }
}

#[derive(Debug)]
pub(super) struct ClientSceneForm {
    pub(super) name: TextEditor,
    pub(super) note: TextEditor,
    pub(super) focused: usize,
    pub(super) error: Option<String>,
}

/// Clickable buttons of the list view (form views reuse the generic
/// primary/cancel hit rects).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SceneOverlayButton {
    Save,
    Restore,
    Rename,
    Delete,
    Close,
    ToggleDisableOthers,
}

// ----- capture & restore planning -----

/// What a restore has to change, computed against the live catalog mirror
/// and endpoint set without touching the filesystem (unit-testable).
#[derive(Debug, Default)]
pub(super) struct SceneRestorePlan {
    pub(super) enable: Vec<ProfileId>,
    pub(super) disable: Vec<ProfileId>,
    /// Labels of scene machines absent from the current catalog.
    pub(super) missing: Vec<String>,
    pub(super) active: Option<SceneActivePlan>,
    /// Online background machines whose recorded workspace still exists.
    pub(super) background_focus: Vec<(ClientEndpointId, String)>,
}

#[derive(Debug, Clone)]
pub(super) struct SceneActivePlan {
    pub(super) endpoint_id: ClientEndpointId,
    pub(super) label: String,
    pub(super) target: Option<ClientEndpointFocusTarget>,
    pub(super) online: bool,
}

fn endpoint_online(endpoint: &ClientShellEndpoint) -> bool {
    endpoint.status == ClientEndpointStatus::Online && endpoint.snapshot.is_some()
}

fn plan_scene_restore(
    scene: &ClientSceneSnapshot,
    saved_profiles: &[SavedSshEndpoint],
    endpoints: &[ClientShellEndpoint],
    disable_others: bool,
) -> SceneRestorePlan {
    let mut plan = SceneRestorePlan::default();
    let mut scene_ids = HashSet::new();
    for machine in &scene.machines {
        let Ok(id) = ProfileId::parse(machine.id.clone()) else {
            plan.missing.push(machine.label.clone());
            continue;
        };
        if saved_profiles.iter().any(|profile| profile.id == id) {
            scene_ids.insert(id.clone());
            plan.enable.push(id);
        } else {
            plan.missing.push(machine.label.clone());
        }
    }
    if disable_others {
        plan.disable = saved_profiles
            .iter()
            .filter(|profile| profile.enabled && !scene_ids.contains(&profile.id))
            .map(|profile| profile.id.clone())
            .collect();
    }

    let focus_of = |machine_key: &str| {
        scene
            .machine_focus
            .iter()
            .find(|focus| focus.machine == machine_key)
    };
    let endpoint_by_id = |endpoint_id: &ClientEndpointId| {
        endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
    };
    let focus_target = |endpoint: &ClientShellEndpoint,
                        focus: Option<&ClientSceneMachineFocus>|
     -> Option<ClientEndpointFocusTarget> {
        let snapshot = endpoint.snapshot.as_deref()?;
        let focus = focus?;
        if let Some(tab_id) = focus.tab_id.as_deref() {
            if snapshot.tabs.iter().any(|tab| tab.tab_id == tab_id) {
                return Some(ClientEndpointFocusTarget::Tab(tab_id.to_owned()));
            }
        }
        if let Some(workspace_id) = focus.workspace_id.as_deref() {
            if snapshot
                .workspaces
                .iter()
                .any(|workspace| workspace.workspace_id == workspace_id)
            {
                return Some(ClientEndpointFocusTarget::Workspace(
                    workspace_id.to_owned(),
                ));
            }
        }
        None
    };

    match parse_scene_machine_key(&scene.active_machine) {
        Some(ClientEndpointId::Local) => {
            let endpoint = endpoint_by_id(&ClientEndpointId::Local);
            plan.active = Some(SceneActivePlan {
                endpoint_id: ClientEndpointId::Local,
                label: endpoint
                    .map(|endpoint| endpoint.label.clone())
                    .unwrap_or_else(|| "Local".to_owned()),
                target: endpoint
                    .and_then(|endpoint| focus_target(endpoint, focus_of(&scene.active_machine))),
                online: endpoint.is_some_and(endpoint_online),
            });
        }
        Some(ClientEndpointId::Ssh(profile_id)) => {
            let profile = saved_profiles
                .iter()
                .find(|profile| profile.id == profile_id);
            match profile {
                Some(profile) => {
                    let endpoint = endpoint_by_id(&ClientEndpointId::Ssh(profile_id.clone()));
                    plan.active = Some(SceneActivePlan {
                        endpoint_id: ClientEndpointId::Ssh(profile_id),
                        label: profile.label.clone(),
                        target: endpoint.and_then(|endpoint| {
                            focus_target(endpoint, focus_of(&scene.active_machine))
                        }),
                        online: endpoint.is_some_and(endpoint_online),
                    });
                }
                None => {
                    let label = scene
                        .machines
                        .iter()
                        .find(|machine| machine.id == profile_id.as_str())
                        .map(|machine| machine.label.clone())
                        .unwrap_or_else(|| profile_id.as_str().to_owned());
                    // The machines loop already reports the same profile.
                    if !plan.missing.iter().any(|known| known == &label) {
                        plan.missing.push(label);
                    }
                }
            }
        }
        None => {}
    }

    for focus in &scene.machine_focus {
        // The scene's active machine is handled by the activation path; every
        // other recorded machine is background after the restore, including
        // the one that is active right now.
        if focus.machine == scene.active_machine {
            continue;
        }
        let Some(endpoint_id) = parse_scene_machine_key(&focus.machine) else {
            continue;
        };
        let Some(workspace_id) = focus.workspace_id.as_deref() else {
            continue;
        };
        let Some(endpoint) = endpoint_by_id(&endpoint_id) else {
            continue;
        };
        if !endpoint_online(endpoint) {
            continue;
        }
        let exists = endpoint.snapshot.as_deref().is_some_and(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .any(|workspace| workspace.workspace_id == workspace_id)
        });
        if exists {
            plan.background_focus
                .push((endpoint_id, workspace_id.to_owned()));
        }
    }
    plan
}

// ----- state methods -----

impl ClientShellState {
    /// Loads the snapshot file into the overlay; a read/parse failure keeps
    /// the list empty and surfaces the error in the footer instead.
    fn reload_scenes(&mut self) {
        let scenes_path = scene_snapshots_path();
        let (scenes, load_error) = match load_scenes_from(&scenes_path) {
            Ok(scenes) => (scenes, None),
            Err(error) => (
                Vec::new(),
                Some(crate::i18n::fill(
                    crate::i18n::texts().scenes.load_failed_fmt,
                    &[("error", &error)],
                )),
            ),
        };
        if let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() {
            overlay.scenes = scenes;
            overlay.load_error = load_error;
            overlay.selected = overlay.selected.min(overlay.scenes.len().saturating_sub(1));
        }
    }

    pub(super) fn open_scenes_overlay(&mut self) {
        self.overlay = Some(ClientShellOverlay::Scenes(ClientScenesOverlay::blank()));
        self.reload_scenes();
    }

    pub(super) fn open_scene_save_form(&mut self) {
        let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let t = &crate::i18n::texts().scenes;
        let mut n = overlay.scenes.len().saturating_add(1);
        let mut suggestion = crate::i18n::fill(t.default_name_fmt, &[("n", &n.to_string())]);
        while overlay.scenes.iter().any(|scene| scene.name == suggestion) {
            n = n.saturating_add(1);
            suggestion = crate::i18n::fill(t.default_name_fmt, &[("n", &n.to_string())]);
        }
        overlay.set_view(ClientScenesView::Save(Box::new(ClientSceneForm {
            name: TextEditor::new(&suggestion, true),
            note: TextEditor::default(),
            focused: 0,
            error: None,
        })));
        overlay.message = None;
        overlay.last_click = None;
    }

    pub(super) fn open_scenes_overlay_saving(&mut self) {
        self.open_scenes_overlay();
        self.open_scene_save_form();
    }

    fn open_scene_rename_form(&mut self) {
        let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let Some(scene) = overlay.scenes.get(overlay.selected) else {
            return;
        };
        let index = overlay.selected;
        let editor = TextEditor::new(&scene.name.clone(), false);
        overlay.set_view(ClientScenesView::Rename {
            index,
            editor,
            error: None,
        });
        overlay.message = None;
        overlay.last_click = None;
    }

    fn open_scene_delete_confirm(&mut self) {
        let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() else {
            return;
        };
        if overlay.scenes.get(overlay.selected).is_none() {
            return;
        }
        let index = overlay.selected;
        overlay.set_view(ClientScenesView::ConfirmDelete(index));
        overlay.message = None;
        overlay.last_click = None;
    }

    pub(super) fn scenes_back(&mut self) {
        if let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() {
            overlay.set_view(ClientScenesView::List);
            overlay.message = None;
            // 离开子视图就清点击痕迹：否则「点一行 → r 改名 → Esc 回列表 →
            // 再点同一行」会在双击窗口内被判成二次点击并直接恢复。
            overlay.last_click = None;
        }
    }

    pub(super) fn move_scenes_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() else {
            return;
        };
        if overlay.scenes.is_empty() {
            overlay.selected = 0;
            return;
        }
        let last = overlay.scenes.len().saturating_sub(1) as isize;
        overlay.selected = (overlay.selected as isize + delta).clamp(0, last) as usize;
    }

    /// 指针悬浮行。`None` 表示指针不在任何行上——出界也要写，否则高亮会留在
    /// 鼠标早已离开的那一行（MENU-01）。只写 `hovered`，不动 `selected`。
    pub(super) fn set_scenes_hover(&mut self, hovered: Option<usize>) -> bool {
        let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        let hovered = hovered.filter(|index| {
            matches!(overlay.view, ClientScenesView::List) && *index < overlay.scenes.len()
        });
        let changed = overlay.hovered != hovered;
        overlay.hovered = hovered;
        changed
    }

    /// 键盘或点击直接落到某一行；返回 true 表示选中行变了。
    pub(super) fn set_scenes_selection(&mut self, index: usize) -> bool {
        let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        if overlay.scenes.is_empty() {
            return false;
        }
        let next = index.min(overlay.scenes.len().saturating_sub(1));
        let changed = overlay.selected != next;
        overlay.selected = next;
        changed
    }

    /// 记录列表点击痕迹并判定这次是否是同一条现场在双击窗口内的二次点击。
    /// 痕迹记现场名而非行号：筛选、删除、重命名导致的重排不会让上一次点击
    /// 落到别的现场上（片段库同构）。身份取 `set_scenes_selection` clamp 之后
    /// 的 `selected`，与选中行永远是同一条现场。
    pub(super) fn scenes_row_click_is_second(&mut self) -> bool {
        let window = self.config.double_click_window;
        let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        let Some(name) = overlay
            .scenes
            .get(overlay.selected)
            .map(|scene| scene.name.clone())
        else {
            overlay.last_click = None;
            return false;
        };
        let now = std::time::Instant::now();
        let second = overlay
            .last_click
            .as_ref()
            .is_some_and(|(last, at)| last == &name && now.duration_since(*at) <= window);
        // 恢复后清痕迹，第三次点击重新从「只选中」开始。
        overlay.last_click = if second { None } else { Some((name, now)) };
        second
    }

    pub(super) fn toggle_scene_disable_others(&mut self) {
        if let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() {
            overlay.restore_disable_others = !overlay.restore_disable_others;
        }
    }

    fn capture_scene(&self, name: String, note: Option<String>) -> ClientSceneSnapshot {
        let machines = self
            .saved_profiles
            .iter()
            .filter(|profile| profile.enabled)
            .map(|profile| ClientSceneMachine {
                id: profile.id.as_str().to_owned(),
                label: profile.label.clone(),
            })
            .collect();
        let machine_focus = self
            .endpoints
            .iter()
            .filter_map(|endpoint| {
                let snapshot = endpoint.snapshot.as_deref()?;
                let workspace_id = snapshot.focused_workspace_id.clone();
                let tab_id = snapshot.focused_tab_id.clone();
                (workspace_id.is_some() || tab_id.is_some()).then(|| ClientSceneMachineFocus {
                    machine: endpoint.endpoint_id.storage_key(),
                    workspace_id,
                    tab_id,
                })
            })
            .collect();
        let mut collapsed_groups = self.collapsed_groups.iter().cloned().collect::<Vec<_>>();
        collapsed_groups.sort();
        let mut remote_collapsed_groups = self
            .remote_collapsed_groups
            .iter()
            .filter_map(|(endpoint_id, groups)| {
                let ClientEndpointId::Ssh(profile_id) = endpoint_id else {
                    return None;
                };
                let mut collapsed_groups = groups.iter().cloned().collect::<Vec<_>>();
                collapsed_groups.sort();
                (!collapsed_groups.is_empty()).then(|| preferences::ClientRemoteCollapsedGroups {
                    profile_id: profile_id.to_string(),
                    collapsed_groups,
                })
            })
            .collect::<Vec<_>>();
        remote_collapsed_groups.sort_by(|left, right| left.profile_id.cmp(&right.profile_id));
        ClientSceneSnapshot {
            name,
            note,
            created_at: scene_timestamp(),
            machines,
            active_machine: self.active_endpoint_id.storage_key(),
            machine_focus,
            sidebar: ClientSceneSidebar {
                width: Some(self.sidebar_width),
                collapsed: self.sidebar_collapsed,
                collapsed_groups,
                remote_collapsed_groups,
            },
        }
    }

    /// Validates the editor text and returns the clean name/note, or the
    /// error string to show inside the form.
    fn validated_scene_fields(
        raw_name: &str,
        raw_note: &str,
    ) -> Result<(String, Option<String>), String> {
        let t = &crate::i18n::texts().scenes;
        let name = sanitize_scene_text(raw_name.trim(), SCENE_NAME_MAX_BYTES.saturating_add(1));
        if name.is_empty() {
            return Err(t.name_required.to_owned());
        }
        if name.len() > SCENE_NAME_MAX_BYTES {
            return Err(t.name_too_long.to_owned());
        }
        let note = sanitize_scene_text(raw_note.trim(), SCENE_NOTE_MAX_BYTES);
        Ok((name, (!note.is_empty()).then_some(note)))
    }

    fn persist_scenes(&mut self, scenes: Vec<ClientSceneSnapshot>) -> Result<(), String> {
        store_scenes_to(&scene_snapshots_path(), &scenes)?;
        if let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() {
            overlay.scenes = scenes;
            overlay.load_error = None;
            overlay.selected = overlay.selected.min(overlay.scenes.len().saturating_sub(1));
        }
        Ok(())
    }

    pub(super) fn submit_scene_save_form(&mut self, outcome: &mut ClientShellInput) {
        let (raw_name, raw_note) = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Scenes(ClientScenesOverlay {
                view: ClientScenesView::Save(form),
                ..
            })) => (form.name.as_str().to_owned(), form.note.as_str().to_owned()),
            _ => return,
        };
        let (name, note) = match Self::validated_scene_fields(&raw_name, &raw_note) {
            Ok(fields) => fields,
            Err(error) => {
                if let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() {
                    if let ClientScenesView::Save(form) = &mut overlay.view {
                        form.error = Some(error);
                    }
                }
                outcome.repaint = true;
                return;
            }
        };
        let mut scenes = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Scenes(overlay)) => overlay.scenes.clone(),
            _ => Vec::new(),
        };
        // 撞名不覆盖（TOOL-03）：保存与改名语义一致，旧快照（机器集合、焦点、
        // 侧栏布局）绝不静默消失。这条拒绝同时是「现场名唯一」的来源，
        // `ClientScenesOverlay::last_click` 用名称当点击身份正是依赖它。
        if scenes.iter().any(|existing| existing.name == name) {
            if let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() {
                if let ClientScenesView::Save(form) = &mut overlay.view {
                    form.error = Some(crate::i18n::texts().scenes.name_duplicate.to_owned());
                }
            }
            outcome.repaint = true;
            return;
        }
        let scene = self.capture_scene(name.clone(), note);
        let machine_count = scene.machines.len();
        scenes.insert(0, scene);
        scenes.truncate(SCENE_SNAPSHOT_LIMIT);
        let t = &crate::i18n::texts().scenes;
        if let Err(error) = self.persist_scenes(scenes) {
            if let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() {
                if let ClientScenesView::Save(form) = &mut overlay.view {
                    form.error = Some(error);
                }
            }
            outcome.repaint = true;
            return;
        }
        self.push_endpoint_notice(
            ClientEndpointNoticeKind::Success,
            "scene:save",
            crate::i18n::fill(t.notice_saved_fmt, &[("name", &name)]),
            crate::i18n::fill(
                t.machines_count_fmt,
                &[("count", &machine_count.to_string())],
            ),
        );
        self.scenes_back();
        outcome.repaint = true;
    }

    pub(super) fn submit_scene_rename(&mut self, outcome: &mut ClientShellInput) {
        let (index, raw_name) = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Scenes(ClientScenesOverlay {
                view: ClientScenesView::Rename { index, editor, .. },
                ..
            })) => (*index, editor.as_str().to_owned()),
            _ => return,
        };
        let (name, _) = match Self::validated_scene_fields(&raw_name, "") {
            Ok(fields) => fields,
            Err(error) => {
                if let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() {
                    if let ClientScenesView::Rename { error: slot, .. } = &mut overlay.view {
                        *slot = Some(error);
                    }
                }
                outcome.repaint = true;
                return;
            }
        };
        let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let mut scenes = overlay.scenes.clone();
        if scenes.get(index).is_none() {
            self.scenes_back();
            return;
        }
        let t = &crate::i18n::texts().scenes;
        // 撞名既不覆盖别人也不删除自己（TOOL-03）：留在表单里报错，两条现场
        // 都原样留在盘上，用户自己决定改哪个名字。
        let duplicate = scenes
            .iter()
            .enumerate()
            .any(|(other, scene)| other != index && scene.name == name);
        if duplicate {
            if let ClientScenesView::Rename { error: slot, .. } = &mut overlay.view {
                *slot = Some(t.name_duplicate.to_owned());
            }
            outcome.repaint = true;
            return;
        }
        scenes[index].name = name.clone();
        let new_name = name;
        if let Err(error) = self.persist_scenes(scenes) {
            if let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() {
                if let ClientScenesView::Rename { error: slot, .. } = &mut overlay.view {
                    *slot = Some(error);
                }
            }
            outcome.repaint = true;
            return;
        }
        self.scenes_back();
        if let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() {
            overlay.message = Some(crate::i18n::fill(
                t.notice_renamed_fmt,
                &[("name", &new_name)],
            ));
        }
        outcome.repaint = true;
    }

    pub(super) fn delete_confirmed_scene(&mut self, outcome: &mut ClientShellInput) {
        let index = match self.overlay.as_ref() {
            Some(ClientShellOverlay::Scenes(ClientScenesOverlay {
                view: ClientScenesView::ConfirmDelete(index),
                ..
            })) => *index,
            _ => return,
        };
        let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() else {
            return;
        };
        let mut scenes = overlay.scenes.clone();
        if scenes.get(index).is_none() {
            self.scenes_back();
            return;
        }
        let removed = scenes.remove(index);
        if let Err(error) = self.persist_scenes(scenes) {
            self.set_endpoint_error(error);
            outcome.repaint = true;
            return;
        }
        self.scenes_back();
        if let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() {
            overlay.message = Some(crate::i18n::fill(
                crate::i18n::texts().scenes.notice_deleted_fmt,
                &[("name", &removed.name)],
            ));
        }
        outcome.repaint = true;
    }

    /// One catalog batch write for the restore's enable/disable set (the
    /// machines overlay pattern: the watcher reconciles live connections).
    fn apply_scene_catalog_changes(
        &mut self,
        enable: &[ProfileId],
        disable: &[ProfileId],
    ) -> Result<(), String> {
        if enable.is_empty() && disable.is_empty() {
            return Ok(());
        }
        let mut catalog = EndpointCatalog::load()?;
        let changed = enable
            .iter()
            .any(|id| catalog.ssh.iter().any(|p| &p.id == id && !p.enabled))
            || disable
                .iter()
                .any(|id| catalog.ssh.iter().any(|p| &p.id == id && p.enabled));
        if !changed {
            return Ok(());
        }
        let previous_selection = catalog.selected_profile.clone();
        for id in enable {
            catalog.set_enabled(id, true);
        }
        for id in disable {
            catalog.set_enabled(id, false);
        }
        catalog.store_profiles()?;
        if catalog.selected_profile != previous_selection {
            catalog.store_selection()?;
        }
        self.mirror_saved_profiles(catalog.ssh.clone());
        Ok(())
    }

    fn apply_scene_sidebar(
        &mut self,
        sidebar: &ClientSceneSidebar,
        outcome: &mut ClientShellInput,
    ) {
        let mut resized = false;
        if let Some(width) = sidebar.width {
            let (min_width, max_width) = crate::config::validated_sidebar_bounds(
                self.config.sidebar_min_width,
                self.config.sidebar_max_width,
            )
            .unwrap_or((18, 36));
            let width = width.clamp(min_width, max_width);
            if width != self.sidebar_width {
                self.sidebar_width = width;
                resized = true;
            }
            self.sidebar_width_manual = true;
        }
        if sidebar.collapsed != self.sidebar_collapsed {
            self.sidebar_collapsed = sidebar.collapsed;
            resized = true;
        }
        self.sidebar_collapsed_manual = true;
        self.collapsed_groups = sidebar.collapsed_groups.iter().cloned().collect();
        self.remote_collapsed_groups = sidebar
            .remote_collapsed_groups
            .iter()
            .filter_map(|saved| {
                let profile_id = ProfileId::parse(saved.profile_id.clone()).ok()?;
                Some((
                    ClientEndpointId::Ssh(profile_id),
                    saved
                        .collapsed_groups
                        .iter()
                        .cloned()
                        .collect::<HashSet<_>>(),
                ))
            })
            .collect();
        if resized {
            self.invalidate_pane_surface();
            outcome.resize = true;
        }
        self.persist_chrome_preferences(outcome);
    }

    pub(super) fn restore_selected_scene(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_ref() else {
            return;
        };
        let Some(scene) = overlay.scenes.get(overlay.selected).cloned() else {
            return;
        };
        let index = overlay.selected;
        let disable_others = overlay.restore_disable_others;
        // 会断开现场之外的机器时先进确认页：恢复写的是端点目录，watcher 随后
        // 真的会断掉 live SSH 连接（TOOL-02）。没有任何机器要停用时不插确认页
        // ——空名单的确认只是噪音。
        if disable_others {
            let targets = self.scene_restore_disable_targets(&scene);
            if !targets.is_empty() {
                let (disable, labels) = targets.into_iter().unzip();
                if let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() {
                    overlay.set_view(ClientScenesView::ConfirmRestore {
                        index,
                        disable,
                        labels,
                    });
                    overlay.message = None;
                    overlay.last_click = None;
                }
                outcome.repaint = true;
                return;
            }
        }
        self.overlay = None;
        self.apply_scene_restore(&scene, disable_others, None, outcome);
    }

    /// Machines a `disable_others` restore of this scene would switch off,
    /// with the label to show for each (empty when it disconnects nothing).
    fn scene_restore_disable_targets(
        &self,
        scene: &ClientSceneSnapshot,
    ) -> Vec<(ProfileId, String)> {
        let plan = plan_scene_restore(scene, &self.saved_profiles, &self.endpoints, true);
        plan.disable
            .into_iter()
            .map(|id| {
                let label = self
                    .saved_profiles
                    .iter()
                    .find(|profile| profile.id == id)
                    .map(|profile| profile.label.clone())
                    .unwrap_or_else(|| id.as_str().to_owned());
                (id, label)
            })
            .collect()
    }

    /// Confirmed restore from `ClientScenesView::ConfirmRestore`.
    pub(super) fn confirm_scene_restore(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_ref() else {
            return;
        };
        let ClientScenesView::ConfirmRestore { index, disable, .. } = &overlay.view else {
            return;
        };
        let index = *index;
        // 确认页摊开过的那一批 id 就是允许停用的全部：确认那一刻重算的计划里
        // 若冒出新机器，它没被展示过，不许被断开。
        let confirmed = disable.clone();
        let disable_others = overlay.restore_disable_others;
        let Some(scene) = overlay.scenes.get(index).cloned() else {
            self.scenes_back();
            outcome.repaint = true;
            return;
        };
        self.overlay = None;
        self.apply_scene_restore(&scene, disable_others, Some(&confirmed), outcome);
    }

    fn apply_scene_restore(
        &mut self,
        scene: &ClientSceneSnapshot,
        disable_others: bool,
        confirmed_disable: Option<&[ProfileId]>,
        outcome: &mut ClientShellInput,
    ) {
        let mut plan =
            plan_scene_restore(scene, &self.saved_profiles, &self.endpoints, disable_others);
        if let Some(confirmed) = confirmed_disable {
            plan.disable.retain(|id| confirmed.contains(id));
        }
        if let Err(error) = self.apply_scene_catalog_changes(&plan.enable, &plan.disable) {
            self.set_endpoint_error(error);
            outcome.repaint = true;
            return;
        }
        self.apply_scene_sidebar(&scene.sidebar, outcome);
        let t = &crate::i18n::texts().scenes;
        let mut degraded = Vec::new();
        if !plan.missing.is_empty() {
            degraded.push(crate::i18n::fill(
                t.restore_missing_fmt,
                &[("names", &plan.missing.join(", "))],
            ));
        }
        if let Some(active) = plan.active {
            if active.online {
                match active.target {
                    Some(target) => {
                        self.focus_or_activate(active.endpoint_id, target, outcome);
                    }
                    None => {
                        self.activate_endpoint(active.endpoint_id, outcome);
                    }
                }
            } else {
                degraded.push(crate::i18n::fill(
                    t.restore_offline_fmt,
                    &[("label", &active.label)],
                ));
            }
        }
        for (endpoint_id, workspace_id) in plan.background_focus {
            self.push_endpoint_method_for(
                &endpoint_id,
                crate::api::schema::Method::WorkspaceFocus(crate::api::schema::WorkspaceTarget {
                    workspace_id,
                }),
                PendingEndpointKind::Generic,
                outcome,
            );
        }
        let body = if degraded.is_empty() {
            t.restore_ok_body.to_owned()
        } else {
            degraded.join("; ")
        };
        self.push_endpoint_notice(
            ClientEndpointNoticeKind::Success,
            "scene:restore",
            crate::i18n::fill(t.notice_restored_fmt, &[("name", &scene.name)]),
            body,
        );
        outcome.repaint = true;
    }

    /// Mouse activation for one rendered scenes-overlay button.
    pub(super) fn activate_scene_button(
        &mut self,
        button: SceneOverlayButton,
        outcome: &mut ClientShellInput,
    ) {
        match button {
            SceneOverlayButton::Save => self.open_scene_save_form(),
            SceneOverlayButton::Restore => self.restore_selected_scene(outcome),
            SceneOverlayButton::Rename => self.open_scene_rename_form(),
            SceneOverlayButton::Delete => self.open_scene_delete_confirm(),
            SceneOverlayButton::Close => self.overlay = None,
            SceneOverlayButton::ToggleDisableOthers => self.toggle_scene_disable_others(),
        }
        outcome.repaint = true;
    }

    /// Generic primary button of the form views (save / rename / delete).
    pub(super) fn activate_scene_primary(&mut self, outcome: &mut ClientShellInput) {
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::Scenes(ClientScenesOverlay {
                view: ClientScenesView::Save(_),
                ..
            })) => self.submit_scene_save_form(outcome),
            Some(ClientShellOverlay::Scenes(ClientScenesOverlay {
                view: ClientScenesView::Rename { .. },
                ..
            })) => self.submit_scene_rename(outcome),
            Some(ClientShellOverlay::Scenes(ClientScenesOverlay {
                view: ClientScenesView::ConfirmDelete(_),
                ..
            })) => self.delete_confirmed_scene(outcome),
            Some(ClientShellOverlay::Scenes(ClientScenesOverlay {
                view: ClientScenesView::ConfirmRestore { .. },
                ..
            })) => self.confirm_scene_restore(outcome),
            _ => {}
        }
    }

    /// Focus one text field of the save form by its rendered index.
    pub(super) fn focus_scene_form_field(&mut self, field: usize) {
        if let Some(ClientShellOverlay::Scenes(ClientScenesOverlay {
            view: ClientScenesView::Save(form),
            ..
        })) = self.overlay.as_mut()
        {
            form.focused = field.min(1);
        }
    }

    pub(super) fn insert_scenes_overlay_text(&mut self, text: &str) -> bool {
        let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() else {
            return false;
        };
        match &mut overlay.view {
            ClientScenesView::Save(form) => {
                let editor = match form.focused {
                    0 => &mut form.name,
                    _ => &mut form.note,
                };
                editor.insert(text)
            }
            ClientScenesView::Rename { editor, .. } => editor.insert(text),
            _ => false,
        }
    }

    pub(super) fn route_scenes_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::Scenes(_))) {
            return false;
        }
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let plain = modifiers.is_empty();
        let save_form = matches!(
            self.overlay,
            Some(ClientShellOverlay::Scenes(ClientScenesOverlay {
                view: ClientScenesView::Save(_),
                ..
            }))
        );
        let rename_form = matches!(
            self.overlay,
            Some(ClientShellOverlay::Scenes(ClientScenesOverlay {
                view: ClientScenesView::Rename { .. },
                ..
            }))
        );
        let confirm_delete = matches!(
            self.overlay,
            Some(ClientShellOverlay::Scenes(ClientScenesOverlay {
                view: ClientScenesView::ConfirmDelete(_),
                ..
            }))
        );
        let confirm_restore = matches!(
            self.overlay,
            Some(ClientShellOverlay::Scenes(ClientScenesOverlay {
                view: ClientScenesView::ConfirmRestore { .. },
                ..
            }))
        );

        if confirm_delete {
            match code {
                KeyCode::Enter => self.delete_confirmed_scene(outcome),
                KeyCode::Esc => {
                    self.scenes_back();
                    outcome.repaint = true;
                }
                _ => {}
            }
            return true;
        }

        if confirm_restore {
            match code {
                // 与 worktree 强制删除同构：确认键换成 y / ctrl+↵，列表里的
                // 回车到这一步就成了空操作。换键与「重复是否可辨识」解耦——
                // 宿主不支持 kitty 事件类型时自动重复只发普通 Press，
                // `overlay_step` 的步进指纹拦不住，但换键后长按或连点回车都
                // 到不了「真的恢复」。
                KeyCode::Enter if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.confirm_scene_restore(outcome)
                }
                KeyCode::Char('y' | 'Y') => self.confirm_scene_restore(outcome),
                KeyCode::Esc => {
                    self.scenes_back();
                    outcome.repaint = true;
                }
                _ => {}
            }
            return true;
        }

        if save_form || rename_form {
            if code == KeyCode::Enter {
                self.activate_scene_primary(outcome);
                return true;
            }
            if code == KeyCode::Esc {
                self.scenes_back();
                outcome.repaint = true;
                return true;
            }
            let Some(ClientShellOverlay::Scenes(overlay)) = self.overlay.as_mut() else {
                return true;
            };
            match &mut overlay.view {
                ClientScenesView::Save(form) => match code {
                    KeyCode::Tab if plain => {
                        form.focused = (form.focused + 1) % 2;
                        outcome.repaint = true;
                    }
                    KeyCode::BackTab if modifiers.difference(KeyModifiers::SHIFT).is_empty() => {
                        form.focused = (form.focused + 1) % 2;
                        outcome.repaint = true;
                    }
                    _ => {
                        let editor = match form.focused {
                            0 => &mut form.name,
                            _ => &mut form.note,
                        };
                        outcome.repaint |= editor.handle_key(key).is_some();
                    }
                },
                ClientScenesView::Rename { editor, .. } => {
                    outcome.repaint |= editor.handle_key(key).is_some();
                }
                _ => {}
            }
            return true;
        }

        // List view.
        match code {
            KeyCode::Esc => {
                self.overlay = None;
                outcome.repaint = true;
            }
            KeyCode::Enter => self.restore_selected_scene(outcome),
            KeyCode::Up | KeyCode::Char('k') if plain => {
                self.move_scenes_selection(-1);
                outcome.repaint = true;
            }
            KeyCode::Down | KeyCode::Char('j') if plain => {
                self.move_scenes_selection(1);
                outcome.repaint = true;
            }
            KeyCode::Char('u') if modifiers == KeyModifiers::CONTROL => {
                self.move_scenes_selection(-8);
                outcome.repaint = true;
            }
            KeyCode::Char('d') if modifiers == KeyModifiers::CONTROL => {
                self.move_scenes_selection(8);
                outcome.repaint = true;
            }
            KeyCode::Char('s') if plain => {
                self.open_scene_save_form();
                outcome.repaint = true;
            }
            KeyCode::Char('r') if plain => {
                self.open_scene_rename_form();
                outcome.repaint = true;
            }
            KeyCode::Char('d') if plain => {
                self.open_scene_delete_confirm();
                outcome.repaint = true;
            }
            KeyCode::Char('x') if plain => {
                self.toggle_scene_disable_others();
                outcome.repaint = true;
            }
            _ => {}
        }
        true
    }
}

// ----- rendering -----

pub(super) fn render_scenes_overlay(
    b: &mut Buffer,
    overlay: &ClientScenesOverlay,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    match &overlay.view {
        ClientScenesView::List => render_scene_list(b, overlay, cx),
        ClientScenesView::Save(form) => render_scene_save_form(b, form, cx),
        ClientScenesView::Rename {
            index,
            editor,
            error,
        } => render_scene_rename_form(b, overlay, *index, editor, error.as_deref(), cx),
        ClientScenesView::ConfirmDelete(index) => {
            render_scene_delete_confirm(b, overlay, *index, cx)
        }
        ClientScenesView::ConfirmRestore { index, labels, .. } => {
            render_scene_restore_confirm(b, overlay, *index, labels, cx)
        }
    }
}

/// Restore confirmation: names the scene and every machine the restore will
/// disable, so the disconnect is never a surprise (TOOL-02). The modal grows
/// with the list and, when it still does not fit, says how many machines it
/// could not show instead of silently dropping them.
fn render_scene_restore_confirm(
    b: &mut Buffer,
    overlay: &ClientScenesOverlay,
    index: usize,
    labels: &[String],
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    /// 名单最多撑到这么高的弹窗（含边框），再多走「还有 N 台」。
    const MAX_CONFIRM_HEIGHT: u16 = 24;
    const MIN_CONFIRM_HEIGHT: u16 = 12;

    let p = cx.palette;
    let t = &crate::i18n::texts().scenes;
    // 内容高度 = 引导行 + 每台机器一行；弹窗再加 header/footer/actions 与边框。
    let content_rows = u16::try_from(labels.len())
        .unwrap_or(u16::MAX)
        .saturating_add(1);
    let height = content_rows
        .saturating_add(8)
        .clamp(MIN_CONFIRM_HEIGHT, MAX_CONFIRM_HEIGHT);
    let (popup, inner) = modal_panel(
        b,
        crate::ui::ModalSize::Medium.with_height(height),
        p.accent,
        cx,
    )?;
    if inner.width < 20 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            scenes_popup: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 1, 1, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    let name = overlay
        .scenes
        .get(index)
        .map(|scene| scene.name.as_str())
        .unwrap_or_default();
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(
            " {}",
            crate::i18n::fill(t.restore_confirm_title_fmt, &[("name", name)])
        ),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );

    let width = usize::from(stack.content.width);
    let available = usize::from(stack.content.height);
    let mut lines: Vec<(String, Style)> = Vec::new();
    if available > 0 {
        lines.push((
            crate::ui::truncate_end(t.restore_confirm_detail, width),
            base.fg(p.text),
        ));
    }
    // 名单放不下时留最后一行给「还有 N 台」：机器名逐条成行并按显示宽度截断，
    // 不从中间把名字切成两半。
    let capacity = available.saturating_sub(lines.len());
    let shown = if labels.len() > capacity {
        capacity.saturating_sub(1)
    } else {
        labels.len()
    };
    for label in labels.iter().take(shown) {
        lines.push((
            format!(
                " · {}",
                crate::ui::truncate_end(label, width.saturating_sub(3))
            ),
            base.fg(p.yellow),
        ));
    }
    let hidden = labels.len().saturating_sub(shown);
    if hidden > 0 && lines.len() < available {
        lines.push((
            crate::ui::truncate_end(
                &crate::i18n::fill(
                    t.restore_confirm_more_fmt,
                    &[("count", &hidden.to_string())],
                ),
                width,
            ),
            base.fg(p.yellow),
        ));
    }
    for (offset, (text, style)) in lines.iter().take(available).enumerate() {
        put_text(
            b,
            stack.content.x,
            stack.content.y.saturating_add(offset as u16),
            stack.content.width,
            text,
            *style,
        );
    }

    // 确认键换了，必须写在浮层里，否则回车「没反应」像是卡住。
    if let Some(footer) = stack.footer {
        put_text(
            b,
            footer.x,
            footer.y,
            footer.width,
            t.restore_confirm_hint,
            base.fg(p.subtext0),
        );
    }

    let restore_label = t.confirm_restore_button;
    let cancel_label = t.cancel_button;
    let buttons = modal_button_row(
        stack.actions.unwrap_or_default(),
        &[restore_label, cancel_label],
        2,
    );
    let mut primary = Rect::default();
    let mut cancel = Rect::default();
    if let [restore_rect, cancel_rect] = buttons.as_slice() {
        primary = *restore_rect;
        cancel = *cancel_rect;
        modal_button(
            b,
            *restore_rect,
            restore_label,
            crate::ui::ModalButtonTone::Danger,
            cx.button_state(
                &super::feedback::ChromeHover::OverlayPrimary,
                // 断连确认不把默认强调留给「继续」：这一步要用户明确瞄准。
                crate::ui::ModalButtonState::Normal,
            ),
            p,
        );
        modal_button(
            b,
            *cancel_rect,
            cancel_label,
            crate::ui::ModalButtonTone::Secondary,
            cx.button_state(
                &super::feedback::ChromeHover::OverlayCancel,
                crate::ui::ModalButtonState::Focused,
            ),
            p,
        );
    }
    Some(OverlayRender {
        area: popup,
        scenes_popup: popup,
        primary,
        cancel,
        ..OverlayRender::default()
    })
}

fn render_scene_list(
    b: &mut Buffer,
    overlay: &ClientScenesOverlay,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().scenes;
    let (popup, inner) = modal_panel(b, crate::ui::ModalSize::Large.with_height(22), p.accent, cx)?;
    if inner.width < 24 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            scenes_popup: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 1, 2, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(" {}", t.title),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    let count = crate::i18n::fill(t.count_fmt, &[("count", &overlay.scenes.len().to_string())]);
    put_right_text(b, stack.header, stack.header.y, &count, base.fg(p.overlay0));

    let body = stack.content;
    let row_height = 2usize;
    let visible = (usize::from(body.height) / row_height).max(1);
    let selected = if overlay.scenes.is_empty() {
        0
    } else {
        overlay.selected.min(overlay.scenes.len() - 1)
    };
    let max_scroll = overlay.scenes.len().saturating_sub(visible);
    let scroll = selected
        .saturating_add(1)
        .saturating_sub(visible)
        .min(selected)
        .min(max_scroll);
    let mut row_hits = Vec::new();
    for (index, scene) in overlay.scenes.iter().enumerate().skip(scroll).take(visible) {
        let y = body
            .y
            .saturating_add(((index - scroll) * row_height) as u16);
        let rect = Rect::new(body.x, y, body.width, row_height as u16);
        row_hits.push((rect, index));
        let is_selected = index == selected;
        let is_hovered = overlay.hovered == Some(index);
        let style = list_row_style(p, cx.components, is_selected, is_hovered);
        b.set_style(rect, style);
        put_text(
            b,
            rect.x,
            rect.y,
            rect.width,
            &format!(" {}", scene.name),
            style,
        );
        put_right_text(b, rect, rect.y, &scene.created_at, style);
        let meta_style = if is_selected {
            style
        } else {
            Style::default()
                .fg(p.overlay0)
                .bg(list_row_bg(p, cx.components, false, is_hovered))
        };
        let machines = crate::i18n::fill(
            t.machines_count_fmt,
            &[("count", &scene.machines.len().to_string())],
        );
        let meta = match scene.note.as_deref().filter(|note| !note.is_empty()) {
            Some(note) => format!("   {machines} · {note}"),
            None => format!("   {machines}"),
        };
        put_text(
            b,
            rect.x,
            rect.y.saturating_add(1),
            rect.width,
            &meta,
            meta_style,
        );
    }
    if overlay.scenes.is_empty() {
        put_text(b, body.x, body.y, body.width, t.empty, base.fg(p.overlay1));
        put_text(
            b,
            body.x,
            body.y.saturating_add(1),
            body.width,
            t.empty_hint,
            base.fg(p.overlay0),
        );
    }

    let mut action_hits = Vec::new();
    if let Some(footer) = stack.footer {
        let toggle_label = format!(
            "[{}] {}",
            if overlay.restore_disable_others {
                "x"
            } else {
                " "
            },
            t.toggle_disable_others
        );
        let toggle_rect = Rect::new(footer.x, footer.y, footer.width, 1);
        let toggle_hover = cx.hovered(&super::feedback::ChromeHover::SceneButton(
            SceneOverlayButton::ToggleDisableOthers,
        ));
        put_text(
            b,
            toggle_rect.x,
            toggle_rect.y,
            toggle_rect.width,
            &toggle_label,
            base.fg(if toggle_hover { p.text } else { p.overlay0 }),
        );
        action_hits.push((toggle_rect, SceneOverlayButton::ToggleDisableOthers));
        let hint_row = Rect::new(footer.x, footer.y.saturating_add(1), footer.width, 1);
        if let Some(error) = overlay.load_error.as_deref() {
            put_text(
                b,
                hint_row.x,
                hint_row.y,
                hint_row.width,
                error,
                base.fg(p.red),
            );
        } else if let Some(message) = overlay.message.as_deref() {
            put_text(
                b,
                hint_row.x,
                hint_row.y,
                hint_row.width,
                message,
                base.fg(p.green),
            );
        } else {
            render_key_hints(
                b,
                hint_row,
                &[
                    ("↑↓".to_owned(), t.hint_select.to_owned()),
                    ("enter".to_owned(), t.hint_restore.to_owned()),
                    ("s".to_owned(), t.hint_save.to_owned()),
                    ("r".to_owned(), t.hint_rename.to_owned()),
                    ("d".to_owned(), t.hint_delete.to_owned()),
                    ("x".to_owned(), t.hint_toggle.to_owned()),
                    ("esc".to_owned(), t.hint_close.to_owned()),
                ],
                p,
                cx.components,
            );
        }
    }

    let save_label = t.save_button;
    let restore_label = t.restore_button;
    let rename_label = t.rename_button;
    let delete_label = t.delete_button;
    let close_label = crate::ui::modal_close_button_text();
    let buttons = modal_button_row(
        stack.actions.unwrap_or_default(),
        &[
            save_label,
            restore_label,
            rename_label,
            delete_label,
            close_label,
        ],
        2,
    );
    if let [save, restore, rename, delete, close] = buttons.as_slice() {
        let empty = overlay.scenes.is_empty();
        for (rect, label, button, tone, enabled) in [
            (
                save,
                save_label,
                SceneOverlayButton::Save,
                crate::ui::ModalButtonTone::Primary,
                true,
            ),
            (
                restore,
                restore_label,
                SceneOverlayButton::Restore,
                crate::ui::ModalButtonTone::Secondary,
                !empty,
            ),
            (
                rename,
                rename_label,
                SceneOverlayButton::Rename,
                crate::ui::ModalButtonTone::Secondary,
                !empty,
            ),
            (
                delete,
                delete_label,
                SceneOverlayButton::Delete,
                crate::ui::ModalButtonTone::Secondary,
                !empty,
            ),
            (
                close,
                close_label,
                SceneOverlayButton::Close,
                crate::ui::ModalButtonTone::Secondary,
                true,
            ),
        ] {
            let base_state = if !enabled {
                crate::ui::ModalButtonState::Disabled
            } else if matches!(button, SceneOverlayButton::Save) {
                crate::ui::ModalButtonState::Focused
            } else {
                crate::ui::ModalButtonState::Normal
            };
            modal_button(
                b,
                *rect,
                label,
                tone,
                cx.button_state(
                    &super::feedback::ChromeHover::SceneButton(button),
                    base_state,
                ),
                p,
            );
            action_hits.push((*rect, button));
        }
    }

    Some(OverlayRender {
        area: popup,
        scenes_popup: popup,
        scenes_rows: row_hits,
        scenes_actions: action_hits,
        ..OverlayRender::default()
    })
}

fn render_scene_save_form(
    b: &mut Buffer,
    form: &ClientSceneForm,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().scenes;
    let (popup, inner) = modal_panel(
        b,
        crate::ui::ModalSize::Medium.with_height(13),
        p.accent,
        cx,
    )?;
    if inner.width < 20 || inner.height < 8 {
        return Some(OverlayRender {
            area: popup,
            scenes_popup: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 1, 1, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(" {}", t.save_title),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    let body = stack.content;
    let mut cursor = None;
    let mut field_hits = Vec::new();
    for (index, (label, editor)) in [(t.field_name, &form.name), (t.field_note, &form.note)]
        .into_iter()
        .enumerate()
    {
        let label_y = body.y.saturating_add((index * 3) as u16);
        if label_y.saturating_add(1) >= body.bottom() {
            break;
        }
        put_text(b, body.x, label_y, body.width, label, base.fg(p.overlay0));
        let input = Rect::new(body.x, label_y.saturating_add(1), body.width, 1);
        let is_focused = form.focused == index;
        // 两个字段都画成输入框（与机器导入表单同口径）：焦点由光标指示，字段
        // 本身不再用「有底 / 无底」区分，否则 terminal 主题下未聚焦字段没有边界。
        let style = crate::ui::input_field_style(p);
        b.set_style(input, style);
        let inner_input = Rect::new(
            input.x.saturating_add(1),
            input.y,
            input.width.saturating_sub(1),
            1,
        );
        let field_cursor = text_editor::render(b, inner_input, editor, style);
        if is_focused {
            cursor = field_cursor;
        }
        field_hits.push((input, index));
    }
    if let Some(footer) = stack.footer {
        if let Some(error) = form.error.as_deref() {
            put_text(b, footer.x, footer.y, footer.width, error, base.fg(p.red));
        } else {
            render_key_hints(
                b,
                footer,
                &[
                    ("tab".to_owned(), t.hint_fields.to_owned()),
                    ("enter".to_owned(), t.hint_confirm.to_owned()),
                    ("esc".to_owned(), t.hint_back.to_owned()),
                ],
                p,
                cx.components,
            );
        }
    }
    let save_label = t.confirm_save_button;
    let cancel_label = t.cancel_button;
    let buttons = modal_button_row(
        stack.actions.unwrap_or_default(),
        &[save_label, cancel_label],
        2,
    );
    let mut primary = Rect::default();
    let mut cancel = Rect::default();
    if let [save_rect, cancel_rect] = buttons.as_slice() {
        primary = *save_rect;
        cancel = *cancel_rect;
        modal_button(
            b,
            *save_rect,
            save_label,
            crate::ui::ModalButtonTone::Primary,
            cx.button_state(
                &super::feedback::ChromeHover::OverlayPrimary,
                crate::ui::ModalButtonState::Focused,
            ),
            p,
        );
        modal_button(
            b,
            *cancel_rect,
            cancel_label,
            crate::ui::ModalButtonTone::Secondary,
            cx.button_state(
                &super::feedback::ChromeHover::OverlayCancel,
                crate::ui::ModalButtonState::Normal,
            ),
            p,
        );
    }
    Some(OverlayRender {
        area: popup,
        scenes_popup: popup,
        scenes_fields: field_hits,
        primary,
        cancel,
        cursor,
        ..OverlayRender::default()
    })
}

fn render_scene_rename_form(
    b: &mut Buffer,
    overlay: &ClientScenesOverlay,
    index: usize,
    editor: &TextEditor,
    error: Option<&str>,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().scenes;
    let (popup, inner) = modal_panel(
        b,
        crate::ui::ModalSize::Medium.with_height(10),
        p.accent,
        cx,
    )?;
    if inner.width < 20 || inner.height < 6 {
        return Some(OverlayRender {
            area: popup,
            scenes_popup: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 1, 1, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    let title = match overlay.scenes.get(index) {
        Some(scene) => format!(" {} — {}", t.rename_title, scene.name),
        None => format!(" {}", t.rename_title),
    };
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &title,
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    let input = Rect::new(stack.content.x, stack.content.y, stack.content.width, 1);
    let style = crate::ui::input_field_style(p);
    b.set_style(input, style);
    let inner_input = Rect::new(
        input.x.saturating_add(1),
        input.y,
        input.width.saturating_sub(1),
        1,
    );
    let cursor = text_editor::render(b, inner_input, editor, style);
    if let Some(footer) = stack.footer {
        if let Some(error) = error {
            put_text(b, footer.x, footer.y, footer.width, error, base.fg(p.red));
        } else {
            render_key_hints(
                b,
                footer,
                &[
                    ("enter".to_owned(), t.hint_confirm.to_owned()),
                    ("esc".to_owned(), t.hint_back.to_owned()),
                ],
                p,
                cx.components,
            );
        }
    }
    let save_label = t.confirm_save_button;
    let cancel_label = t.cancel_button;
    let buttons = modal_button_row(
        stack.actions.unwrap_or_default(),
        &[save_label, cancel_label],
        2,
    );
    let mut primary = Rect::default();
    let mut cancel = Rect::default();
    if let [save_rect, cancel_rect] = buttons.as_slice() {
        primary = *save_rect;
        cancel = *cancel_rect;
        modal_button(
            b,
            *save_rect,
            save_label,
            crate::ui::ModalButtonTone::Primary,
            cx.button_state(
                &super::feedback::ChromeHover::OverlayPrimary,
                crate::ui::ModalButtonState::Focused,
            ),
            p,
        );
        modal_button(
            b,
            *cancel_rect,
            cancel_label,
            crate::ui::ModalButtonTone::Secondary,
            cx.button_state(
                &super::feedback::ChromeHover::OverlayCancel,
                crate::ui::ModalButtonState::Normal,
            ),
            p,
        );
    }
    Some(OverlayRender {
        area: popup,
        scenes_popup: popup,
        primary,
        cancel,
        cursor,
        ..OverlayRender::default()
    })
}

fn render_scene_delete_confirm(
    b: &mut Buffer,
    overlay: &ClientScenesOverlay,
    index: usize,
    cx: &super::feedback::ChromeContext<'_>,
) -> Option<OverlayRender> {
    let p = cx.palette;
    let t = &crate::i18n::texts().scenes;
    let (popup, inner) = modal_panel(
        b,
        crate::ui::ModalSize::Medium.with_height(10),
        p.accent,
        cx,
    )?;
    if inner.width < 20 || inner.height < 6 {
        return Some(OverlayRender {
            area: popup,
            scenes_popup: popup,
            ..OverlayRender::default()
        });
    }
    let stack = crate::ui::modal_stack_areas(inner, 1, 0, 1, 1);
    let base = Style::default()
        .bg(p.panel_bg)
        .remove_modifier(Modifier::DIM);
    let name = overlay
        .scenes
        .get(index)
        .map(|scene| scene.name.as_str())
        .unwrap_or_default();
    put_text(
        b,
        stack.header.x,
        stack.header.y,
        stack.header.width,
        &format!(
            " {}",
            crate::i18n::fill(t.delete_title_fmt, &[("name", name)])
        ),
        base.fg(p.text).add_modifier(Modifier::BOLD),
    );
    put_text(
        b,
        stack.content.x,
        stack.content.y,
        stack.content.width,
        t.delete_detail,
        base.fg(p.overlay0),
    );
    let delete_label = t.confirm_delete_button;
    let cancel_label = t.cancel_button;
    let buttons = modal_button_row(
        stack.actions.unwrap_or_default(),
        &[delete_label, cancel_label],
        2,
    );
    let mut primary = Rect::default();
    let mut cancel = Rect::default();
    if let [delete_rect, cancel_rect] = buttons.as_slice() {
        primary = *delete_rect;
        cancel = *cancel_rect;
        modal_button(
            b,
            *delete_rect,
            delete_label,
            crate::ui::ModalButtonTone::Danger,
            cx.button_state(
                &super::feedback::ChromeHover::OverlayPrimary,
                crate::ui::ModalButtonState::Focused,
            ),
            p,
        );
        modal_button(
            b,
            *cancel_rect,
            cancel_label,
            crate::ui::ModalButtonTone::Secondary,
            cx.button_state(
                &super::feedback::ChromeHover::OverlayCancel,
                crate::ui::ModalButtonState::Normal,
            ),
            p,
        );
    }
    Some(OverlayRender {
        area: popup,
        scenes_popup: popup,
        primary,
        cancel,
        ..OverlayRender::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "herdr-scene-snapshots-{tag}-{}.json",
            std::process::id()
        ))
    }

    fn scene(name: &str) -> ClientSceneSnapshot {
        ClientSceneSnapshot {
            name: name.to_owned(),
            note: Some("note".to_owned()),
            created_at: "2026-09-17 10:00".to_owned(),
            machines: vec![ClientSceneMachine {
                id: "a".repeat(32),
                label: "Build".to_owned(),
            }],
            active_machine: "local".to_owned(),
            machine_focus: vec![ClientSceneMachineFocus {
                machine: "local".to_owned(),
                workspace_id: Some("ws_1".to_owned()),
                tab_id: None,
            }],
            sidebar: ClientSceneSidebar {
                width: Some(24),
                collapsed: false,
                collapsed_groups: vec!["/repo".to_owned()],
                remote_collapsed_groups: Vec::new(),
            },
        }
    }

    #[test]
    fn store_and_load_roundtrip() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        store_scenes_to(&path, &[scene("morning")]).expect("store scenes");
        let loaded = load_scenes_from(&path).expect("load scenes");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "morning");
        assert_eq!(loaded[0].machines.len(), 1);
        assert_eq!(loaded[0].sidebar.width, Some(24));
        std::fs::remove_file(path).expect("remove scenes");
    }

    #[test]
    fn load_missing_file_is_empty() {
        let path = temp_path("missing");
        let _ = std::fs::remove_file(&path);
        assert!(load_scenes_from(&path).expect("load").is_empty());
    }

    #[test]
    fn load_rejects_unknown_version() {
        let path = temp_path("version");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, r#"{"version":99,"scenes":[]}"#).expect("write scenes");
        assert!(load_scenes_from(&path).is_err());
        std::fs::remove_file(path).expect("remove scenes");
    }

    #[test]
    fn load_drops_control_char_names_and_bad_ids() {
        let path = temp_path("sanitize");
        let _ = std::fs::remove_file(&path);
        let mut dirty = scene("clean");
        dirty.machines.push(ClientSceneMachine {
            id: "not-a-profile-id".to_owned(),
            label: "ghost".to_owned(),
        });
        let control = ClientSceneSnapshot {
            name: "bad\u{0007}name".to_owned(),
            ..scene("unused")
        };
        store_scenes_to(&path, &[dirty, control]).expect("store scenes");
        let loaded = load_scenes_from(&path).expect("load scenes");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "clean");
        assert_eq!(loaded[0].machines.len(), 1);
        std::fs::remove_file(path).expect("remove scenes");
    }

    #[test]
    fn store_caps_at_limit() {
        let path = temp_path("limit");
        let _ = std::fs::remove_file(&path);
        let scenes = (0..SCENE_SNAPSHOT_LIMIT + 5)
            .map(|index| scene(&format!("scene {index}")))
            .collect::<Vec<_>>();
        store_scenes_to(&path, &scenes).expect("store scenes");
        let loaded = load_scenes_from(&path).expect("load scenes");
        assert_eq!(loaded.len(), SCENE_SNAPSHOT_LIMIT);
        std::fs::remove_file(path).expect("remove scenes");
    }

    #[test]
    fn validated_fields_reject_empty_and_long_names() {
        assert!(ClientShellState::validated_scene_fields("", "").is_err());
        assert!(ClientShellState::validated_scene_fields("  ", "").is_err());
        assert!(ClientShellState::validated_scene_fields(
            &"x".repeat(SCENE_NAME_MAX_BYTES + 1),
            ""
        )
        .is_err());
        let (name, note) =
            ClientShellState::validated_scene_fields(" night shift ", " a\u{0007}b ")
                .expect("valid fields");
        assert_eq!(name, "night shift");
        assert_eq!(note.as_deref(), Some("ab"));
        assert!(ClientShellState::validated_scene_fields("ok", "   ")
            .expect("blank note")
            .1
            .is_none());
    }
}
