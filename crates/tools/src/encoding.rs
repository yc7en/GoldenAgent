//! 命令输出的字符解码。
//!
//! Windows 上同一条命令的输出可能落在三种编码里，而且**一次调用里可能混着出现**：
//!
//! - 我们主动 `chcp 65001` 之后，多数程序写 UTF-8（`echo`、带通配符的 `dir`、git 之类）；
//! - cmd 自己的老代码路径仍按 ANSI 代码页写字节（最典型的是 `dir /b` 不带参数，
//!   在中文 Windows 上吐出 GBK）；
//! - 少数程序（`cmd /U`、部分包装脚本）直接吐 UTF-16LE。
//!
//! 所以不能一句 `from_utf8_lossy` 了事 —— 那会把 GBK 的文件名变成一串 `�`，
//! 模型看到的是"文件名是乱码"，接下来所有基于它的路径都会拼错，
//! 而错误信息里只有乱码，用户根本无从判断哪儿出了问题。
//!
//! 判定顺序：BOM → 无 BOM 的 UTF-16 → 严格 UTF-8 → 逐行回落系统代码页。
//! 逐行是必要的：`dir /b & git status` 这种混合输出，两段编码不同，
//! 整段一起解只能有一半是对的。

use std::borrow::Cow;

/// 解码一段命令输出。
///
/// 能用 UTF-8 解释就直接借用，不额外拷贝；否则返回解码后的新串。
pub fn decode(bytes: &[u8]) -> Cow<'_, str> {
    if bytes.is_empty() {
        return Cow::Borrowed("");
    }

    if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
        // 有 BOM 就不用猜了，剩下的按 UTF-8 读；坏字节宁可替换也不整段丢弃。
        return Cow::Owned(String::from_utf8_lossy(rest).into_owned());
    }

    if let Some(text) = decode_utf16(bytes) {
        return Cow::Owned(text);
    }

    match std::str::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => Cow::Owned(decode_lines(bytes)),
    }
}

/// 按行回落解码：能当 UTF-8 读的行照读，读不了的行交给系统代码页。
fn decode_lines(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        match std::str::from_utf8(line) {
            Ok(text) => out.push_str(text),
            Err(_) => out.push_str(&decode_legacy(line)),
        }
    }
    out
}

/// UTF-16：先认 BOM，再认"ASCII 主导的无 BOM UTF-16LE"。
fn decode_utf16(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(b"\xFF\xFE") {
        return utf16_to_string(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(b"\xFE\xFF") {
        return utf16_to_string(rest, false);
    }
    // `wsl.exe` 一类的输出没有 BOM。判据放在这里而不是 UTF-8 之前：
    // UTF-16LE 的 ASCII 内容是"字符 + 0x00"，严格 UTF-8 完全能解（NUL 是合法字节），
    // 不先拦下来就会拿到一串夹着 \0 的字符串。
    if looks_like_utf16le(bytes) {
        return utf16_to_string(bytes, true);
    }
    None
}

fn utf16_to_string(bytes: &[u8], little_endian: bool) -> Option<String> {
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    String::from_utf16(&units).ok()
}

/// 前 512 字节里"高字节为 0、低字节非 0"的 16 位单元过半数，就当作 UTF-16LE。
///
/// 只看开头：一个几十 KB 的文本输出后面跟一段二进制附件也是可能的，
/// 通篇统计反而会被那段二进制带偏。GBK / UTF-8 的双字节字符第二个字节
/// 都不可能是 0x00，所以这个判据不会误伤它们。
fn looks_like_utf16le(bytes: &[u8]) -> bool {
    if bytes.len() < 4 || bytes.len() % 2 != 0 {
        return false;
    }
    let head = &bytes[..bytes.len().min(512)];
    let pairs = head.len() / 2;
    let ascii_units = head
        .chunks_exact(2)
        .filter(|pair| pair[0] != 0 && pair[1] == 0)
        .count();
    ascii_units * 2 >= pairs
}

/// 交给系统代码页。先严格、再宽松：严格模式能识别"这条代码页也解不了"，
/// 宽松模式至少不会把整行丢掉。
fn decode_legacy(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    #[cfg(windows)]
    {
        for strict in [true, false] {
            for cp in code_pages() {
                if let Some(text) = decode_with_code_page(bytes, *cp, strict) {
                    return text;
                }
            }
        }
    }

    String::from_utf8_lossy(bytes).into_owned()
}

/// 要试的代码页：OEM 在前，ANSI 在后。
///
/// 控制台程序默认写 OEM（中文 Windows 上 936），GUI 程序默认写 ANSI（1252 之类），
/// 顺序反了就会把西欧系统的 cp850 输出当成 cp1252 解。中文系统上两者都是 936，
/// 这时只试一次。
#[cfg(windows)]
fn code_pages() -> &'static [u32] {
    static PAGES: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    PAGES.get_or_init(|| {
        // SAFETY: 两个函数都不接参数、不写内存，只回一个 UINT 代码页。
        let oem = unsafe { win32::GetOEMCP() };
        let ansi = unsafe { win32::GetACP() };
        if oem == ansi {
            vec![oem]
        } else {
            vec![oem, ansi]
        }
    })
}

#[cfg(windows)]
fn decode_with_code_page(bytes: &[u8], code_page: u32, strict: bool) -> Option<String> {
    use std::os::raw::c_int;

    let src_len = c_int::try_from(bytes.len()).ok()?;
    let flags = if strict { win32::MB_ERR_INVALID_CHARS } else { 0 };

    // SAFETY: 入参全来自本函数的 `bytes`，长度按 c_int 原样传入。
    // 先用空目标问需要多少 UTF-16 单元，再按这个尺寸分配并写第二次；
    // 两次之间没有别的线程能改动 `bytes`（它是 `&[u8]`）。
    unsafe {
        let wide_len = win32::MultiByteToWideChar(
            code_page,
            flags,
            bytes.as_ptr().cast(),
            src_len,
            std::ptr::null_mut(),
            0,
        );
        if wide_len <= 0 {
            return None;
        }

        let mut buf = vec![0u16; wide_len as usize];
        let written = win32::MultiByteToWideChar(
            code_page,
            flags,
            bytes.as_ptr().cast(),
            src_len,
            buf.as_mut_ptr(),
            wide_len,
        );
        if written <= 0 {
            return None;
        }
        buf.truncate(written as usize);
        String::from_utf16(&buf).ok()
    }
}

#[cfg(windows)]
mod win32 {
    use std::os::raw::{c_char, c_int, c_uint};

    /// 遇到非法字节就失败，而不是静默替换成 `?`。
    pub const MB_ERR_INVALID_CHARS: c_uint = 0x0000_0008;

    // 手写最小声明而不是引入 windows-sys：只用到三个函数，
    // 十几个常量/类型的依赖换不来什么，还要多一层版本跟着 Tauri 走。
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetACP() -> c_uint;
        pub fn GetOEMCP() -> c_uint;
        pub fn MultiByteToWideChar(
            code_page: c_uint,
            flags: c_uint,
            src: *const c_char,
            src_len: c_int,
            dst: *mut u16,
            dst_len: c_int,
        ) -> c_int;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_utf8_is_borrowed_untouched() {
        let text = "退出码 0 · 22ms\n中文文件.md\n";
        assert_eq!(decode(text.as_bytes()), text);
    }

    #[test]
    fn empty_output_stays_empty() {
        assert_eq!(decode(b""), "");
    }

    #[test]
    fn utf8_bom_is_stripped() {
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend_from_slice("你好".as_bytes());
        assert_eq!(decode(&bytes), "你好");
    }

    #[test]
    fn utf16le_with_bom_is_read() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "中文 abc".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode(&bytes), "中文 abc");
    }

    #[test]
    fn utf16le_without_bom_is_detected() {
        let bytes: Vec<u8> = "hello\r\nworld".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode(&bytes), "hello\r\nworld");
    }

    /// 这条是本轮 bug 的现场：`dir /b` 在中文 Windows 上吐 GBK 字节，
    /// 按 UTF-8 解会得到"一串问号菱形"。
    #[cfg(windows)]
    #[test]
    fn gbk_bytes_decode_to_chinese() {
        // "中文文件" 的 GBK 编码
        let bytes = [0xD6, 0xD0, 0xCE, 0xC4, 0xCE, 0xC4, 0xBC, 0xFE];
        assert_eq!(decode(&bytes), "中文文件");
    }

    /// 一半 UTF-8、一半 GBK 的输出：整段解不出来，逐行可以各归各位。
    #[cfg(windows)]
    #[test]
    fn mixed_encoding_lines_are_decoded_separately() {
        let mut bytes = "中文文件.md\r\n".as_bytes().to_vec();
        bytes.extend_from_slice(&[0xD6, 0xD0, 0xCE, 0xC4, 0x0D, 0x0A]);
        assert_eq!(decode(&bytes), "中文文件.md\r\n中文\r\n");
    }
}
