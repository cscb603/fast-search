#!/bin/bash
# pack_mac.sh — 星TAP 极速搜索 Mac 出包加固脚本
# ============================================================================
# 四步铁律（确保分享到别的 Mac 后能正常打开，而非双击「没反应」）：
#   1) chmod -R 755                 → 否则其他用户/ExFAT 是 700 无读执行权，双击静默失败
#   2) codesign --force --deep --sign -  → ad-hoc 免费签名，否则 launchd 报 Code=163 拒开 GUI
#   3) xattr -cr                    → 清下载隔离属性，避免弹「已损坏」
#   4) 正确打 zip（UTF-8 中文名 + 禁 AppleDouble 垃圾 ._*）→ 分享物带正确权限
# ----------------------------------------------------------------------------
# 用法：
#   ./pack_mac.sh                 自动定位 .app（优先 tauri build 产物，否则 dist 里最新）并加固打包
#   ./pack_mac.sh --build         先 `npm run tauri build` 再加固打包
#   ./pack_mac.sh --app <路径>    加固指定 .app（如 dist/星TAP极速搜索_v2.0.3_Mac/星TAP 极速搜索.app）
#   ./pack_mac.sh --deploy <目录> 打包后顺便拷贝到外置盘（自动处理 ExFAT 坑，只发 zip 不发文件夹）
# ============================================================================
set -euo pipefail

APP_NAME="星TAP 极速搜索"
PROD_APP="src-tauri/target/release/bundle/macos/${APP_NAME}.app"
OUT_DIR="dist"
DO_BUILD=0
SRC_APP=""
DEPLOY_DIR=""

# ---- 解析参数 ----
i=1
while [[ $i -le $# ]]; do
  case "${!i}" in
    --build)   DO_BUILD=1 ;;
    --app)     i=$((i+1)); SRC_APP="${!i}" ;;
    --deploy)  i=$((i+1)); DEPLOY_DIR="${!i}" ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "✗ 未知参数: ${!i}（用 -h 看用法）"; exit 1 ;;
  esac
  i=$((i+1))
done

# ---- 定位 .app ----
if [[ -n "$SRC_APP" ]]; then
  APP="$SRC_APP"
elif [[ $DO_BUILD -eq 1 ]]; then
  echo ">>> [build] npm run tauri build"
  npm run tauri build
  APP="$PROD_APP"
else
  if [[ -d "$PROD_APP" ]]; then
    APP="$PROD_APP"
  else
    # 在 dist 里找最新的 .app
    APP=$(find dist -maxdepth 2 -name "${APP_NAME}.app" -type d 2>/dev/null | xargs ls -dt 2>/dev/null | head -1)
    if [[ -z "$APP" ]]; then
      echo "✗ 找不到 .app。请先 ./pack_mac.sh --build，或用 --app 指定已出包的 .app 路径。"
      exit 1
    fi
  fi
fi
[[ -d "$APP" ]] || { echo "✗ .app 不存在: $APP"; exit 1; }
echo ">>> 目标 app: $APP"

# ---- 四步加固 ----
echo ">>> [0/4] 清 AppleDouble 垃圾 ._*（ExFAT 拷入会自动生成，会搞挂 codesign --deep）"
find "$APP" -name '._*' -delete 2>/dev/null || true

echo ">>> [1/4] chmod -R 755（修复其他用户/ExFAT 的 700 无权限坑）"
chmod -R 755 "$APP"

echo ">>> [2/4] xattr -cr（清隔离属性，防『已损坏』；必须在 codesign 之前，否则会清掉签名写入的 com.apple.cs.* xattr 破坏 seal）"
xattr -cr "$APP"

echo ">>> [3/4] ad-hoc codesign（--force --deep --sign -，免费签名防 Code=163）"
codesign --force --deep --sign - "$APP"

# ---- 版本号 & 输出 zip ----
VER=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist" 2>/dev/null || echo "dev")
mkdir -p "$OUT_DIR"
ZIP_NAME="${APP_NAME}_v${VER}_Mac.zip"
ZIP_ABS="$(cd "$OUT_DIR" && pwd)/$ZIP_NAME"

# 准备说明文件（同级复用或内置生成）
NOTES="$OUT_DIR/首次打开必看.txt"
if [[ ! -f "$NOTES" ]]; then
  cat > "$NOTES" <<'EOF'
星TAP 极速搜索 · 首次打开必看
================================
本软件是 ad-hoc 免费测试签名版，仅限小圈子朋友内部使用（非正式 Developer ID 公证版）。

1. 把「星TAP 极速搜索.app」拖进「应用程序」文件夹。
2. 第一次打开：右键点击 app →「打开」→ 弹窗点「打开」。
   （macOS 会拦截「未验证开发者」，右键打开一次后以后双击即可。）
3. 关于隔离属性（com.apple.quarantine），分两种来源：
   • 通过「星TAP ARLink」点对直传给你：文件是应用直接落盘，系统不会打隔离标记，
     一般双击就能走「右键打开」流程，不会报「已损坏」。
   • 通过浏览器 / 邮件 / AirDrop 下载：系统会给文件打隔离属性，
     解压后可能提示「已损坏，应移到废纸篓」。终端执行（app 名带空格要加引号）：
       xattr -dr com.apple.quarantine "星TAP 极速搜索.app"
     再重试第 2 步。（-dr 递归删隔离属性；等价于更彻底的 xattr -cr）
4. 搜索基于系统 Spotlight 索引；外接磁盘需先被 Spotlight 索引过才能搜到。
   （本软件另有后台 MCP 版，无需 GUI，详见 极速搜索后台MCP-计划.md）
EOF
fi

echo ">>> [4/4] 打 zip: ${ZIP_ABS}（Python zipfile 保证中文名 UTF-8 bit11，避免 zip 命令乱码）"
rm -f "${ZIP_ABS}"
APP_BASE="$(basename "$APP")"
python3 - "$APP" "$NOTES" "${ZIP_ABS}" "$APP_BASE" <<'PY'
import zipfile, os, sys
app, notes, zippath, app_base = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
z = zipfile.ZipFile(zippath, 'w', zipfile.ZIP_DEFLATED)
for dp,_,fns in os.walk(app):
    for f in fns:
        full = os.path.join(dp, f)
        arc = app_base + '/' + os.path.relpath(full, app)
        z.write(full, arc)
if os.path.exists(notes):
    z.write(notes, os.path.basename(notes))
z.close()
print("repacked ok")
PY

# ---- 自检 ----
echo ">>> 自检"
if codesign --verify --deep "$APP" >/dev/null 2>&1; then
  echo "  ✓ 签名有效 (codesign --verify --deep 通过)"
else
  echo "  ✗ 签名校验失败"; exit 1
fi
PERM=$(stat -f '%Lp' "$APP/Contents/MacOS" 2>/dev/null || echo "???")
echo "  ✓ MacOS 目录权限: ${PERM}（应为 755）"
TMPV=$(mktemp -d)
( cd "$TMPV" && unzip -q "${ZIP_ABS}" )
ZAPP="$TMPV/${APP_NAME}.app"
if codesign --verify --deep "$ZAPP" >/dev/null 2>&1; then
  echo "  ✓ zip 内 app 签名有效"
else
  echo "  ✗ zip 内 app 签名失效"; rm -rf "$TMPV"; exit 1
fi
GARBAGE=$(find "$TMPV" -name '._*' | wc -l | tr -d ' ')
echo "  ✓ zip 内 AppleDouble 垃圾数: ${GARBAGE}（应为 0）"
rm -rf "$TMPV"

echo ""
echo "✅ 完成: ${ZIP_ABS} ($(du -h "${ZIP_ABS}" | cut -f1))"
echo "   发给朋友：解压 → 右键打开即可（勿直接发文件夹，ExFAT 存不住权限）。"

# ---- 可选：部署到外置盘 ----
if [[ -n "${DEPLOY_DIR}" ]]; then
  echo ">>> [deploy] 拷贝到 ${DEPLOY_DIR}（ExFAT 安全模式）"
  mkdir -p "${DEPLOY_DIR}"
  cp "${ZIP_ABS}" "${DEPLOY_DIR}/"
  find "${DEPLOY_DIR}" -name '._*' -delete 2>/dev/null || true
  echo "  ✓ 已部署 zip 到 ${DEPLOY_DIR}（只发 zip，勿直接发文件夹）"
fi
