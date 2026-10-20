//! 剪贴板读写（arboard：Windows / Linux X11 / macOS 通用）。
//!
//! X11 的剪贴板内容由持有进程保管，且实例被丢弃后内容会很快失效
//! （裸 X 会话没有 clipboard manager 接管），因此实例必须常驻：
//! Runtime 持有一个，惰性初始化、失败只影响剪贴板工具本身。

use anyhow::{Context, Result};

/// 从常驻实例读取文本（剪贴板里是图片/文件时返回错误）。
pub fn get_text(cb: &mut arboard::Clipboard) -> Result<String> {
    cb.get_text().context("read clipboard text")
}

/// 通过常驻实例写入文本。
pub fn set_text(cb: &mut arboard::Clipboard, text: &str) -> Result<()> {
    cb.set_text(text.to_string())
        .context("write clipboard text")
}
