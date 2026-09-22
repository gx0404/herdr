//! 树：通用行条目 `TreeEntry<K>` 与行首前缀（缩进引导线 + 连接线 + 折叠开关）。
//! 泛型于 `K`，不认识客户端类型；领域层（如 `client/shell/agent_tree.rs`）在视图
//! 计算阶段按前序构造条目并缓存，渲染时逐行调 [`render_tree_prefix`]。
//!
//! 前缀版式：每层 2 列。第 1..depth-1 层画祖先的引导线（`│ ` 或空白），第
//! depth 层画本节点的连接线（`├─` / `└─`），随后 1 列折叠开关（`▾` / `▸`；叶子
//! 节点接一段 `─` 把连接线延到文字前）与 1 列间隔。根（depth 0）没有连接线：
//!
//! ```text
//! ▾ machine
//! ├─▾ workspace
//! │ ├── tab
//! │ └── tab
//! └─▸ workspace
//!   └── tab
//! ```
//!
//! 引导线从父节点的开关格垂下，连接线正好接在它下面。

#![allow(dead_code)] // seam-stub(agent-panel)：波 2 面板车道接入 Agents 树后删除

use ratatui::{buffer::Buffer, layout::Rect, style::Style};

use super::put_str;

/// `last_child_mask` 能表达的最深层级；更深的节点按此饱和（前缀宽度同样封顶）。
pub(crate) const MAX_TREE_DEPTH: u8 = 63;

/// 每层缩进的列数。
const INDENT: u16 = 2;

/// 一行树条目。`last_child_mask` 的 bit d（1 ≤ d ≤ depth）= 本节点在第 d 层的
/// 祖先（d == depth 时就是本节点自己）是否为其父节点的末子；bit 0 不用（根不画
/// 连接线）。按前序排好的条目可用 [`fill_last_child_masks`] 一次算出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeEntry<K> {
    pub depth: u8,
    pub key: String,
    pub kind: K,
    pub last_child_mask: u64,
    pub has_children: bool,
    pub collapsed: bool,
}

struct Glyphs {
    guide: &'static str,
    blank: &'static str,
    branch: &'static str,
    last: &'static str,
    leaf: &'static str,
    expanded: &'static str,
    collapsed: &'static str,
}

const UNICODE: Glyphs = Glyphs {
    guide: "│ ",
    blank: "  ",
    branch: "├─",
    last: "└─",
    leaf: "─",
    expanded: "▾",
    collapsed: "▸",
};

const ASCII: Glyphs = Glyphs {
    guide: "| ",
    blank: "  ",
    branch: "|-",
    last: "`-",
    leaf: "-",
    expanded: "v",
    collapsed: ">",
};

fn saturated(depth: u8) -> u8 {
    depth.min(MAX_TREE_DEPTH)
}

/// 前缀占多少列（只看深度，不分配），供行宽预算：`2 × depth + 2`，深度按
/// [`MAX_TREE_DEPTH`] 饱和。
pub(crate) const fn tree_prefix_width(depth: u8) -> u16 {
    let depth = if depth > MAX_TREE_DEPTH {
        MAX_TREE_DEPTH
    } else {
        depth
    };
    depth as u16 * INDENT + 2
}

/// 按前序（父在子前、兄弟相邻）排好的条目算出每条的 `last_child_mask`，原地写回，
/// 不分配：倒序一遍求「本节点是否末子」，正序一遍把祖先的结论逐层继承下来。
/// 深度跳级（子比父深 2 层以上）按缺失的中间层「是末子」处理，只画空白。
pub(crate) fn fill_last_child_masks<K>(entries: &mut [TreeEntry<K>]) {
    // 倒序：`later` 的 bit d = 在当前父作用域里，后面还有第 d 层的兄弟。
    let mut later = 0u64;
    for entry in entries.iter_mut().rev() {
        let depth = u32::from(saturated(entry.depth));
        let bit = 1u64 << depth;
        let is_last = later & bit == 0;
        entry.last_child_mask = u64::from(is_last);
        // 更深层的记录属于后面兄弟的子树，与本节点之前的兄弟无关。
        later &= low_bits(depth + 1);
        later |= bit;
    }
    // 正序：`ancestors` 的 bit d = 当前路径上第 d 层节点是否末子；缺失的中间层
    // 按「是末子」补齐。
    let mut ancestors = u64::MAX;
    for entry in entries.iter_mut() {
        let depth = u32::from(saturated(entry.depth));
        let bit = 1u64 << depth;
        let is_last = entry.last_child_mask == 1;
        ancestors = (ancestors & low_bits(depth)) | !low_bits(depth + 1);
        if is_last {
            ancestors |= bit;
        } else {
            ancestors &= !bit;
        }
        entry.last_child_mask = ancestors & low_bits(depth + 1) & !1;
    }
}

/// 低 `count` 位全 1（`count` ≥ 64 时为全 1）。
fn low_bits(count: u32) -> u64 {
    if count >= 64 {
        u64::MAX
    } else {
        (1u64 << count) - 1
    }
}

/// 在 `(x, y)` 起画前缀，最多占 `max_width` 列，逐格写、不分配。`style` 用于
/// 引导线、连接线与开关；开关要换色时，调用方对返回的命中区 `set_style` 即可。
/// 返回 `(实际占用列数, 折叠开关命中区)`：命中区覆盖开关与其后的间隔共 2 列
/// （好点），无子节点或开关被裁掉时为空 `Rect`。
pub(crate) fn render_tree_prefix(
    buffer: &mut Buffer,
    x: u16,
    y: u16,
    max_width: u16,
    depth: u8,
    last_child_mask: u64,
    has_children: bool,
    collapsed: bool,
    ascii: bool,
    style: Style,
) -> (u16, Rect) {
    let glyphs = if ascii { &ASCII } else { &UNICODE };
    let depth = saturated(depth);
    let mut cursor = Cursor {
        x,
        y,
        max_width,
        used: 0,
    };

    for level in 1..=depth {
        let is_last = last_child_mask & (1u64 << level) != 0;
        let text = match (level == depth, is_last) {
            (false, false) => glyphs.guide,
            (false, true) => glyphs.blank,
            (true, false) => glyphs.branch,
            (true, true) => glyphs.last,
        };
        if !cursor.put(buffer, text, INDENT, style) {
            return (cursor.used, Rect::default());
        }
    }

    let toggle = if has_children {
        if collapsed {
            glyphs.collapsed
        } else {
            glyphs.expanded
        }
    } else if depth > 0 {
        glyphs.leaf
    } else {
        " "
    };
    let toggle_x = x.saturating_add(cursor.used);
    if !cursor.put(buffer, toggle, 1, style) {
        return (cursor.used, Rect::default());
    }
    let gap = cursor.put(buffer, " ", 1, style);
    let hit = if has_children {
        Rect::new(toggle_x, y, 1 + u16::from(gap), 1)
    } else {
        Rect::default()
    };
    (cursor.used, hit)
}

/// 前缀的写入游标：整段写，放不下的一段（一层缩进、开关或间隔）整段不画，不画
/// 半截连接线。
struct Cursor {
    x: u16,
    y: u16,
    max_width: u16,
    used: u16,
}

impl Cursor {
    fn put(&mut self, buffer: &mut Buffer, text: &str, width: u16, style: Style) -> bool {
        if self.used + width > self.max_width {
            return false;
        }
        put_str(
            buffer,
            self.x.saturating_add(self.used),
            self.y,
            width,
            text,
            style,
        );
        self.used += width;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::row_text;
    use super::*;
    use ratatui::style::Color;

    fn entry(depth: u8, has_children: bool) -> TreeEntry<()> {
        TreeEntry {
            depth,
            key: String::new(),
            kind: (),
            last_child_mask: 0,
            has_children,
            collapsed: false,
        }
    }

    /// 把条目逐行画成文本（前缀 + 占位标签），方便整棵树对照。
    fn draw(entries: &[TreeEntry<()>], ascii: bool) -> Vec<String> {
        let width = 16;
        let area = Rect::new(0, 0, width, entries.len() as u16);
        let mut buffer = Buffer::empty(area);
        for (row, item) in entries.iter().enumerate() {
            let y = row as u16;
            let (used, _) = render_tree_prefix(
                &mut buffer,
                0,
                y,
                width,
                item.depth,
                item.last_child_mask,
                item.has_children,
                item.collapsed,
                ascii,
                Style::default(),
            );
            assert_eq!(used, tree_prefix_width(item.depth), "第 {row} 行");
            put_str(&mut buffer, used, y, width - used, "x", Style::default());
        }
        (0..entries.len())
            .map(|row| row_text(&buffer, row as u16).trim_end().to_owned())
            .collect()
    }

    fn sample_tree() -> Vec<TreeEntry<()>> {
        let mut entries = vec![
            entry(0, true),
            entry(1, true),
            entry(2, false),
            entry(2, false),
            entry(1, true),
            entry(2, false),
            entry(0, false),
        ];
        entries[4].collapsed = true;
        fill_last_child_masks(&mut entries);
        entries
    }

    #[test]
    fn masks_and_prefixes_draw_a_connected_tree() {
        let entries = sample_tree();
        let masks: Vec<u64> = entries.iter().map(|e| e.last_child_mask).collect();
        // 第 1 层第一个 workspace 不是末子（bit1=0）；其下末个 tab 的 bit2=1。
        assert_eq!(masks, vec![0, 0b000, 0b000, 0b100, 0b010, 0b110, 0]);
        assert_eq!(
            draw(&entries, false),
            vec![
                "▾ x",
                "├─▾ x",
                "│ ├── x",
                "│ └── x",
                "└─▸ x",
                "  └── x",
                "  x",
            ]
        );
    }

    #[test]
    fn ascii_prefixes_degrade_every_glyph() {
        assert_eq!(
            draw(&sample_tree(), true),
            vec!["v x", "|-v x", "| |-- x", "| `-- x", "`-> x", "  `-- x", "  x"]
        );
    }

    #[test]
    fn toggle_hit_rect_covers_the_toggle_and_gap_only_for_parents() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        let (used, hit) = render_tree_prefix(
            &mut buffer,
            2,
            0,
            10,
            1,
            0b10,
            true,
            false,
            false,
            Style::default().fg(Color::Red),
        );
        assert_eq!(used, 4);
        assert_eq!(hit, Rect::new(4, 0, 2, 1));
        assert_eq!(buffer[(4, 0)].symbol(), "▾");
        assert_eq!(buffer[(2, 0)].style().fg, Some(Color::Red));
        let (_, hit) = render_tree_prefix(
            &mut buffer,
            0,
            0,
            10,
            1,
            0,
            false,
            false,
            false,
            Style::default(),
        );
        assert_eq!(hit, Rect::default(), "叶子没有开关");
    }

    #[test]
    fn narrow_widths_clip_whole_levels_and_drop_the_toggle() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        // 深度 3 需要 8 列；只给 5 列：两层引导线 4 列，连接线整段放不下就不画。
        let (used, hit) = render_tree_prefix(
            &mut buffer,
            0,
            0,
            5,
            3,
            0,
            true,
            true,
            false,
            Style::default(),
        );
        assert_eq!(used, 4);
        assert_eq!(row_text(&buffer, 0), "│ │     ");
        assert_eq!(hit, Rect::default(), "连接线都没画完，开关被裁掉");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let (used, hit) = render_tree_prefix(
            &mut buffer,
            0,
            0,
            7,
            3,
            0,
            true,
            true,
            false,
            Style::default(),
        );
        assert_eq!(used, 7);
        assert_eq!(row_text(&buffer, 0), "│ │ ├─▸ ");
        assert_eq!(hit, Rect::new(6, 0, 1, 1), "间隔被裁掉时命中区只剩开关");
        let (used, hit) = render_tree_prefix(
            &mut buffer,
            0,
            0,
            0,
            3,
            0,
            true,
            true,
            false,
            Style::default(),
        );
        assert_eq!((used, hit), (0, Rect::default()));
    }

    #[test]
    fn depth_saturates_at_the_mask_width() {
        assert_eq!(tree_prefix_width(0), 2);
        assert_eq!(tree_prefix_width(3), 8);
        assert_eq!(tree_prefix_width(MAX_TREE_DEPTH), 128);
        assert_eq!(tree_prefix_width(200), 128);
        let mut entries = vec![entry(0, true), entry(200, false), entry(200, false)];
        fill_last_child_masks(&mut entries);
        assert_eq!(entries[1].last_child_mask & (1 << 63), 0);
        assert_ne!(entries[2].last_child_mask & (1 << 63), 0);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let (used, _) = render_tree_prefix(
            &mut buffer,
            0,
            0,
            4,
            200,
            u64::MAX,
            false,
            false,
            false,
            Style::default(),
        );
        assert_eq!(used, 4, "超深节点只按可用宽度画");
        assert_eq!(row_text(&buffer, 0), "    ", "祖先全是末子 → 全空白");
    }

    #[test]
    fn skipped_levels_render_as_blank_and_empty_input_is_fine() {
        let mut entries = vec![entry(0, true), entry(2, false), entry(0, false)];
        fill_last_child_masks(&mut entries);
        assert_eq!(draw(&entries, false), vec!["▾ x", "  └── x", "  x"]);
        let mut none: Vec<TreeEntry<()>> = Vec::new();
        fill_last_child_masks(&mut none);
    }
}
