#!/usr/bin/env bash
# Phase 1 冒烟测试（在仓库根目录运行）：
#   A) dry-run 跑动作序列 —— 断言目标文件不产生（安全围栏有效）
#   B) autonomous 再跑一遍 —— 断言 xterm 里敲入的命令真的写出目标文件
#      （证明：截屏 + 键鼠注入 + 程序启动 整条链路闭环）
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

echo "== build =="
cargo build --workspace 2>&1 | tail -2

DISP=":99"
echo "== prepare Xvfb $DISP =="
if test ! -e /tmp/.X11-unix/X99; then
  Xvfb "$DISP" -screen 0 1280x800x24 &
  sleep 1
fi
export DISPLAY="$DISP"

if test -z "$(command -v xterm || true)"; then
  echo "== install xterm =="
  sudo apt-get update -qq
  sudo apt-get install -y -qq xterm xfonts-base >/dev/null
fi

CLI=./target/debug/cua-cli
SMOKE_FILE=/tmp/cua_smoke.txt
rm -f "$SMOKE_FILE"

echo "== phase A: dry-run must NOT create the file =="
"$CLI" exec --file examples/actions.json --mode dry-run --audit /tmp/cua_audit_dry.jsonl --shots /tmp/cua_shots >/dev/null
if test -e "$SMOKE_FILE"; then
  echo "SMOKE FAIL: dry-run created $SMOKE_FILE"
  exit 1
fi
echo "dry-run OK"

echo "== phase B: autonomous must create the file via real typing =="
"$CLI" exec --file examples/actions.json --mode autonomous --audit /tmp/cua_audit_auto.jsonl --shots /tmp/cua_shots
grep -q "cua-smoke-ok" "$SMOKE_FILE" || { echo "SMOKE FAIL: unexpected content in $SMOKE_FILE"; exit 1; }

echo "== screenshots =="
ls -la /tmp/cua_shots/
echo "SMOKE PASS"
