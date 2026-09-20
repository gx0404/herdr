use std::collections::HashSet;
use std::io;
use std::sync::{Mutex, OnceLock};

use crate::protocol::render_ansi;

static RECEIVED_KITTY_GRAPHICS_IDS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();

pub(super) fn write_encoded_frame_with_graphics(
    mut writer: impl io::Write,
    encoded: &[u8],
    graphics: &[u8],
) -> io::Result<()> {
    if graphics.is_empty() {
        return writer.write_all(encoded);
    }

    let insertion = render_ansi::final_sync_output_end(encoded).unwrap_or(encoded.len());

    writer.write_all(&encoded[..insertion])?;
    record_received_kitty_graphics(graphics);
    writer.write_all(b"\x1b7")?;
    writer.write_all(graphics)?;
    writer.write_all(b"\x1b8")?;
    writer.write_all(&encoded[insertion..])
}

/// 没有帧可搭车的图形清理：仍包在自己的同步块里，保证整批到达宿主。
pub(super) fn write_standalone_graphics(
    mut writer: impl io::Write,
    graphics: &[u8],
) -> io::Result<()> {
    if graphics.is_empty() {
        return Ok(());
    }
    record_received_kitty_graphics(graphics);
    writer.write_all(b"\x1b[?2026h\x1b7")?;
    writer.write_all(graphics)?;
    writer.write_all(b"\x1b8\x1b[?2026l")
}

/// 透传/直传的图形命令：只包同步块标记，不加 ESC7/ESC8（字节本身已带光标定位语义，
/// 直传命令自带 ESC7/ESC8），保证整批到达宿主而不改变既有光标行为。
pub(super) fn write_synchronized_graphics(
    mut writer: impl io::Write,
    graphics: &[u8],
) -> io::Result<()> {
    if graphics.is_empty() {
        return Ok(());
    }
    record_received_kitty_graphics(graphics);
    writer.write_all(b"\x1b[?2026h")?;
    writer.write_all(graphics)?;
    writer.write_all(b"\x1b[?2026l")
}

pub(super) fn contains_kitty_graphics_bytes(bytes: &[u8]) -> bool {
    bytes.windows(3).any(|window| window == b"\x1b_G")
}

pub(super) fn record_received_kitty_graphics(bytes: &[u8]) {
    let ids = kitty_graphics_image_ids(bytes);
    if ids.is_empty() {
        return;
    }
    let set = RECEIVED_KITTY_GRAPHICS_IDS.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut set) = set.lock() {
        set.extend(ids);
    }
}

pub(super) fn clear_received_kitty_graphics(mut writer: impl io::Write) -> io::Result<()> {
    let Some(set) = RECEIVED_KITTY_GRAPHICS_IDS.get() else {
        return Ok(());
    };
    let Ok(mut set) = set.lock() else {
        return Ok(());
    };
    for id in set.drain() {
        write!(writer, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\")?;
    }
    writer.flush()
}

pub(super) fn kitty_graphics_image_ids(bytes: &[u8]) -> Vec<u32> {
    let mut ids = Vec::new();
    let mut index = 0usize;
    while let Some(start) = find_subslice(&bytes[index..], b"\x1b_G") {
        let command_start = index + start + 3;
        let Some(end) = find_subslice(&bytes[command_start..], b"\x1b\\") else {
            break;
        };
        let command = &bytes[command_start..command_start + end];
        if let Some(id) = kitty_graphics_command_image_id(command) {
            ids.push(id);
        }
        index = command_start + end + 2;
    }
    ids
}

fn kitty_graphics_command_image_id(command: &[u8]) -> Option<u32> {
    let header_end = command
        .iter()
        .position(|byte| *byte == b';')
        .unwrap_or(command.len());
    for part in command[..header_end].split(|byte| *byte == b',') {
        let Some(value) = part.strip_prefix(b"i=") else {
            continue;
        };
        let text = std::str::from_utf8(value).ok()?;
        if let Ok(id) = text.parse::<u32>() {
            return Some(id);
        }
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELETE_IMAGE: &[u8] = b"\x1b_Ga=d,d=I,i=7,q=2;\x1b\\";

    #[test]
    fn standalone_graphics_are_wrapped_in_their_own_synchronized_block() {
        // 没有帧可搭车时，图形清理也必须整批到达宿主：?2026h … ?2026l 包裹。
        let mut output = Vec::new();
        write_standalone_graphics(&mut output, DELETE_IMAGE).unwrap();
        let expected = [
            b"\x1b[?2026h\x1b7".as_slice(),
            DELETE_IMAGE,
            b"\x1b8\x1b[?2026l",
        ]
        .concat();
        assert_eq!(output, expected);

        let mut empty = Vec::new();
        write_standalone_graphics(&mut empty, b"").unwrap();
        assert!(empty.is_empty(), "空图形不写任何字节");
    }

    #[test]
    fn synchronized_graphics_keep_their_own_cursor_semantics_inside_a_sync_block() {
        // 透传/直传：只加 ?2026h/?2026l，不加 ESC7/ESC8（直传命令自带）。
        let command = b"\x1b7\x1b[2;3H\x1b_Ga=T,t=f,i=9;cGF0aA==\x1b\\\x1b8";
        let mut output = Vec::new();
        write_synchronized_graphics(&mut output, command).unwrap();
        let expected = [b"\x1b[?2026h".as_slice(), command, b"\x1b[?2026l"].concat();
        assert_eq!(output, expected);

        let mut empty = Vec::new();
        write_synchronized_graphics(&mut empty, b"").unwrap();
        assert!(empty.is_empty(), "空图形不写任何字节");
    }

    #[test]
    fn frame_graphics_land_between_the_frame_sync_markers() {
        let encoded = b"\x1b[?2026h\x1b[?25l\x1b[1;1HX\x1b[1;2H\x1b[?25h\x1b[?2026l";
        let mut output = Vec::new();
        write_encoded_frame_with_graphics(&mut output, encoded, DELETE_IMAGE).unwrap();
        let output = String::from_utf8_lossy(&output).into_owned();
        let start = output.find("\x1b[?2026h").expect("sync start");
        let end = output.rfind("\x1b[?2026l").expect("sync end");
        let graphics = output.find("\x1b_G").expect("graphics");
        assert!(start < graphics && graphics < end, "{output:?}");
        assert_eq!(output.matches("\x1b[?2026h").count(), 1);
        assert_eq!(output.matches("\x1b[?2026l").count(), 1);
        // 图形紧跟最终光标状态之后、同步块结束之前：宿主一次呈现帧与图片。
        let cursor_shown = output.find("\x1b[?25h").expect("final cursor state");
        assert!(cursor_shown < graphics, "{output:?}");
        assert!(output.ends_with("\x1b8\x1b[?2026l"), "{output:?}");
        assert!(
            output[end..].len() == "\x1b[?2026l".len(),
            "块外 0 字节: {output:?}"
        );
    }
}
