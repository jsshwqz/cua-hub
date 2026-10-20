#!/usr/bin/env bash
# 一键安装 Rust 工具链（装到 ~/.cargo + ~/.rustup，随 /home 持久化，VM 重置不丢）
set -euo pipefail
if command -v cargo >/dev/null 2>&1; then
  echo "cargo already installed: $(cargo --version)"
  exit 0
fi
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
export PATH="$HOME/.cargo/bin:$PATH"
rustup component add rustfmt clippy
echo "installed: $(rustc --version)"
