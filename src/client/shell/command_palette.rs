use super::action_table::{
    action_spec, global_action_state, machine_action_state, ActionCategory, ActionId, ActionSpec,
    ActionTarget, PaletteMode, ACTIONS,
};
use super::feedback::ChromeContext;
use super::render::{
    display_width, modal_panel, put_right_text, put_text, render_key_hints, render_search_bar,
    OverlayRender, SearchBar,
};
use super::*;
use crate::ui::kit::menu::{menu_scroll, menu_size, menu_step, render_menu, MenuItem, MenuState};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

/// Cap on the persisted most-recently-used command list.
pub(super) const PALETTE_RECENT_LIMIT: usize = 5;

/// 按机器铺开的条目的组号起点：每台机器一组，排在动作表的组号（`u8`）之后。
const MACHINE_GROUP_BASE: u16 = 0x100;

/// 按机器铺开的条目 id（最近使用列表按它持久化）。
fn machine_item_id(spec: &ActionSpec, profile: &SavedSshEndpoint) -> String {
    format!("{}:{}", spec.key, profile.id.as_str())
}

/// One executable row of the command palette.
#[derive(Debug, Clone)]
pub(super) struct ClientPaletteItem {
    /// Stable identifier for MRU persistence (e.g. "binding:NewTab",
    /// "machine:connect:<profile-id>").
    pub(super) id: String,
    pub(super) title: String,
    /// Dim helper line: the live binding label or a machine target.
    pub(super) subtitle: String,
    /// 主菜单分类下标（`GlobalMenuTexts::categories`），由动作表给出。
    pub(super) category: usize,
    /// 分类里的分组号：目录视图在相邻两项组号不同处画分隔线。取动作表的
    /// `group`；按机器铺开的条目每台机器一组。
    pub(super) group: u16,
    pub(super) badge: bool,
    /// 暂时不可用：目录视图里置灰、搜索里不列出，激活是空操作。
    pub(super) enabled: bool,
    /// 二态开关的当前状态（`None` = 不是开关）。
    pub(super) checked: Option<bool>,
    pub(super) action: ClientPaletteAction,
}

#[derive(Debug, Clone)]
pub(super) enum ClientPaletteAction {
    /// 执行动作表里的一个动作。
    Run(ActionId, ActionTarget),
    Category(usize),
    Search,
    Back,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BrowserView {
    Menu(Option<usize>),
    Search,
}

fn navigation(item: &ClientPaletteItem) -> bool {
    matches!(
        item.action,
        ClientPaletteAction::Category(_) | ClientPaletteAction::Search | ClientPaletteAction::Back
    )
}

/// Command palette overlay state: the item index is built once at open and
/// `recent_ids` snapshots the MRU so ordering stays stable while open.
#[derive(Debug)]
pub(super) struct ClientCommandPaletteOverlay {
    pub(super) view: BrowserView,
    pub(super) reveal: bool,
    pub(super) focus: super::page::PageFocus,
    pub(super) query: TextEditor,
    pub(super) selected: usize,
    /// 指针悬浮行：只由 `Moved` 改写，`selected` 只由键盘与点击改写（MENU-01）。
    pub(super) hovered: Option<usize>,
    pub(super) scroll: usize,
    pub(super) items: Vec<ClientPaletteItem>,
    pub(super) recent_ids: Vec<String>,
    /// 条目 id → 中英两种语言（含交替态）的标题，去重后逐个存放；搜索时
    /// 每个别名单独匹配（见 `ClientShellState::palette_aliases`）。
    pub(super) aliases: HashMap<String, Vec<String>>,
    /// 打开时入口（顶栏「herdr ≡」/ 侧栏「菜单」）所在的矩形：目录视图的下拉
    /// 菜单贴着它展开（入口在上半屏向下、在下半屏向上）；没有可见入口时居中。
    pub(super) anchor: Option<Rect>,
}

/// One filtered row: the item plus fuzzy-match character positions in its
/// title (for highlight) and whether it came from the MRU section.
pub(super) struct ClientPaletteRow<'a> {
    pub(super) item: &'a ClientPaletteItem,
    pub(super) match_indices: Vec<usize>,
    pub(super) recent: bool,
}

/// 汉字与日文假名不用空格分词：逐字算词首，否则词中间的中文词（「关闭
/// 窗格」里的「窗格」）永远搜不到（浮层复审 1）。
fn is_unspaced_letter(ch: char) -> bool {
    matches!(
        u32::from(ch),
        0x3040..=0x30FF // 平假名、片假名
            | 0x31F0..=0x31FF // 片假名语音扩展
            | 0x3400..=0x4DBF // CJK 扩展 A
            | 0x4E00..=0x9FFF // CJK 基本区
            | 0xF900..=0xFAFF // CJK 兼容表意文字
            | 0xFF66..=0xFF9F // 半角片假名
            | 0x20000..=0x3134F // CJK 扩展 B–G 与兼容补充
    )
}

/// `chars[index]` 是否是一个词的开头：文本开头；汉字 / 假名（逐字成词）及
/// 紧跟其后的字符；非字母数字（空白、标点、`-_/:.` 等）之后；字母与数字
/// 之间；camelCase 的小写转大写（`moveTab` 的 T）；连写缩写后的新词
/// （`SSHImport` 的 I）。
fn is_word_start(chars: &[char], index: usize) -> bool {
    let Some(&current) = chars.get(index) else {
        return false;
    };
    let Some(&previous) = index
        .checked_sub(1)
        .and_then(|previous| chars.get(previous))
    else {
        return true;
    };
    if is_unspaced_letter(current) || is_unspaced_letter(previous) || !previous.is_alphanumeric() {
        return true;
    }
    if previous.is_numeric() != current.is_numeric() {
        return true;
    }
    if previous.is_lowercase() && current.is_uppercase() {
        return true;
    }
    previous.is_uppercase()
        && current.is_uppercase()
        && chars.get(index + 1).is_some_and(|next| next.is_lowercase())
}

/// 命中一个字符的得分：基础 1 分，紧接上一个命中 +8，落在词首 +6。
const MATCH_SCORE: i64 = 1;
const CONTIGUOUS_BONUS: i64 = 8;
const WORD_START_BONUS: i64 = 6;

/// 按词匹配，不区分大小写。返回得分（越高越好）与命中字符在 `text` 里的
/// 下标（按字符计，供标题高亮）。
///
/// 规则（冒烟 L6 收紧 + 浮层复审 1 / 2）：
/// - 第一个查询字符必须落在词首（见 `is_word_start`）；
/// - 之后每个字符要么紧接上一个命中，要么另起一个更靠后的词首——不能像
///   纯子序列那样跳到某个词中间，"mon" 不再由 "Move" 的 m、o 加上别处
///   随便一个 n 拼成；
/// - 查询里的空白不参与匹配，只要求下一个字符另起一词（"mon sys" 命中
///   "Monitor (system …)"）。
///
/// 满足规则的对齐可能不止一种（"abc" 对 "ab bc"：贪心先吃掉紧邻的 b 就再
/// 也接不上 c），这里用动态规划取得分最高的一种。
pub(super) fn fuzzy_match(query: &str, text: &str) -> Option<(i64, Vec<usize>)> {
    // 查询逐字小写展开：(字符, 是否必须另起一词)。
    let mut needles: Vec<(char, bool)> = Vec::new();
    let mut new_word = false;
    for ch in query.chars() {
        if ch.is_whitespace() {
            new_word = !needles.is_empty();
            continue;
        }
        for (offset, lower) in ch.to_lowercase().enumerate() {
            needles.push((lower, new_word && offset == 0));
        }
        new_word = false;
    }
    if needles.is_empty() {
        return Some((0, Vec::new()));
    }
    let text_chars: Vec<char> = text.chars().collect();
    // 文本逐字小写展开：(原文字符下标, 小写字符, 是否词首)。一个原文字符
    // 展开出的第 2 个及以后的小写字符不是词首，只能靠连续命中接上。
    let haystack: Vec<(usize, char, bool)> = text_chars
        .iter()
        .enumerate()
        .flat_map(|(index, ch)| {
            let word_start = is_word_start(&text_chars, index);
            ch.to_lowercase()
                .enumerate()
                .map(move |(offset, lower)| (index, lower, word_start && offset == 0))
        })
        .collect();
    // 先做一次不分配的子序列检查：连子序列都不成立就不必动态规划。
    let mut remaining = haystack.iter();
    if !needles
        .iter()
        .all(|(needle, _)| remaining.any(|(_, ch, _)| ch == needle))
    {
        return None;
    }
    let width = haystack.len();
    // best[q * width + p]：needles[..=q] 且 needles[q] 落在 haystack[p] 时的
    // 最高分，以及 needles[q - 1] 落在哪里（回溯高亮下标用）。
    let mut best: Vec<Option<(i64, usize)>> = vec![None; needles.len() * width];
    for (q, &(needle, must_start_word)) in needles.iter().enumerate() {
        // 上一个查询字符在 p 之前的最高分落点：跳到新词首时从这里接上。
        let mut earlier: Option<(i64, usize)> = None;
        for (p, &(_, ch, word_start)) in haystack.iter().enumerate() {
            let previous = q
                .checked_sub(1)
                .zip(p.checked_sub(1))
                .and_then(|(q, p)| best[q * width + p].map(|(score, _)| (score, p)));
            if let Some((score, at)) = previous {
                if earlier.is_none_or(|(best_score, _)| score > best_score) {
                    earlier = Some((score, at));
                }
            }
            if ch != needle {
                continue;
            }
            let hit = MATCH_SCORE + if word_start { WORD_START_BONUS } else { 0 };
            let candidate = if q == 0 {
                word_start.then_some((hit, 0))
            } else {
                let contiguous = previous
                    .filter(|_| !must_start_word)
                    .map(|(score, at)| (score + hit + CONTIGUOUS_BONUS, at));
                let jump = earlier
                    .filter(|_| word_start)
                    .map(|(score, at)| (score + hit, at));
                match (contiguous, jump) {
                    (Some(contiguous), Some(jump)) if jump.0 > contiguous.0 => Some(jump),
                    (Some(contiguous), _) => Some(contiguous),
                    (None, jump) => jump,
                }
            };
            best[q * width + p] = candidate;
        }
    }
    let last = needles.len() - 1;
    let (mut position, score) = (0..width)
        .filter_map(|p| best[last * width + p].map(|(score, _)| (p, score)))
        .fold(
            None,
            |winner: Option<(usize, i64)>, (p, score)| match winner {
                Some((_, winning)) if winning >= score => winner,
                _ => Some((p, score)),
            },
        )?;
    let mut positions = vec![0; needles.len()];
    for q in (0..needles.len()).rev() {
        positions[q] = position;
        if let Some((_, at)) = best[q * width + position] {
            position = at;
        }
    }
    let mut indices: Vec<usize> = Vec::with_capacity(needles.len());
    for p in positions {
        let index = haystack[p].0;
        if indices.last() != Some(&index) {
            indices.push(index);
        }
    }
    Some((score, indices))
}

/// 标题之外可供搜索的字段，每个字段单独匹配、取最高分。
///
/// 浮层复审 2：以前把 id、副标题、别名与分类拼成一串整体匹配，子序列会
/// 跨字段拼凑（"mon" = id `binding:MoveTabPrevious` 的 Mo + 分类
/// "Tabs & panes" 的 n）。id 是内部标识、从不显示，不再参与匹配；自定义
/// 命令的命令本身以前只能经 id 搜到，这里单列一项。
fn search_aliases<'a>(
    palette: &'a ClientCommandPaletteOverlay,
    item: &'a ClientPaletteItem,
) -> impl Iterator<Item = &'a str> + 'a {
    let command = match &item.action {
        ClientPaletteAction::Run(_, ActionTarget::Command(command)) => {
            Some(command.command.as_str())
        }
        _ => None,
    };
    std::iter::once(item.subtitle.as_str())
        .chain(
            palette
                .aliases
                .get(&item.id)
                .into_iter()
                .flatten()
                .map(String::as_str),
        )
        .chain(command)
        .chain([
            crate::i18n::en::TEXTS.global_menu.categories[item.category],
            crate::i18n::zh_cn::TEXTS.global_menu.categories[item.category],
        ])
        // 与标题相同的别名（当前界面语言那一份）已经先按标题匹配过。
        .filter(|alias| !alias.is_empty() && *alias != item.title)
}

/// The rows currently visible in the palette. With an empty query the MRU
/// section leads (deduplicated, capped at open time); a non-empty query
/// fuzzy-filters and sorts by score, stable by declaration order.
pub(super) fn palette_rows(palette: &ClientCommandPaletteOverlay) -> Vec<ClientPaletteRow<'_>> {
    let query = palette.query.as_str().trim();
    if query.is_empty() {
        let mut rows = Vec::new();
        let mut seen = std::collections::HashSet::new();
        if !matches!(palette.view, BrowserView::Menu(Some(_))) {
            for id in palette.recent_ids.iter().take(PALETTE_RECENT_LIMIT) {
                if let Some(item) = palette.items.iter().find(|item| {
                    &item.id == id
                        && !navigation(item)
                        && (item.enabled || palette.view != BrowserView::Search)
                }) {
                    if seen.insert(item.id.as_str()) {
                        rows.push(ClientPaletteRow {
                            item,
                            match_indices: Vec::new(),
                            recent: true,
                        });
                    }
                }
            }
        }
        for item in &palette.items {
            let show = match palette.view {
                BrowserView::Search => !navigation(item) && item.enabled,
                BrowserView::Menu(None) => matches!(
                    item.action,
                    ClientPaletteAction::Category(_) | ClientPaletteAction::Search
                ),
                BrowserView::Menu(Some(group)) => {
                    (!navigation(item) && item.category == group)
                        || matches!(item.action, ClientPaletteAction::Back)
                }
            };
            if show && seen.insert(item.id.as_str()) {
                rows.push(ClientPaletteRow {
                    item,
                    match_indices: Vec::new(),
                    recent: false,
                });
            }
        }
        return rows;
    }
    let mut scored = palette
        .items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            if navigation(item) || !item.enabled {
                return None;
            }
            fuzzy_match(query, &item.title)
                .map(|(score, indices)| (score + 50, indices))
                .or_else(|| {
                    search_aliases(palette, item)
                        .filter_map(|alias| fuzzy_match(query, alias))
                        .map(|(score, _)| score)
                        .max()
                        .map(|score| (score, Vec::new()))
                })
                .map(|(score, indices)| {
                    (
                        score,
                        index,
                        ClientPaletteRow {
                            item,
                            match_indices: indices,
                            recent: false,
                        },
                    )
                })
        })
        .collect::<Vec<_>>();
    scored.sort_by(
        |(left_score, left_index, _), (right_score, right_index, _)| {
            right_score
                .cmp(left_score)
                .then_with(|| left_index.cmp(right_index))
        },
    );
    scored
        .into_iter()
        .map(|(_, _, row)| row)
        .collect::<Vec<_>>()
}

/// 目录视图：主菜单与分类子菜单（查询为空）走 kit::menu 画成下拉菜单；搜索
/// 视图仍是带模糊高亮的列表页。
fn is_catalog(palette: &ClientCommandPaletteOverlay) -> bool {
    matches!(palette.view, BrowserView::Menu(_)) && palette.query.as_str().trim().is_empty()
}

/// 目录视图的 kit 条目，以及每条对应的 [`palette_rows`] 下标（标题与分隔线为
/// `None`）。`selected` / `hovered` / 命中表都按 `palette_rows` 下标说话，
/// 换算只在这里。
struct CatalogMenu<'a> {
    items: Vec<MenuItem<'a>>,
    ids: Vec<Option<usize>>,
}

impl CatalogMenu<'_> {
    /// `palette_rows` 下标 → kit 下标；不在菜单里时给越界值（kit 视为未选中）。
    fn kit_index(&self, row: usize) -> usize {
        self.ids
            .iter()
            .position(|id| *id == Some(row))
            .unwrap_or(usize::MAX)
    }

    /// kit 下标 → `palette_rows` 下标。
    fn row(&self, kit: usize) -> Option<usize> {
        self.ids.get(kit).copied().flatten()
    }
}

fn machine_item(item: &ClientPaletteItem) -> bool {
    item.group >= MACHINE_GROUP_BASE
}

fn catalog_item(item: &ClientPaletteItem) -> MenuItem<'_> {
    let base = if matches!(item.action, ClientPaletteAction::Category(_)) {
        MenuItem::submenu(&item.title)
    } else {
        MenuItem::action(&item.title)
    };
    // 右列只放快捷键：按机器铺开的条目的副标题是机器地址，改由分组标题给出。
    let shortcut =
        (!item.subtitle.is_empty() && !machine_item(item)).then_some(item.subtitle.as_str());
    MenuItem {
        shortcut,
        enabled: item.enabled,
        checked: item.checked,
        danger: matches!(item.action, ClientPaletteAction::Run(id, _) if action_spec(id).danger),
        badge: item.badge,
        ..base
    }
}

/// 按行投影排出目录视图的菜单：主菜单 = 「最近使用」段 + 分类（子菜单项）+
/// 「搜索命令」；分类子菜单 = 分类名标题 + 条目（组号变化处画分隔线，每台
/// 机器一组、以机器地址作组标题）+ 「返回」。
fn catalog_menu<'a>(
    palette: &ClientCommandPaletteOverlay,
    rows: &[ClientPaletteRow<'a>],
) -> CatalogMenu<'a> {
    let t = &crate::i18n::texts().global_menu;
    let mut menu = CatalogMenu {
        items: Vec::with_capacity(rows.len() + 4),
        ids: Vec::with_capacity(rows.len() + 4),
    };
    if let BrowserView::Menu(Some(category)) = palette.view {
        if let Some(title) = t.categories.get(category) {
            menu.items.push(MenuItem::header(title));
            menu.ids.push(None);
        }
    }
    let mut previous: Option<&ClientPaletteRow<'a>> = None;
    for (index, row) in rows.iter().enumerate() {
        let separator = previous.is_some_and(|previous| {
            (previous.recent && !row.recent)
                || matches!(
                    row.item.action,
                    ClientPaletteAction::Search | ClientPaletteAction::Back
                )
                || (!row.recent
                    && !navigation(previous.item)
                    && !navigation(row.item)
                    && previous.item.group != row.item.group)
        });
        if separator {
            menu.items.push(MenuItem::separator());
            menu.ids.push(None);
        } else if previous.is_none() && row.recent {
            menu.items.push(MenuItem::header(t.recent));
            menu.ids.push(None);
        }
        let new_machine = !row.recent
            && machine_item(row.item)
            && previous.is_none_or(|previous| previous.item.group != row.item.group);
        if new_machine && !row.item.subtitle.is_empty() {
            menu.items.push(MenuItem::header(&row.item.subtitle));
            menu.ids.push(None);
        }
        menu.items.push(catalog_item(row.item));
        menu.ids.push(Some(index));
        previous = Some(row);
    }
    menu
}

/// 目录视图菜单的摆放：锚点、允许占用的区域与边框内可见行数。视图计算与
/// 渲染共用同一口径。有入口时像桌面下拉菜单一样贴着入口展开：下方放得下（或
/// 下方比上方宽裕）就向下，否则向上、下沿贴住入口；只占入口一侧，放不下就在
/// 那一侧滚动，不盖住入口本身。没有入口时水平居中、垂直偏上。
struct CatalogGeometry {
    anchor: (u16, u16),
    bounds: Rect,
    visible: usize,
}

fn catalog_geometry(
    palette: &ClientCommandPaletteOverlay,
    items: &[MenuItem<'_>],
    area: Rect,
) -> CatalogGeometry {
    let (width, natural) = menu_size(items);
    let side = palette.anchor.and_then(|entry| {
        let below_y = entry.bottom().clamp(area.y, area.bottom());
        let below = Rect::new(area.x, below_y, area.width, area.bottom() - below_y);
        let above_bottom = entry.y.clamp(area.y, area.bottom());
        let above = Rect::new(area.x, area.y, area.width, above_bottom - area.y);
        let region = if natural <= below.height || below.height >= above.height {
            below
        } else {
            above
        };
        // 连边框都放不下的一侧不用，退回整屏。
        (region.height >= 3).then(|| {
            let height = natural.min(region.height);
            let y = if region == below {
                region.y
            } else {
                region.bottom() - height
            };
            ((entry.x, y), region, height)
        })
    });
    let (anchor, bounds, height) = side.unwrap_or_else(|| {
        let height = natural.min(area.height);
        (
            (
                area.x + area.width.saturating_sub(width) / 2,
                area.y + area.height.saturating_sub(height) / 3,
            ),
            area,
            height,
        )
    });
    CatalogGeometry {
        anchor,
        bounds,
        visible: usize::from(height.saturating_sub(2)),
    }
}

/// 目录视图的滚动起点：键盘刚移动过（`reveal`）就把选中项滚进窗口，否则只把
/// 滚轮留下的 `scroll` 收回合法范围、不拉回选中项。
fn catalog_scroll(
    palette: &ClientCommandPaletteOverlay,
    menu: &CatalogMenu<'_>,
    visible: usize,
) -> usize {
    let highlighted = if palette.reveal {
        menu.kit_index(palette.selected)
    } else {
        usize::MAX
    };
    menu_scroll(
        &menu.items,
        &MenuState {
            highlighted,
            scroll: palette.scroll,
            ..MenuState::default()
        },
        visible,
    )
}

/// 翻页：向 `delta` 的方向最多走 `|delta|` 个可激活项，到头停住、不回绕。
fn catalog_page_step(items: &[MenuItem<'_>], from: usize, delta: isize) -> Option<usize> {
    let forward = delta > 0;
    let mut current = from;
    for _ in 0..delta.unsigned_abs() {
        let Some(next) = menu_step(items, current, delta.signum()) else {
            break;
        };
        if current < items.len() && (next > current) != forward {
            break;
        }
        current = next;
    }
    (current < items.len()).then_some(current)
}

/// 目录视图里选中项不可激活（置灰、或行集合刚换过）时落到下一个可激活项。
fn settle_catalog_selection(palette: &mut ClientCommandPaletteOverlay) {
    if !is_catalog(palette) {
        return;
    }
    let settled = {
        let rows = palette_rows(palette);
        let menu = catalog_menu(palette, &rows);
        menu_step(&menu.items, menu.kit_index(palette.selected), 0).and_then(|kit| menu.row(kit))
    };
    if let Some(row) = settled {
        palette.selected = row;
    }
}

impl ClientShellState {
    /// 命令面板的条目全集：动作表按声明顺序展开（聚焦对象语义的一条、按机器
    /// 铺开的每台机器一组、自定义命令每条一项），再追加分类与导航项。暂时不可
    /// 用的动作照样列出（目录视图置灰），对当前布局不适用的不列。
    fn build_palette_items(&self) -> Vec<ClientPaletteItem> {
        let texts = crate::i18n::texts();
        let keybinds = &self.config.keybinds.keybinds;
        let cx = self.global_action_context();
        let mut items = Vec::new();
        let mut machines_listed = false;
        for spec in ACTIONS {
            match spec.palette {
                PaletteMode::Hidden => {}
                PaletteMode::Focused => {
                    let state = global_action_state(spec.id, &cx);
                    if !state.visible {
                        continue;
                    }
                    items.push(ClientPaletteItem {
                        id: spec.key.to_owned(),
                        title: spec.title_text(texts, state.alternate).to_owned(),
                        subtitle: spec
                            .binding
                            .and_then(|binding| (binding.keys)(keybinds).label())
                            .unwrap_or_default(),
                        category: spec.category.index(),
                        group: spec.group.into(),
                        badge: state.badge,
                        enabled: state.enabled,
                        checked: state.checked,
                        action: ClientPaletteAction::Run(spec.id, ActionTarget::Focused),
                    });
                }
                PaletteMode::PerCommand => {
                    for command in &keybinds.custom_commands {
                        items.push(ClientPaletteItem {
                            id: format!("{}:{}", spec.key, command.command),
                            title: command
                                .description
                                .clone()
                                .unwrap_or_else(|| command.command.clone()),
                            subtitle: command.label.clone(),
                            category: spec.category.index(),
                            group: spec.group.into(),
                            badge: false,
                            enabled: true,
                            checked: None,
                            action: ClientPaletteAction::Run(
                                spec.id,
                                ActionTarget::Command(command.clone()),
                            ),
                        });
                    }
                }
                // 机器动作组整组按机器展开一次（机器之间不交错），位置取组里
                // 第一条的声明位置。
                PaletteMode::PerMachine if !machines_listed => {
                    machines_listed = true;
                    self.push_machine_palette_items(&mut items);
                }
                PaletteMode::PerMachine => {}
            }
        }
        for (index, title) in texts.global_menu.categories.iter().enumerate() {
            let badge = items
                .iter()
                .any(|item| item.category == index && item.badge);
            // 目录视图里是子菜单项（行尾画 ▸），不再另写条目数与箭头。
            items.push(ClientPaletteItem {
                id: format!("category:{index}"),
                title: title.to_string(),
                subtitle: String::new(),
                category: index,
                group: 0,
                badge,
                enabled: true,
                checked: None,
                action: ClientPaletteAction::Category(index),
            });
        }
        let global_menu = &texts.global_menu;
        for (id, title, action) in [
            (
                "search",
                global_menu.command_search,
                ClientPaletteAction::Search,
            ),
            ("back", global_menu.back, ClientPaletteAction::Back),
        ] {
            items.push(ClientPaletteItem {
                id: id.into(),
                title: title.into(),
                subtitle: if id == "search" {
                    keybinds.command_search.label().unwrap_or_default()
                } else {
                    String::new()
                },
                category: ActionCategory::Help.index(),
                group: 0,
                badge: false,
                enabled: true,
                checked: None,
                action,
            });
        }
        items
    }

    /// 机器动作组按已保存机器展开：与机器行右键菜单同一组条目、同一套可用性。
    fn push_machine_palette_items(&self, items: &mut Vec<ClientPaletteItem>) {
        let texts = crate::i18n::texts();
        for (machine, profile) in self.saved_profiles.iter().enumerate() {
            let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
            let group =
                MACHINE_GROUP_BASE.saturating_add(u16::try_from(machine).unwrap_or(u16::MAX));
            let online = self.endpoint_is_online(&endpoint_id);
            let active = self.active_endpoint_id == endpoint_id;
            for spec in ACTIONS
                .iter()
                .filter(|spec| spec.palette == PaletteMode::PerMachine)
            {
                let state = machine_action_state(spec.id, profile.enabled, online, active);
                items.push(ClientPaletteItem {
                    id: machine_item_id(spec, profile),
                    title: crate::i18n::fill(
                        spec.title_text(texts, state.alternate),
                        &[("label", &profile.label)],
                    ),
                    subtitle: profile.target.clone(),
                    category: spec.category.index(),
                    group,
                    badge: state.badge,
                    enabled: state.enabled,
                    checked: state.checked,
                    action: ClientPaletteAction::Run(
                        spec.id,
                        ActionTarget::Machine(endpoint_id.clone()),
                    ),
                });
            }
        }
    }

    /// 搜索别名：动作在中英两种语言下的标题（含交替态），去重后逐个存放；
    /// 按机器铺开的条目填好机器名。界面是中文时照样能用英文词搜到，反之
    /// 亦然。以前按机器的条目只能经 id（`machine:connect:…`）搜到英文词，
    /// id 不再参与匹配后由这里补上（浮层复审 2）。
    fn palette_aliases(&self) -> HashMap<String, Vec<String>> {
        let mut aliases = HashMap::<String, Vec<String>>::new();
        let mut add = |id: String, title: String| {
            let entry = aliases.entry(id).or_default();
            if !entry.contains(&title) {
                entry.push(title);
            }
        };
        for texts in [&crate::i18n::en::TEXTS, &crate::i18n::zh_cn::TEXTS] {
            for spec in ACTIONS {
                for alternate in [false, true] {
                    let title = spec.title_text(texts, alternate);
                    match spec.palette {
                        PaletteMode::Focused => add(spec.key.to_owned(), title.to_owned()),
                        PaletteMode::PerMachine => {
                            for profile in &self.saved_profiles {
                                add(
                                    machine_item_id(spec, profile),
                                    crate::i18n::fill(title, &[("label", &profile.label)]),
                                );
                            }
                        }
                        PaletteMode::Hidden | PaletteMode::PerCommand => {}
                    }
                }
            }
        }
        aliases
    }

    pub(super) fn toggle_global_menu(&mut self) {
        if matches!(self.overlay, Some(ClientShellOverlay::CommandPalette(_))) {
            self.close_command_browser();
        } else {
            self.open_command_browser(BrowserView::Menu(None));
        }
    }

    pub(super) fn open_command_search(&mut self) {
        self.open_command_browser(BrowserView::Search);
    }

    fn open_command_browser(&mut self, view: BrowserView) {
        self.cancel_frozen_selection();
        if !matches!(self.overlay, Some(ClientShellOverlay::CommandPalette(_))) {
            self.browser_return = self.overlay.take().map(Box::new);
        }
        let aliases = self.palette_aliases();
        self.overlay = Some(ClientShellOverlay::CommandPalette(
            ClientCommandPaletteOverlay {
                focus: if view == BrowserView::Search {
                    super::page::PageFocus::Search
                } else {
                    super::page::PageFocus::Navigation
                },
                view,
                aliases,
                reveal: true,
                query: TextEditor::default(),
                selected: 0,
                hovered: None,
                scroll: 0,
                items: self.build_palette_items(),
                recent_ids: self.palette_recent.clone(),
                // 上一帧画出来的入口：顶栏「herdr ≡」或侧栏「菜单」。
                anchor: (!self.hits.global_launcher.is_empty())
                    .then_some(self.hits.global_launcher),
            },
        ));
        if let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() {
            settle_catalog_selection(palette);
        }
    }

    pub(super) fn close_command_browser(&mut self) {
        self.overlay = self.browser_return.take().map(|page| *page);
    }

    pub(super) fn browser_back(&mut self) {
        if let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() {
            if let BrowserView::Menu(Some(category)) = palette.view {
                palette.view = BrowserView::Menu(None);
                palette.query = TextEditor::default();
                // 回到主菜单时选中刚才进入的分类，与桌面菜单退回父级一致。
                let parent = format!("category:{category}");
                palette.selected = palette_rows(palette)
                    .iter()
                    .position(|row| row.item.id == parent)
                    .unwrap_or(0);
                palette.scroll = 0;
                palette.reveal = true;
                // 行集合整个换了，旧行号立刻失效：不清就会在新列表里把同号的
                // 那一行画成「悬浮」，而指针其实停在别处（MENU-01）。
                palette.hovered = None;
                settle_catalog_selection(palette);
                return;
            }
        }
        self.close_command_browser();
    }

    pub(super) fn move_palette_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() else {
            return;
        };
        if is_catalog(palette) {
            // 目录视图按 kit::menu 的规则走：跳过标题、分隔线与置灰项；单步回绕，
            // 翻页到头停住。
            let next = {
                let rows = palette_rows(palette);
                let menu = catalog_menu(palette, &rows);
                let from = menu.kit_index(palette.selected);
                let kit = if delta.unsigned_abs() > 1 {
                    catalog_page_step(&menu.items, from, delta)
                } else {
                    menu_step(&menu.items, from, delta)
                };
                kit.and_then(|kit| menu.row(kit))
            };
            if let Some(row) = next {
                palette.selected = row;
            }
            palette.reveal = true;
            palette.hovered = None;
            return;
        }
        let count = palette_rows(palette).len();
        if count == 0 {
            palette.selected = 0;
            palette.scroll = 0;
            return;
        }
        palette.reveal = true;
        palette.selected =
            (palette.selected as isize + delta).clamp(0, count.saturating_sub(1) as isize) as usize;
        // reveal 会在渲染期重算 scroll，指针下面的行可能已经换了。
        palette.hovered = None;
    }

    pub(super) fn set_palette_selection(&mut self, index: usize) -> bool {
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() else {
            return false;
        };
        let changed = palette.selected != index;
        palette.selected = index;
        changed
    }

    /// 指针悬浮行。`None` 表示指针不在任何行上——出界也要写，否则高亮会留在
    /// 鼠标早已离开的那一行（MENU-01）。
    pub(super) fn set_palette_hover(&mut self, hovered: Option<usize>) -> bool {
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() else {
            return false;
        };
        let changed = palette.hovered != hovered;
        palette.hovered = hovered;
        changed
    }

    pub(super) fn scroll_palette(&mut self, delta: isize) {
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() else {
            return;
        };
        palette.reveal = false;
        palette.scroll = palette.scroll.saturating_add_signed(delta);
        // 滚动会把别的行挪到指针下面，旧的 hover 行号立刻失效。
        palette.hovered = None;
    }

    /// 目录视图专有的按键：→ 进入高亮的分类、← 回到主菜单、Home / End 到首尾
    /// 可用项。其余按键（上下、回车、Esc、打字即搜索）照旧由调用方处理；不在
    /// 目录视图时返回 `false`。
    pub(super) fn route_palette_catalog_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() else {
            return false;
        };
        if !is_catalog(palette)
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        match key.code {
            KeyCode::Right => {
                let selected = palette.selected;
                let category = palette_rows(palette)
                    .get(selected)
                    .is_some_and(|row| matches!(row.item.action, ClientPaletteAction::Category(_)));
                if category {
                    self.activate_palette_item(selected, outcome);
                }
            }
            KeyCode::Left => {
                if matches!(palette.view, BrowserView::Menu(Some(_))) {
                    self.browser_back();
                }
            }
            KeyCode::Home | KeyCode::End => {
                let next = {
                    let rows = palette_rows(palette);
                    let menu = catalog_menu(palette, &rows);
                    let delta = if key.code == KeyCode::Home { 1 } else { -1 };
                    menu_step(&menu.items, usize::MAX, delta).and_then(|kit| menu.row(kit))
                };
                if let Some(row) = next {
                    palette.selected = row;
                    palette.reveal = true;
                    palette.hovered = None;
                }
            }
            _ => return false,
        }
        outcome.repaint = true;
        true
    }

    /// 目录视图里 Enter 应该激活的行：`selected` 是键盘上一次移动或点击
    /// 落下的项，滚轮（`scroll_palette`）只挪 `scroll`、不跟着挪
    /// `selected`（放弃键盘高亮，见 `kit::menu::menu_scroll` 文档），于是
    /// `selected` 可能落在当前可见窗口外——`render_catalog` 那时不画任何
    /// 高亮，但 `selected` 本身仍是一个"看不见"的有效下标，Enter 照样会
    /// 激活它。这里按渲染同一套窗口口径重新判断：还在窗口内就用它自己，
    /// 否则改用窗口内离原位置最近的可激活项（冒烟 B1）。非目录视图（搜索
    /// 结果列表）不受此限制，直接用当前选中。
    pub(super) fn palette_enter_target(&self) -> Option<usize> {
        let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_ref() else {
            return None;
        };
        if !is_catalog(palette) {
            return Some(palette.selected);
        }
        let (cols, rows) = self.last_composed_size?;
        let rows_data = palette_rows(palette);
        let menu = catalog_menu(palette, &rows_data);
        let geometry = catalog_geometry(palette, &menu.items, Rect::new(0, 0, cols, rows));
        let selected_kit = menu.kit_index(palette.selected);
        let window_end = palette.scroll.saturating_add(geometry.visible.max(1));
        if selected_kit >= palette.scroll && selected_kit < window_end {
            return Some(palette.selected);
        }
        let lo = palette.scroll.min(menu.items.len());
        let hi = window_end.min(menu.items.len());
        let fallback = if selected_kit < palette.scroll {
            (lo..hi).find(|&kit| menu.items[kit].is_activatable())
        } else {
            (lo..hi).rev().find(|&kit| menu.items[kit].is_activatable())
        };
        Some(
            fallback
                .and_then(|kit| menu.row(kit))
                .unwrap_or(palette.selected),
        )
    }

    pub(super) fn activate_palette_item(&mut self, index: usize, outcome: &mut ClientShellInput) {
        let (id, action) = {
            let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_ref() else {
                return;
            };
            let rows = palette_rows(palette);
            let Some(row) = rows.get(index) else {
                return;
            };
            if !row.item.enabled {
                // 不可用的条目：面板保持打开，什么都不做。
                return;
            }
            (row.item.id.clone(), row.item.action.clone())
        };
        if let Some(ClientShellOverlay::CommandPalette(palette)) = self.overlay.as_mut() {
            let next = match action {
                ClientPaletteAction::Category(index) => Some(BrowserView::Menu(Some(index))),
                ClientPaletteAction::Search => Some(BrowserView::Search),
                ClientPaletteAction::Back => Some(BrowserView::Menu(None)),
                _ => None,
            };
            if let Some(view) = next {
                palette.view = view;
                palette.selected = 0;
                palette.scroll = 0;
                palette.reveal = true;
                palette.query = TextEditor::default();
                // 点击分类进子菜单之后不会再来一个 `Moved`，旧行号会立刻在新
                // 列表里画出一条假的悬浮行（MENU-01）。
                palette.hovered = None;
                settle_catalog_selection(palette);
                outcome.repaint = true;
                return;
            }
        }
        self.close_command_browser();
        self.palette_recent.retain(|entry| entry != &id);
        self.palette_recent.insert(0, id);
        self.palette_recent.truncate(PALETTE_RECENT_LIMIT);
        self.persist_chrome_preferences(outcome);
        if let ClientPaletteAction::Run(id, target) = action {
            self.run_action(id, target, outcome);
        }
        outcome.repaint = true;
    }
}

/// 命令面板的列表窗口：行投影（`recent` 分组占行）与几何一次算好，视图计算
/// 阶段与渲染阶段共用（STATE-04）。
pub(crate) fn palette_window(
    area: Rect,
    page_bounds: Option<Rect>,
    palette: &ClientCommandPaletteOverlay,
) -> Option<crate::client::shell::page::ListWindow> {
    let rows = palette_rows(palette);
    if is_catalog(palette) {
        // 目录视图是贴着入口的下拉菜单，与浮动页无关；滚动口径同 kit::menu。
        let menu = catalog_menu(palette, &rows);
        let geometry = catalog_geometry(palette, &menu.items, area);
        return Some(super::page::ListWindow {
            body: geometry.bounds,
            visible: geometry.visible,
            start: catalog_scroll(palette, &menu, geometry.visible),
        });
    }
    let menu_height = match palette.view {
        BrowserView::Menu(_) if palette.query.as_str().is_empty() => {
            (rows.len().saturating_add(6).min(22)) as u16
        }
        _ => 22,
    };
    let outer = page_bounds
        .map(|rect| rect.intersection(area))
        .or_else(|| {
            crate::ui::modal_rect(area, crate::ui::ModalSize::Large.with_height(menu_height))
        })?;
    let inner = super::render::panel_inner(outer)?;
    let searching = palette.view == BrowserView::Search || !palette.query.as_str().is_empty();
    let layout = super::page::PageLayout::new(inner, 0, searching, false);
    let mut visual = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if (index == 0 && row.recent) || (index > 0 && rows[index - 1].recent && !row.recent) {
            visual.push(None);
        }
        visual.push(Some(index));
    }
    let selected = palette.selected.min(rows.len().saturating_sub(1));
    let selected_line = visual
        .iter()
        .position(|entry| *entry == Some(selected))
        .unwrap_or(0);
    Some(super::page::list_window(
        layout.content,
        1,
        visual.len(),
        palette.scroll,
        selected_line,
        palette.reveal,
    ))
}

/// 画目录视图（只读状态）：滚动起点已在视图计算阶段写回 `palette.scroll`；
/// 选中项不在窗口里（滚轮滚走了）时不画键盘高亮。
fn render_catalog(
    b: &mut Buffer,
    palette: &ClientCommandPaletteOverlay,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    let rows = palette_rows(palette);
    let menu = catalog_menu(palette, &rows);
    let geometry = catalog_geometry(palette, &menu.items, b.area);
    let selected = menu.kit_index(palette.selected);
    let in_window = selected >= palette.scroll && selected - palette.scroll < geometry.visible;
    let state = MenuState {
        highlighted: if in_window { selected } else { usize::MAX },
        hovered: palette.hovered.map(|row| menu.kit_index(row)),
        hover_bg: Some(cx.components.hover_bg),
        scroll: palette.scroll,
    };
    let rendered = render_menu(
        b,
        geometry.anchor,
        geometry.bounds,
        &menu.items,
        &state,
        cx.glyphs,
        false,
        cx.palette,
    );
    if rendered.area.is_empty() {
        return None;
    }
    let menu_rows = rendered
        .rows
        .iter()
        .filter_map(|(rect, kit)| menu.row(*kit).map(|row| (*rect, row)))
        .collect();
    Some(OverlayRender {
        area: rendered.area,
        menu_popup: rendered.area,
        menu_rows,
        ..OverlayRender::default()
    })
}

pub(crate) fn render_command_palette(
    b: &mut Buffer,
    palette: &ClientCommandPaletteOverlay,
    cx: &ChromeContext<'_>,
) -> Option<OverlayRender> {
    use super::page::{list_start, PageLayout};
    if is_catalog(palette) {
        return render_catalog(b, palette, cx);
    }
    let p = cx.palette;
    let t = &crate::i18n::texts().global_menu;
    let rows = palette_rows(palette);
    let menu_height = match palette.view {
        BrowserView::Menu(_) if palette.query.as_str().is_empty() => {
            (rows.len().saturating_add(6).min(22)) as u16
        }
        _ => 22,
    };
    let (outer, inner) = modal_panel(
        b,
        crate::ui::ModalSize::Large.with_height(menu_height),
        p.accent,
        cx,
    )?;
    let searching = palette.view == BrowserView::Search || !palette.query.as_str().is_empty();
    let layout = PageLayout::new(inner, 0, searching, false);
    let title = match palette.view {
        BrowserView::Search => t.command_search,
        BrowserView::Menu(None) => t.main_menu,
        BrowserView::Menu(Some(group)) => t.categories[group],
    };
    put_text(
        b,
        layout.header.x,
        layout.header.y,
        layout.header.width,
        title,
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
    );
    let cursor = if searching {
        render_search_bar(
            b,
            layout.search,
            &SearchBar {
                focused: palette.focus == super::page::PageFocus::Search || searching,
                query: &palette.query,
                hint: t.search_hint,
                status: None,
                echo_query: false,
                count: Some(rows.len().to_string()),
            },
            p,
        )
    } else {
        None
    };
    let body = layout.content;
    let mut visual = Vec::new();
    let has_recent = rows.first().is_some_and(|row| row.recent);
    for (index, row) in rows.iter().enumerate() {
        if (index == 0 && row.recent) || (index > 0 && rows[index - 1].recent && !row.recent) {
            visual.push(None);
        }
        visual.push(Some(index));
    }
    let selected = palette.selected.min(rows.len().saturating_sub(1));
    let selected_line = visual
        .iter()
        .position(|entry| *entry == Some(selected))
        .unwrap_or(0);
    let scroll = list_start(
        palette.scroll,
        selected_line,
        visual.len(),
        usize::from(body.height),
        palette.reveal,
    );
    let mut row_hits = Vec::new();
    if rows.is_empty() {
        put_text(
            b,
            body.x,
            body.y,
            body.width,
            t.no_matches,
            Style::default().fg(p.overlay0),
        );
    }
    for (offset, visual_index) in (scroll..visual.len())
        .take(usize::from(body.height))
        .enumerate()
    {
        let rect = Rect::new(
            body.x,
            body.y + offset as u16,
            body.width.saturating_sub(1),
            1,
        );
        let Some(index) = visual[visual_index] else {
            put_text(
                b,
                rect.x,
                rect.y,
                rect.width,
                if visual_index == 0 && has_recent {
                    t.recent
                } else {
                    title
                },
                Style::default().fg(p.overlay0).add_modifier(Modifier::BOLD),
            );
            continue;
        };
        let row = &rows[index];
        let chosen = index == selected;
        let style = list_row_style(p, cx.components, chosen, palette.hovered == Some(index));
        // 暂时不可用的条目只在目录视图里出现：置灰，选中也不反色成可执行的样子。
        let style = if row.item.enabled {
            style
        } else {
            style.fg(p.overlay0)
        };
        b.set_style(rect, style);
        let marker_width = 2.min(rect.width);
        // 标记列：第 1 格是键盘选中的「›」，第 2 格是开关的勾选态。
        let marker = match (chosen, row.item.checked == Some(true)) {
            (true, true) => "›✓",
            (true, false) => "› ",
            (false, true) => " ✓",
            (false, false) => "  ",
        };
        put_text(b, rect.x, rect.y, marker_width, marker, style);
        let badge_width = if row.item.badge { 2 } else { 0 };
        let available = rect.width.saturating_sub(marker_width + badge_width);
        let subtitle_width = if available < 28 || row.item.subtitle.is_empty() {
            0
        } else {
            display_width(&row.item.subtitle)
                .saturating_add(1)
                .min(available / 2)
        };
        let title_width = available.saturating_sub(subtitle_width);
        let mut x = rect.x + marker_width;
        let mut used = 0;
        use unicode_segmentation::UnicodeSegmentation;
        let mut char_index = 0;
        for grapheme in row.item.title.graphemes(true) {
            let width = display_width(grapheme);
            if used + width > title_width {
                break;
            }
            let end = char_index + grapheme.chars().count();
            let matched = row
                .match_indices
                .iter()
                .any(|index| *index >= char_index && *index < end);
            let emphasis = if !chosen && matched {
                style.fg(p.mauve).add_modifier(Modifier::BOLD)
            } else {
                style
            };
            put_text(b, x, rect.y, width, grapheme, emphasis);
            char_index = end;
            x += width;
            used += width;
        }
        if subtitle_width > 0 {
            let subtitle = Rect::new(
                rect.right() - badge_width - subtitle_width,
                rect.y,
                subtitle_width,
                1,
            );
            put_right_text(
                b,
                subtitle,
                rect.y,
                &row.item.subtitle,
                if chosen { style } else { style.fg(p.overlay0) },
            );
        }
        if row.item.badge {
            put_text(
                b,
                rect.right().saturating_sub(2),
                rect.y,
                2,
                " ●",
                if chosen { style } else { style.fg(p.accent) },
            );
        }
        row_hits.push((rect, index));
    }
    // 冒烟 L16：滑块长度按可视比例——之前恒为 1 行，与 `release_notes` /
    // `help` 等浮层的滚动条观感不一致，长列表里看不出还剩多少内容。复用
    // 与它们相同的 `ScrollMetrics` + `render_scrollbar_buffer`。
    if visual.len() > usize::from(body.height) && body.width > 0 && body.height > 0 {
        let track = Rect::new(body.right() - 1, body.y, 1, body.height);
        let viewport_rows = usize::from(body.height);
        let max_offset_from_bottom = visual.len().saturating_sub(viewport_rows);
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: max_offset_from_bottom.saturating_sub(scroll),
            max_offset_from_bottom,
            viewport_rows,
        };
        crate::ui::render_scrollbar_buffer(b, metrics, track, p.overlay0, p.accent, "┃");
    }
    let mut hints = vec![
        ("enter".into(), t.footer_run.into()),
        ("↑↓".into(), t.footer_select.into()),
    ];
    // 冒烟 L6：已经在搜索态时页脚不再提示「/ 搜索命令」——用户已经在搜索
    // 框里打字，这条提示只会让人以为还没进入搜索。
    if !searching {
        hints.push(("/".into(), t.command_search.into()));
    }
    hints.push((
        "esc".into(),
        if matches!(palette.view, BrowserView::Menu(Some(_))) {
            t.back
        } else {
            t.footer_close
        }
        .into(),
    ));
    render_key_hints(b, layout.footer, &hints, p, cx.components);
    Some(OverlayRender {
        area: outer,
        menu_popup: outer,
        menu_search: layout.search,
        menu_rows: row_hits,
        cursor,
        ..OverlayRender::default()
    })
}

impl ClientShellState {
    /// 命令面板 / 全局菜单打开时的鼠标分派：不在该浮层时返回 `false`，
    /// 由 `mouse.rs::handle_mouse` 继续往下走。
    pub(super) fn handle_command_palette_mouse(
        &mut self,
        mouse: MouseEvent,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        if !matches!(self.overlay, Some(ClientShellOverlay::CommandPalette(_))) {
            return false;
        }
        let row_hit = self
            .hits
            .global_menu_rows
            .iter()
            .find(|(rect, _)| super::contains(*rect, point))
            .copied();
        match mouse.kind {
            MouseEventKind::Moved => {
                // 指针只写 hover：键盘选中不被「鼠标路过」改写，出界也要写
                // None 才不会留下残影（MENU-01）。
                outcome.repaint |= self.set_palette_hover(row_hit.map(|(_, index)| index));
            }
            MouseEventKind::ScrollUp => {
                self.scroll_palette(-(self.config.mouse_scroll_lines as isize));
                outcome.repaint = true;
            }
            MouseEventKind::ScrollDown => {
                self.scroll_palette(self.config.mouse_scroll_lines as isize);
                outcome.repaint = true;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if super::contains(self.hits.global_launcher, point) {
                    self.toggle_global_menu();
                    outcome.repaint = true;
                } else if let Some((_, index)) = row_hit {
                    // 点击是显式选择：与键盘一样改写 `selected`，再激活。
                    self.set_palette_selection(index);
                    self.activate_palette_item(index, outcome);
                } else if !super::contains(self.hits.menu_popup, point) {
                    self.close_command_browser();
                    outcome.repaint = true;
                }
            }
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_match_requires_subsequence_in_order() {
        assert!(fuzzy_match("nt", "new tab").is_some());
        assert!(fuzzy_match("tn", "new tab").is_none());
        assert!(fuzzy_match("zz", "new tab").is_none());
        assert_eq!(fuzzy_match("", "anything"), Some((0, Vec::new())));
    }

    #[test]
    fn fuzzy_match_is_case_insensitive_and_reports_char_indices() {
        let (score, indices) = fuzzy_match("NT", "New Tab").expect("match");
        assert_eq!(indices, vec![0, 4]);
        assert!(score > 0);
    }

    #[test]
    fn fuzzy_match_prefers_consecutive_and_word_start_runs() {
        let (run_score, _) = fuzzy_match("tab", "tab").expect("exact");
        let (spread_score, _) = fuzzy_match("tab", "t a   b").expect("spread");
        assert!(run_score > spread_score);
    }

    #[test]
    fn fuzzy_match_handles_cjk_titles() {
        let (_, indices) = fuzzy_match("机器", "连接 机器 面板").expect("cjk match");
        assert_eq!(indices.len(), 2);
        assert!(fuzzy_match("不存在xyz", "连接 机器 面板").is_none());
    }

    /// 冒烟 L6：收紧前，纯子序列匹配对"首字符落在哪"没有任何要求——
    /// 查询 "ta" 会命中 "attach machine" 里 "aTtAch" 词中间的 t（下标 1）
    /// 与随后的 a（下标 3），两者都不是词的开头，观感是"什么都能命中"。
    /// 收紧后：首字符必须落在词首（见 `is_word_start`），这类词中间起步
    /// 的命中不再算数；真正的词前缀（如 "im" 命中 "import" 的开头）依然
    /// 命中。
    #[test]
    fn fuzzy_match_requires_the_first_character_to_land_on_a_token_boundary() {
        // 收紧前：`fuzzy_match("ta", "attach machine")` 是 `Some`（旧算法
        // 从 "attach" 词中间随便找到的 t/a，与任何词的开头都无关）。
        assert!(
            fuzzy_match("ta", "attach machine").is_none(),
            "首字符不落在词开头的子序列不该再命中"
        );
        // 分词前缀依然命中：查询是某个词的真实前缀。
        assert!(fuzzy_match("im", "ssh import").is_some());
        assert!(fuzzy_match("mac", "attach machine").is_some());
        // 首字符正好是某个词的开头、后续字符另起一个词首（m 落在
        // "machine" 开头）：照常命中。
        assert!(fuzzy_match("am", "attach machine").is_some());
    }

    fn indices(query: &str, text: &str) -> Option<Vec<usize>> {
        fuzzy_match(query, text).map(|(_, indices)| indices)
    }

    /// 浮层复审 1（严重）：L6 收紧后首字符只认空白 / `-_/:.` 之后的位置，
    /// 而中文标题不用空格分词——「窗格」「标签页」「工作树」落在标题中间，
    /// 整段失配，中文界面下核心词几乎搜不到命令。汉字与假名逐字算词首。
    #[test]
    fn fuzzy_match_treats_each_cjk_character_as_a_word_start() {
        assert_eq!(indices("窗格", "关闭窗格"), Some(vec![2, 3]));
        assert_eq!(indices("标签页", "新建标签页"), Some(vec![2, 3, 4]));
        assert_eq!(indices("工作树", "删除工作树检出"), Some(vec![2, 3, 4]));
        // 截屏 36：「机器」同时命中标题开头与标题末尾两处。
        assert_eq!(indices("机器", "机器"), Some(vec![0, 1]));
        assert_eq!(indices("机器", "从 SSH 配置导入机器"), Some(vec![10, 11]));
        assert_eq!(indices("ツール", "開発ツール"), Some(vec![2, 3, 4]));
    }

    /// 词首除了空白与标点之后，还包括 camelCase 边界、连写缩写后的新词与
    /// 汉字和拉丁字母之间的文字切换；同一个词中间的字母不算。
    #[test]
    fn fuzzy_match_word_starts_cover_camel_case_and_script_changes() {
        assert_eq!(indices("mtp", "MoveTabPrevious"), Some(vec![0, 4, 7]));
        assert_eq!(indices("ove", "MoveTabPrevious"), None);
        assert_eq!(indices("imp", "SSHImport"), Some(vec![3, 4, 5]));
        assert_eq!(indices("h", "SSH"), None);
        assert_eq!(indices("kimi", "账号Kimi"), Some(vec![2, 3, 4, 5]));
        assert_eq!(indices("12", "F12"), Some(vec![1, 2]));
        assert_eq!(indices("s", "(system)"), Some(vec![1]));
    }

    /// 浮层复审 2：首字符落在词首后，后续字符曾可以跳到任意位置——"mon"
    /// 由 "Move" 的 m、o 加上后面随便一个 n 拼成。后续字符必须紧接上一个
    /// 命中，或者另起一个词首。
    #[test]
    fn fuzzy_match_continuations_are_contiguous_or_word_starts() {
        assert_eq!(indices("mon", "Move tab left"), None);
        assert_eq!(indices("mon", "Import machines from SSH config"), None);
        assert_eq!(indices("mon", "Monitor & usage"), Some(vec![0, 1, 2]));
        // 首字母缩写、词前缀接词前缀照常命中。
        assert_eq!(indices("mtl", "Move tab left"), Some(vec![0, 5, 9]));
        assert_eq!(indices("movta", "Move tab left"), Some(vec![0, 1, 2, 5, 6]));
        // 贪心会先吃掉紧邻的 b、随后找不到 c；要回头改用词首的 b 才能命中。
        assert_eq!(indices("abc", "ab bc"), Some(vec![0, 3, 4]));
        // 连续命中比拆散到几个词首得分高。
        let (run, _) = fuzzy_match("tab", "tab").expect("run");
        let (acronym, _) = fuzzy_match("tab", "t a b").expect("acronym");
        assert!(run > acronym);
    }

    /// 查询里的空白表示「下一个字符另起一词」：多词查询不要求文本在同一
    /// 位置也有空格，只要求每段落在词首。
    #[test]
    fn fuzzy_match_query_whitespace_starts_a_new_word() {
        assert_eq!(
            indices("mon sys", "Monitor (system · accounts · settings)"),
            Some(vec![0, 1, 2, 9, 10, 11])
        );
        assert_eq!(indices("new tab", "New tab"), Some(vec![0, 1, 2, 4, 5, 6]));
        assert_eq!(indices("ne ab", "New tab"), None);
        assert_eq!(fuzzy_match("   ", "anything"), Some((0, Vec::new())));
    }
}
