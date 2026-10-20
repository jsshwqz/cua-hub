#!/usr/bin/env bash
# VM 重置后一键恢复本项目：Rust 工具链 + 构建产物
# （桌面环境本身由 ~/bin/restore-remote-access.sh 恢复，不在本脚本范围）
set -euo pipefail
cd "$(dirname "$0")/.."
bash scripts/install-rust.sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --workspace
echo "restore OK: ./target/debug/cua-cli"
