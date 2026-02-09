const { invoke } = window.__TAURI__.core;
const { writeText } = window.__TAURI_PLUGIN_CLIPBOARD_MANAGER__;

// 轮询索引状态
async function updateIndexingStatus() {
  try {
    const isIndexing = await invoke('get_indexing_status');
    const statusEl = document.getElementById('indexing-status');
    if (isIndexing) {
      statusEl.textContent = '(正在更新外接盘索引...)';
      statusEl.style.color = '#ff9800';
    } else {
      statusEl.textContent = '(索引已就绪)';
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
let isComposing = false;

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

  try {
    const results = await invoke("search_files", { keyword, filterType: currentFilter });
    renderResults(results);
  } catch (error) {
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

function renderResults(results) {
  resultsContainer.innerHTML = '';
  
  if (results.length === 0) {
    resultsContainer.innerHTML = '<div class="no-results">未找到匹配项，请尝试其他关键字</div>';
    return;
  }

  results.forEach(result => {
    // 增加严格过滤，确保前端不渲染路径或名称为空的坏数据
    if (!result.name || !result.path || result.name.trim() === "" || result.path.trim() === "") {
        return;
    }

    const item = document.createElement('div');
    item.className = 'result-item';
    
    // 双击打开文件
    item.ondblclick = () => openFile(result.path);

    const icon = getFileIcon(result);

    item.innerHTML = `
      <div class="result-icon-box">${icon}</div>
      <div class="result-info">
        <span class="result-name">${result.name}</span>
        <span class="result-path">${result.path}</span>
      </div>
      <div class="result-actions">
        <button class="action-btn copy-btn" title="复制路径">复制</button>
        <button class="action-btn open-btn" title="直接打开">打开</button>
        <button class="action-btn folder-btn" title="打开所在位置">位置</button>
      </div>
    `;

    // 绑定事件，避免使用 innerHTML 中的 onclick 以提高性能和可靠性
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

    // 单击信息部分也可以直接打开文件/文件夹（提升体验）
    item.querySelector('.result-info').onclick = (e) => {
        openFile(result.path);
    };

    resultsContainer.appendChild(item);
  });
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
    clearTimeout(searchTimeout);
    searchTimeout = setTimeout(() => performSearch(), 300);
  });

  searchInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      if (isComposing) return; // 如果正在选词，回车不触发搜索
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

  // 初始加载显示最近文件
  performSearch();
});
