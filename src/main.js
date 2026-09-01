const { invoke } = window.__TAURI__.core;
const { writeText } = window.__TAURI_PLUGIN_CLIPBOARD_MANAGER__;

// 轮询索引状态
async function updateIndexingStatus() {
  try {
    const isIndexing = await invoke('get_indexing_status');
    const statusEl = document.getElementById('indexing-status');
    if (isIndexing) {
      statusEl.textContent = '🔄 索引构建中...已扫描常用目录，可先搜索';
      statusEl.style.color = '#ff9800';
    } else {
      statusEl.textContent = '✓ 索引已就绪';
      statusEl.style.color = '#4caf50';
    }
  } catch (e) {
    console.error('获取索引状态失败:', e);
  }
}

setInterval(updateIndexingStatus, 5000);
updateIndexingStatus();

let searchInput;
let resultsContainer;
let searchTimeout;
let currentFilter = 'all';
let lastSearchKeyword = '';
let lastResults = []; // 最近一次搜索结果（Enter 快速打开 Top-1 用）
let isComposing = false;
let searchReqId = 0; // 搜索请求序号：仅渲染最后一次请求的结果，丢弃叠加的旧请求

// P4e: 语法提示轮转（恰当时刻提醒用法，防"想不起来"）
const SYNTAX_HINTS = [
  '语法：*.pdf 只搜 PDF 文件',
  '语法：path:项目 按路径搜索',
  '语法：size:>100m 大于 100MB',
  '语法："完整短语" 精确匹配',
  '语法：ps / wx / wps 直搜软件',
  '回车 = 直接打开第一条结果',
];
let hintTimer = null;
let hintIdx = 0;
function showHint(text) {
  const el = document.getElementById('syntax-hint');
  if (el) el.textContent = text || '';
}
function startHintRotation() {
  stopHintRotation();
  hintTimer = setInterval(() => {
    hintIdx = (hintIdx + 1) % SYNTAX_HINTS.length;
    showHint(SYNTAX_HINTS[hintIdx]);
  }, 7000);
}
function stopHintRotation() {
  if (hintTimer) { clearInterval(hintTimer); hintTimer = null; }
}
function updateHintForInput(v) {
  const t = v.trim();
  if (t.startsWith('*.')) showHint('按扩展名搜索中：*.pdf 只搜 PDF · *.png 只搜图片');
  else if (t.startsWith('path:')) showHint('按路径搜索中：path:项目 匹配路径含"项目"的文件');
  else if (t.startsWith('size:')) showHint('按大小搜索中：size:>100m = 大于 100MB · size:<1g = 小于 1GB');
  else if (t.startsWith('"')) showHint('精确短语搜索中："完整短语" 要求连续完整匹配');
  else showHint(SYNTAX_HINTS[hintIdx]);
}

async function performSearch(force = false) {
  const keyword = searchInput.value.trim();
  
  // 如果关键词没变且不是强制刷新，则不重复搜索
  if (!force && keyword === lastSearchKeyword && keyword !== '') {
    return;
  }
  lastSearchKeyword = keyword;
  
  if (!keyword && currentFilter === 'all') {
    resultsContainer.innerHTML = '<div class="loading">正在获取最近修改的文件 (V5)...</div>';
  } else {
    resultsContainer.innerHTML = '<div class="loading">V5 引擎正在极速扫描...</div>';
  }

  const reqId = ++searchReqId; // 仅渲染最新一次请求的结果，丢弃叠加的旧请求
  try {
    const results = await invoke("search_files", { keyword, filterType: currentFilter });
    if (reqId !== searchReqId) return;
    lastResults = results;
    renderResults(results);
  } catch (error) {
    if (reqId !== searchReqId) return;
    console.error("搜索出错:", error);
    resultsContainer.innerHTML = `<div class="error">搜索失败: ${error}</div>`;
  }
}

function getFileIcon(result) {
  const path = result.path;
  const name = result.name;
  
  // macOS 特有的程序包（本质是目录）
  if (path.endsWith('.app')) return '🚀';
  
  const ext = name.split('.').pop().toLowerCase();
  
  // 如果没有扩展名，且不是隐藏文件，大概率是文件夹
  if (!name.includes('.') && !name.startsWith('.')) return '📂';
  
  const imageExts = ['jpg', 'jpeg', 'png', 'gif', 'webp', 'heic', 'svg'];
  const videoExts = ['mp4', 'mkv', 'mov', 'avi', 'wmv'];
  const docExts = ['pdf', 'docx', 'doc', 'ppt', 'pptx', 'xlsx', 'xls', 'txt', 'md', 'csv'];
  const appExts = ['dmg', 'pkg', 'exe', 'sh'];

  if (imageExts.includes(ext)) return '🖼️';
  if (videoExts.includes(ext)) return '🎬';
  if (docExts.includes(ext)) return '📄';
  if (appExts.includes(ext)) return '🚀';
  
  return '📄';
}

// P3a: 关键词匹配高亮（nucleo match_pos 为码点下标；顺带 HTML 转义防注入）
function highlightName(name, positions) {
  const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
  if (!positions || !positions.length) return esc(name);
  const chars = Array.from(name);
  const posSet = new Set(positions);
  let html = '';
  chars.forEach((c, i) => { html += posSet.has(i) ? `<mark>${esc(c)}</mark>` : esc(c); });
  return html;
}

// P4d: 按类型分组（图片/视频/文档/应用/文件夹/其他）
function groupResults(results) {
  const imageExts = ['jpg', 'jpeg', 'png', 'gif', 'webp', 'heic', 'svg', 'bmp', 'tiff', 'tif'];
  const videoExts = ['mp4', 'mov', 'avi', 'mkv', 'flv', 'wmv', 'm4v'];
  const docExts = ['pdf', 'txt', 'md', 'doc', 'docx', 'xls', 'xlsx', 'ppt', 'pptx', 'pages', 'numbers', 'keynote'];
  const groups = { image: [], video: [], doc: [], app: [], folder: [], other: [] };
  results.forEach(r => {
    if (!r.name || !r.path) return;
    const ext = (r.name.split('.').pop() || '').toLowerCase();
    if (r.path.endsWith('.app') || r.path.endsWith('.app/')) groups.app.push(r);
    else if (imageExts.includes(ext)) groups.image.push(r);
    else if (videoExts.includes(ext)) groups.video.push(r);
    else if (docExts.includes(ext)) groups.doc.push(r);
    else if (!r.name.includes('.')) groups.folder.push(r);
    else groups.other.push(r);
  });
  const order = [['image', '图片'], ['video', '视频'], ['doc', '文档'], ['app', '应用'], ['folder', '文件夹'], ['other', '其他']];
  return order.map(([k, label]) => ({ label, items: groups[k] })).filter(g => g.items.length > 0);
}

function renderResults(results) {
  resultsContainer.innerHTML = '';
  
  if (results.length === 0) {
    resultsContainer.innerHTML = '<div class="no-results">未找到匹配项。试试语法：<code>*.pdf</code> 扩展名 · <code>path:项目</code> 路径 · <code>size:>100m</code> 大小 · <code>"完整短语"</code> 精确</div>';
    return;
  }

  const imageExts = ['jpg', 'jpeg', 'png', 'gif', 'webp', 'heic', 'svg', 'bmp', 'tiff', 'tif'];
  let thumbQueue = [];

  const groups = groupResults(results);
  groups.forEach(group => {
    // 分组头（P4d）
    const header = document.createElement('div');
    header.className = 'group-header';
    header.textContent = `${group.label} (${group.items.length})`;
    resultsContainer.appendChild(header);

    group.items.forEach(result => {
      if (!result.name || !result.path || result.name.trim() === "" || result.path.trim() === "") {
          return;
      }

      const item = document.createElement('div');
      item.className = 'result-item';
      item.ondblclick = () => openFile(result.path);

      const ext = result.name.split('.').pop().toLowerCase();
      const isImage = imageExts.includes(ext);
      const icon = getFileIcon(result);

      item.innerHTML = `
        <div class="result-icon-box" data-thumb-path="${isImage ? encodeURIComponent(result.path) : ''}">${icon}</div>
        <div class="result-info">
          <span class="result-name">${highlightName(result.name, result.match_pos)}</span>
          <span class="result-path">${result.path}</span>
        </div>
        <div class="result-actions">
          <button class="action-btn copy-btn" title="复制路径">复制</button>
          <button class="action-btn open-btn" title="直接打开">打开</button>
          <button class="action-btn folder-btn" title="打开所在位置">位置</button>
        </div>
      `;

      // 绑定事件
      item.querySelector('.copy-btn').onclick = (e) => {
          e.stopPropagation();
          copyPath(result.path, e.target);
      };
      item.querySelector('.open-btn').onclick = (e) => {
          e.stopPropagation();
          openFile(result.path);
      };
      item.querySelector('.folder-btn').onclick = (e) => {
          e.stopPropagation();
          openFolder(result.path);
      };
      item.querySelector('.result-info').onclick = (e) => {
          openFile(result.path);
      };

      resultsContainer.appendChild(item);

      // 图片文件入队，后续批量加载缩略图
      if (isImage) {
        thumbQueue.push(result.path);
      }
    });
  });

  // 批量懒加载缩略图（最多同时 3 个，避免 qlmanage 风暴）
  if (thumbQueue.length > 0) {
    let idx = 0;
    const loadNext = () => {
      if (idx >= thumbQueue.length) return;
      const path = thumbQueue[idx++];
      const box = document.querySelector(`.result-icon-box[data-thumb-path="${encodeURIComponent(path)}"]`);
      if (!box) { loadNext(); return; }
      invoke('get_thumbnail', { path, size: 96 })
        .then(b64 => {
          if (b64) {
            box.innerHTML = `<img src="${b64}" class="thumb-img" alt="缩略图" style="width:48px;height:48px;border-radius:6px;object-fit:cover;">`;
          }
        })
        .catch(() => {})
        .finally(() => {
          setTimeout(loadNext, 100);
        });
    };
    // 启动 3 个并行加载
    loadNext();
    loadNext();
    loadNext();
  }
}

async function openFile(path) {
  try {
    console.log("正在打开:", path);
    await invoke("open_file", { path });
  } catch (error) {
    console.error("打开失败:", error);
    alert("无法打开: " + error);
  }
}

async function openFolder(path) {
  try {
    console.log("正在打开位置:", path);
    await invoke("open_folder", { path });
  } catch (error) {
    console.error("打开位置失败:", error);
    alert("无法打开位置: " + error);
  }
}

async function copyPath(path, btn) {
  try {
    // 调用后端增强的复制功能
    await invoke("copy_to_clipboard", { path });
    
    const originalText = btn.innerText;
    btn.innerText = "已复制";
    
    btn.classList.add('success');
    setTimeout(() => {
        btn.innerText = originalText;
        btn.classList.remove('success');
    }, 1500);
  } catch (error) {
    console.error("后端复制失败，尝试前端纯文本复制:", error);
    try {
        await writeText(path);
        const originalText = btn.innerText;
        btn.innerText = "已复制路径";
        btn.classList.add('success');
        setTimeout(() => {
            btn.innerText = originalText;
            btn.classList.remove('success');
        }, 1500);
    } catch (textError) {
        console.error("所有复制方式均失败:", textError);
    }
  }
}

// 暴露给全局以便 HTML 调用
window.copyPath = copyPath;
window.openFolder = openFolder;

window.addEventListener("DOMContentLoaded", () => {
  searchInput = document.querySelector("#search-input");
  resultsContainer = document.querySelector("#results");
  const tabs = document.querySelectorAll(".tab-btn");

  // 搜索输入监听
  searchInput.addEventListener("compositionstart", () => {
    isComposing = true;
  });

  searchInput.addEventListener("compositionend", () => {
    isComposing = false;
    // IME 输入结束后触发一次搜索
    clearTimeout(searchTimeout);
    searchTimeout = setTimeout(() => performSearch(), 300);
  });

  searchInput.addEventListener("input", () => {
    if (isComposing) return; // 正在输入拼音时不触发
    updateHintForInput(searchInput.value); // P4e: 前缀感知提示
    clearTimeout(searchTimeout);
    searchTimeout = setTimeout(() => performSearch(), 300);
  });

  searchInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      if (isComposing) return; // 如果正在选词，回车不触发搜索
      if (lastResults && lastResults.length > 0) {
        openFile(lastResults[0].path); // 回车快速打开 Top-1（Everything 风格）
        return;
      }
      clearTimeout(searchTimeout);
      performSearch(true); // 强制搜索
    }
  });

  // 标签切换监听
  tabs.forEach(tab => {
    tab.addEventListener("click", () => {
      tabs.forEach(t => t.classList.remove("active"));
      tab.classList.add("active");
      currentFilter = tab.dataset.type;
      performSearch();
    });
  });

  // 初始加载显示最近文件 + 语法提示轮转（P4e）
  startHintRotation();
  showHint(SYNTAX_HINTS[0]);
  performSearch();
});
