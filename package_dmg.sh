#!/usr/bin/env bash
# 星TAP 极速搜索 —— Release 打包 dmg 脚本（补 Tauri 不自动拷前端资源的坑）
# 用法：在极速搜索 项目根目录执行 ./package_dmg.sh
set -e

PROJ="$(cd "$(dirname "$0")" && pwd)"
APP="$PROJ/target/release/bundle/macos/星TAP 极速搜索.app"
RES="$APP/Contents/Resources"
DMG="$PROJ/target/release/bundle/dmg/星TAP极速搜索_v2.2.0.dmg"

if [ ! -d "$APP" ]; then
  echo "[package_dmg] 找不到构建产物: $APP"
  echo "请先运行 ./node_modules/.bin/tauri build"
  exit 1
fi

# 1) 拷贝前端资源（Tauri 不会自动拷 src/ 进 Contents/Resources）
mkdir -p "$RES"
cp -R "$PROJ/src/index.html" "$PROJ/src/main.js" "$PROJ/src/styles.css" "$PROJ/src/assets" "$RES/"
echo "[package_dmg] 前端资源已拷入 $RES"

# 2) 清理 bundle_dmg.sh 失败遗留的 rw.*.dmg 临时片
rm -f "$PROJ"/target/release/bundle/macos/rw.*.dmg 2>/dev/null || true

# 3) 手工 hdiutil 打包（bundle_dmg.sh 在本地必然失败，改用此路径）
STAGE="$(mktemp -d)"
cp -R "$APP" "$STAGE/"
hdiutil create -volname "星TAP 极速搜索" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"

echo "[package_dmg] 完成: $DMG"
ls -lh "$DMG"
