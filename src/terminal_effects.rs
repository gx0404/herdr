use std::io::{self, Write};

const BELL_CHUNK: [u8; 64] = [b'\x07'; 64];

pub(crate) fn write_terminal_bells<W: Write>(writer: &mut W, count: u16) -> io::Result<()> {
    let full_chunks = usize::from(count) / BELL_CHUNK.len();
    let remainder = usize::from(count) % BELL_CHUNK.len();
    for _ in 0..full_chunks {
        writer.write_all(&BELL_CHUNK)?;
    }
    writer.write_all(&BELL_CHUNK[..remainder])?;
    writer.flush()
}

pub(crate) fn write_window_title<W: Write>(writer: &mut W, title: Option<&str>) -> io::Result<()> {
    let title = title.unwrap_or("herdr");
    let safe_title = title
        .chars()
        .filter(|ch| !matches!(*ch, '\u{1b}' | '\u{7}' | '\u{9c}'))
        .collect::<String>();
    write!(writer, "\x1b]0;{safe_title}\x07")?;
    writer.flush()
}

/// 把焦点 pane 的 cwd 以 OSC 7 写到宿主终端（WEZ-INT-02）：wezterm 等宿主据此让新建
/// 标签/分屏继承目录。URI 由 server 完成 hostname 与 percent 编码；`None` 表示当前没有
/// 可上送的 cwd，保持宿主已有值不动（OSC 7 没有「清除」语义）。
pub(crate) fn write_terminal_cwd<W: Write>(writer: &mut W, uri: Option<&str>) -> io::Result<()> {
    let Some(uri) = uri.filter(|uri| !uri.is_empty()) else {
        return Ok(());
    };
    write!(writer, "\x1b]7;{uri}\x1b\\")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_exact_terminal_bell_count() {
        let mut output = Vec::new();

        write_terminal_bells(&mut output, 130).unwrap();

        assert_eq!(output, vec![b'\x07'; 130]);
    }

    #[test]
    fn window_title_strips_terminators_and_defaults_to_herdr() {
        let mut output = Vec::new();
        write_window_title(&mut output, Some("herdr\x1b api\u{7}\u{9c}")).unwrap();
        assert_eq!(output, b"\x1b]0;herdr api\x07");

        output.clear();
        write_window_title(&mut output, None).unwrap();
        assert_eq!(output, b"\x1b]0;herdr\x07");
    }

    #[test]
    fn terminal_cwd_writes_osc7_with_st_and_none_writes_nothing() {
        let mut output = Vec::new();
        write_terminal_cwd(&mut output, Some("file://host/tmp%20x")).unwrap();
        assert_eq!(output, b"\x1b]7;file://host/tmp%20x\x1b\\");

        output.clear();
        write_terminal_cwd(&mut output, None).unwrap();
        write_terminal_cwd(&mut output, Some("")).unwrap();
        assert!(output.is_empty());
    }
}
