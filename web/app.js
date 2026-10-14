// Единый frontend PDA: Charts + Bits (uPlot), статус, live-WS, модальные диалоги.
// Логика графиков: Charts + Bits (uPlot), live-трансляция, курсоры X1/X2/ΔT.

// ---------------------------------------------------------------- API helpers
async function apiGet(path) {
    const res = await fetch(path);
    if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
    return res.json();
}
async function apiSend(method, path, body) {
    const res = await fetch(path, {
        method,
        headers: { 'Content-Type': 'application/json' },
        body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await res.text();
    let data = null;
    try { data = text ? JSON.parse(text) : null; } catch (e) { /* не JSON — текст ошибки */ }
    if (!res.ok) throw new Error((data && (data.error || data.detail)) || text || `${res.status}`);
    return data;
}
const apiPost = (p, b) => apiSend('POST', p, b);
const apiPut = (p, b) => apiSend('PUT', p, b);
const apiDel = (p) => apiSend('DELETE', p);

let allTags = [];
let driverTypes = [];      // /api/drivers/info -> types
let bitsByteSwap = localStorage.getItem('pda_bits_byteswap') === '1';
let mainChart = null;
let chartWindows = [];
let activeWindowId = null;
let bitsChart = null;
// Курсоры X1/X2 независимы для каждого графика: живут в win.cursors
// (окна Charts) и bitsCursors (вкладка Bits). Панель в шапке показывает
// курсоры активного графика.
function makeCursors() { return { x1: null, x2: null }; }
let bitsCursors = makeCursors();
let chartsPaused = false;
let bitsPaused = false;
let userZoomedCharts = false;
let userZoomedBits = false;

const COLORS = ['#667EEA', '#FF6384', '#4BC0C0', '#FF9F40', '#9966FF', '#FFCD56', '#00E676', '#E91E63'];

function cssVar(name) {
    return getComputedStyle(document.body).getPropertyValue(name).trim();
}
function themeColors() {
    return {
        axis: cssVar('--axis'), grid: cssVar('--grid'), plotBg: cssVar('--plot-bg'),
        text: cssVar('--text'), x1: cssVar('--x1'), x2: cssVar('--x2'),
    };
}
function updateThemeButton() {
    const light = document.body.classList.contains('light');
    document.getElementById('btn-theme').innerText = light ? '🌙 Тёмная' : '☀️ Светлая';
}
function toggleTheme() {
    const light = document.body.classList.toggle('light');
    localStorage.setItem('pda_web_theme', light ? 'light' : 'dark');
    updateThemeButton();
    chartWindows.forEach(w => rebuildChartWindow(w, false));
    if (bitsChart) reloadBits(false);
    refreshHeaderCursors();
}

function initDateTimeInputs() {
    let now = new Date();
    let past = new Date(now.getTime() - 24 * 3600 * 1000);
    let toIsoStr = d => new Date(d.getTime() - d.getTimezoneOffset() * 60000).toISOString().slice(0, 16);
    document.getElementById('charts-arch-start').value = toIsoStr(past);
    document.getElementById('bits-arch-start').value = toIsoStr(past);
}

function onModeChange(tab) {
    let isLive = document.getElementById(`${tab}-mode`).value === 'Live';
    document.getElementById(`${tab}-live-panel`).style.display = isLive ? 'inline-flex' : 'none';
    document.getElementById(`${tab}-arch-panel`).style.display = isLive ? 'none' : 'inline-flex';
    if (tab === 'charts') reloadCharts(true);
    else reloadBits(true);
}

function togglePause(tab) {
    // Как в десктопе: на паузе кнопка подсвечивается зелёным «add»-чипсом
    if (tab === 'charts') {
        chartsPaused = !chartsPaused;
        const btn = document.getElementById('btn-pause-charts');
        btn.innerText = chartsPaused ? '▶ Продолжить' : '⏸ Пауза';
        btn.classList.toggle('btn-add', chartsPaused);
        btn.classList.toggle('btn-action', !chartsPaused);
        if (!chartsPaused) reloadCharts(false);
    } else {
        bitsPaused = !bitsPaused;
        const btn = document.getElementById('btn-pause-bits');
        btn.innerText = bitsPaused ? '▶ Продолжить' : '⏸ Пауза';
        btn.classList.toggle('btn-add', bitsPaused);
        btn.classList.toggle('btn-action', !bitsPaused);
        if (!bitsPaused) reloadBits(false);
    }
}

// Разбор подписи окна: полный цикл «15 мин» проверяем раньше «5 мин»,
// «12/24/72 часа» раньше «1 час».
function getDurationMs(text) {
    if (text.includes('15') && text.includes('мин')) return 15 * 60 * 1000;
    if (text.includes('5') && text.includes('мин')) return 5 * 60 * 1000;
    if (text.includes('12')) return 12 * 3600 * 1000;
    if (text.includes('24')) return 24 * 3600 * 1000;
    if (text.includes('72')) return 72 * 3600 * 1000;
    if (text.includes('4') && text.includes('час')) return 4 * 3600 * 1000;
    if (text.includes('1') && text.includes('час')) return 3600 * 1000;
    return 24 * 3600 * 1000;
}

function shiftArchive(tab, dir) {
    let input = document.getElementById(`${tab}-arch-start`);
    let durMs = getDurationMs(document.getElementById(`${tab}-arch-dur`).value);
    let curr = new Date(input.value);
    let next = new Date(curr.getTime() + (durMs * dir));
    let toIsoStr = d => new Date(d.getTime() - d.getTimezoneOffset() * 60000).toISOString().slice(0, 16);
    input.value = toIsoStr(next);
    if (tab === 'charts') reloadCharts(true);
    else reloadBits(true);
}

// курсоры этого графика обновляют шапку только если график активен
function refreshHeaderCursors() {
    if (activeTab === 'bits') updateCursorPanel(bitsCursors);
    else {
        const win = getActiveWindow();
        updateCursorPanel(win ? win.cursors : null);
    }
}

function wheelZoomPlugin(chartType, cursors) {
    return {
        hooks: {
            ready: u => {
                let plot = u.root.querySelector(".u-over");
                plot.addEventListener("wheel", e => {
                    e.preventDefault();
                    if (chartType === 'charts') userZoomedCharts = true;
                    else userZoomedBits = true;
                    let factor = 0.85;
                    let left = u.posToVal(e.clientX - plot.getBoundingClientRect().left, "x");
                    let xMin = u.scales.x.min, xMax = u.scales.x.max;
                    let delta = xMax - xMin;
                    let nextDelta = e.deltaY < 0 ? delta * factor : delta / factor;
                    if (nextDelta < 0.1) nextDelta = 0.1;
                    let ratio = (left - xMin) / delta;
                    u.setScale("x", { min: left - ratio * nextDelta, max: left + (1 - ratio) * nextDelta });
                });
                plot.addEventListener("dblclick", () => resetZoom(u));

                // Перетаскивание курсоров X1/X2 (аналог movable InfiniteLine из chart_widget.py).
                // Capture-слушатель на u.root срабатывает ДО обработчика uPlot на .u-over:
                // если мышь рядом с линией курсора, гасим rubber-band zoom и тащим курсор сами.
                let dragCursor = null;   // 'x1' | 'x2'
                let suppressClick = false;
                const nearCursor = (px) => {
                    for (const name of ['x1', 'x2']) {
                        const cx = cursors[name];
                        if (cx == null) continue;
                        if (Math.abs(u.valToPos(cx, 'x', true) - px) <= 6) return name;
                    }
                    return null;
                };
                u.root.addEventListener("mousedown", e => {
                    const rect = plot.getBoundingClientRect();
                    if (!rect.contains(e.clientX, e.clientY)) return;
                    // Перетаскивание возможно только за свою линию: ЛКМ — X1, ПКМ — X2
                    const near = nearCursor(e.clientX - rect.left);
                    dragCursor = ((e.button === 0 && near === 'x1') || (e.button === 2 && near === 'x2')) ? near : null;
                    if (!dragCursor) return;
                    e.preventDefault();
                    e.stopPropagation();
                    suppressClick = true;
                    document.body.style.userSelect = 'none';
                }, true);
                window.addEventListener("mousemove", e => {
                    if (!dragCursor) return;
                    const left = e.clientX - plot.getBoundingClientRect().left;
                    cursors[dragCursor] = u.posToVal(left, "x");
                    refreshHeaderCursors();
                    u.redraw();
                });
                window.addEventListener("mouseup", () => {
                    if (!dragCursor) return;
                    dragCursor = null;
                    document.body.style.userSelect = '';
                    // click после mouseup может не прийти (отпустили вне plot) —
                    // снимаем флаг, чтобы он не «съел» следующий обычный клик
                    setTimeout(() => { suppressClick = false; }, 0);
                });

                // ЛКМ -> X1, ПКМ -> X2 (на каждом графике свои)
                plot.addEventListener("click", e => {
                    if (suppressClick) { suppressClick = false; return; }
                    cursors.x1 = u.posToVal(e.clientX - plot.getBoundingClientRect().left, "x");
                    refreshHeaderCursors();
                    u.redraw();
                });
                plot.addEventListener("contextmenu", e => {
                    e.preventDefault();
                    cursors.x2 = u.posToVal(e.clientX - plot.getBoundingClientRect().left, "x");
                    refreshHeaderCursors();
                    u.redraw();
                });
            },
            draw: u => {
                // У каждого графика свои курсоры (cursors из замыкания) —
                // линии видны на всех окнах одновременно и не связаны между собой.
                let ctx = u.ctx, top = u.bbox.top, height = u.bbox.height;
                ctx.save();
                const tc = themeColors();
                for (const [cx, lbl, color] of [[cursors.x1, 'X1', tc.x1], [cursors.x2, 'X2', tc.x2]]) {
                    if (cx == null) continue;
                    let xPos = u.valToPos(cx, "x", true);
                    if (xPos >= u.bbox.left && xPos <= u.bbox.left + u.bbox.width) {
                        ctx.strokeStyle = color; ctx.lineWidth = 2; ctx.setLineDash([5, 4]);
                        ctx.beginPath(); ctx.moveTo(xPos, top); ctx.lineTo(xPos, top + height); ctx.stroke();
                        ctx.fillStyle = color; ctx.font = "bold 11px sans-serif"; ctx.fillText(lbl, xPos + 4, top + 14);
                    }
                }
                ctx.restore();
            }
        }
    };
}

// Инициализация курсоров на 5%/95% диапазона (как cursor_x1/x2 setValue в update_data(force_fit)).
// Каждый график получает свои значения независимо.
function initCursorsFromRange(cursors, tMin, tMax, forceFit) {
    if (cursors.x1 == null || cursors.x2 == null || forceFit) {
        const span = tMax - tMin;
        cursors.x1 = tMin + span * 0.05;
        cursors.x2 = tMax - span * 0.05;
    }
}

function resetZoom(u) {
    if (!u) return;
    if (u === mainChart) userZoomedCharts = false;
    if (u === bitsChart) userZoomedBits = false;
    u.setData(u.data, true);
}

function formatTime(sec) {
    if (!sec) return "—";
    let d = new Date(sec * 1000);
    return `${String(d.getDate()).padStart(2,'0')}.${String(d.getMonth()+1).padStart(2,'0')} ${d.toTimeString().split(' ')[0]}.${String(d.getMilliseconds()).padStart(3, '0')}`;
}

function updateCursorPanel(cursors) {
    const c = cursors || { x1: null, x2: null };
    document.getElementById("lbl-x1").innerText = "X1: " + (c.x1 ? formatTime(c.x1) : "—");
    document.getElementById("lbl-x2").innerText = "X2: " + (c.x2 ? formatTime(c.x2) : "—");
    if (c.x1 && c.x2) {
        let dt = Math.abs(c.x2 - c.x1);
        let totalSec = Math.floor(dt);
        let ms = Math.floor((dt - totalSec) * 1000);
        let hh = Math.floor(totalSec / 3600);
        let mm = Math.floor((totalSec % 3600) / 60);
        let ss = totalSec % 60;
        let sMM = String(mm).padStart(2, '0'), sSS = String(ss).padStart(2, '0'), sMS = String(ms).padStart(3, '0');
        document.getElementById("lbl-dt").innerText = hh > 0
            ? `ΔT: ${hh}ч ${sMM}м ${sSS}с ${sMS}мс` : `ΔT: ${mm}м ${sSS}с ${sMS}мс`;
    } else {
        document.getElementById("lbl-dt").innerText = "ΔT: —";
    }
}

let activeTab = 'charts';
function switchTab(name) {
    document.querySelectorAll('.tab-btn').forEach(b => b.classList.remove('active'));
    document.querySelectorAll('.tab-page').forEach(p => p.classList.remove('active'));
    document.getElementById(`btn-tab-${name}`).classList.add('active');
    document.getElementById(`page-${name}`).classList.add('active');
    activeTab = name;
    if (name === 'charts') {
        refreshHeaderCursors();
        setTimeout(() => { if (mainChart) resizeChart(mainChart, getActiveWindow()); }, 50);
    } else if (name === 'bits') {
        updateCursorPanel(bitsCursors);
        setTimeout(() => {
            if (bitsChart) resizeBits();
            else reloadBits(true);
        }, 50);
    } else if (name === 'io') {
        ioRefreshAll();
    } else if (name === 'db') {
        dbStartPanel();
    } else if (name === 'help') {
        renderHelp();
    }
}

// ---------------------------------------------------------------- сигналы/дерево
async function fetchTags() {
    const list = document.getElementById('sidebar-tags');
    try {
        allTags = await apiGet('/api/tags');
        renderSidebar();
        renderBitsTree();
        if (typeof ioOnTagsChanged === 'function') ioOnTagsChanged();
    } catch (e) {
        list.innerHTML = `<li style="padding:12px; color:var(--error);">Ошибка: ${e.message}</li>`;
    }
}

function escHtml(s) {
    return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

function buildTreeHtml(tags, leafRenderer) {
    const byConn = {};
    tags.forEach(t => {
        const c = t.connection || 'Без подключения';
        const g = t.group || 'Общие';
        (byConn[c] = byConn[c] || {});
        (byConn[c][g] = byConn[c][g] || []).push(t);
    });
    let html = '';
    Object.keys(byConn).sort().forEach(c => {
        html += `<li><div class="tree-branch">🔌 ${escHtml(c)}</div><ul>`;
        Object.keys(byConn[c]).sort().forEach(g => {
            html += `<li><div class="tree-branch">📁 ${escHtml(g)} (${byConn[c][g].length})</div><ul>`;
            html += byConn[c][g].map(leafRenderer).join('');
            html += `</ul></li>`;
        });
        html += `</ul></li>`;
    });
    return html;
}

function renderSidebar() {
    const list = document.getElementById('sidebar-tags');
    if (!allTags.length) { list.innerHTML = `<li class="tree-empty">Нет сигналов в базе</li>`; return; }
    list.innerHTML = buildTreeHtml(allTags, t => `
        <li><div class="tree-leaf" draggable="true" ondragstart="dragSignal(event, ${t.id})" onclick="addSignal(${t.id})">
            ${escHtml(t.name)} <span style="color:var(--text-faint); font-size:11px;">${escHtml(t.unit || '')}</span>
        </div></li>`);
}

const BIT_TYPES = new Set(['BYTE', 'INT16', 'UINT16']);
let bitsSelectedId = null;

function renderBitsTree() {
    const root = document.getElementById('bits-tree');
    const tags = allTags.filter(t => BIT_TYPES.has((t.data_type || '').toUpperCase()));
    if (!tags.length) { root.innerHTML = `<li class="tree-empty">Нет сигналов типа BYTE/INT16/UINT16</li>`; bitsSelectedId = null; return; }
    root.innerHTML = buildTreeHtml(tags, t => {
        const sel = t.id === bitsSelectedId ? ' selected' : '';
        return `<li><div id="bits-node-${t.id}" class="tree-leaf${sel}" onclick="selectBitsTag(${t.id})">` +
               `${escHtml(t.name)} [${escHtml(t.data_type)}]</div></li>`;
    });
    if (!bitsSelectedId || !document.getElementById(`bits-node-${bitsSelectedId}`)) {
        selectBitsTag(tags[0].id);
    }
}

function selectBitsTag(tagId) {
    const prev = document.querySelector('#bits-tree .tree-leaf.selected');
    if (prev) prev.classList.remove('selected');
    const node = document.getElementById(`bits-node-${tagId}`);
    if (node) node.classList.add('selected');
    bitsSelectedId = tagId;
    const t = allTags.find(x => x.id === tagId);
    document.getElementById('bits-current').innerText = `Регистр: ${t ? `${t.name} (${t.data_type})` : '—'}`;
    updateBitsSwapButton();
    reloadBits(true);
}

// ---------------------------------------------------------------- CHARTS
function getChartsUrl(tagId) {
    if (document.getElementById('charts-mode').value === 'Live') {
        return `/api/points/${tagId}?span=${encodeURIComponent(document.getElementById('charts-span').value)}`;
    }
    const start = document.getElementById('charts-arch-start').value;
    const dur = document.getElementById('charts-arch-dur').value;
    return `/api/archive/${tagId}?start=${encodeURIComponent(start)}&duration=${encodeURIComponent(dur)}`;
}

function getActiveWindow() {
    return chartWindows.find(w => w.id === activeWindowId) || chartWindows[0];
}

function dragSignal(event, tagId) {
    event.dataTransfer.effectAllowed = 'copy';
    event.dataTransfer.setData('text/plain', String(tagId));
}

function addChartWindow() {
    const id = Date.now() + Math.random();
    const box = document.createElement('div');
    box.className = 'chart-window';
    box.dataset.windowId = id;
    box.innerHTML = `<button class="window-close" onclick="removeChartWindow(${id}); event.stopPropagation();">×</button>` +
        `<div class="chart-box"><div class="chart-legend" style="display:none;"></div></div>` +
        `<div class="resize-handle" title="Потяните за нижний край, чтобы изменить высоту окна"></div>`;
    box.addEventListener('click', () => selectChartWindow(id));
    box.addEventListener('dragover', event => { event.preventDefault(); box.classList.add('drop-target'); });
    box.addEventListener('dragleave', () => box.classList.remove('drop-target'));
    box.addEventListener('drop', event => { event.preventDefault(); box.classList.remove('drop-target'); selectChartWindow(id); const tagId = Number(event.dataTransfer.getData('text/plain')); if (tagId) addSignal(tagId); });
    attachResizeHandle(box);
    const win = { id, signals: [], chart: null, zoomed: false, cursors: makeCursors() };
    // ResizeObserver (аналог live-ресайза Qt-виджета): uPlot перерисовывается
    // при любой смене размеров контейнера — ручка ресайза, сворачивание панелей, окно браузера.
    const ro = new ResizeObserver(() => resizeChart(win.chart, win));
    ro.observe(box.querySelector('.chart-box'));
    win.resizeObserver = ro;
    document.getElementById('chart-windows').appendChild(box);
    chartWindows.push(win);
    selectChartWindow(id);
}

// Ресайз окна графика за нижнюю ручку: min-height 260px как в ChartWidget (chart_widget.py).
function attachResizeHandle(winBox) {
    const handle = winBox.querySelector('.resize-handle');
    handle.addEventListener('mousedown', (e) => {
        e.preventDefault();
        e.stopPropagation();
        const startY = e.clientY;
        const startH = winBox.offsetHeight;
        winBox.classList.add('resizing');
        const onMove = (ev) => {
            winBox.style.height = `${Math.max(260, startH + (ev.clientY - startY))}px`;
        };
        const onUp = () => {
            winBox.classList.remove('resizing');
            window.removeEventListener('mousemove', onMove);
            window.removeEventListener('mouseup', onUp);
        };
        window.addEventListener('mousemove', onMove);
        window.addEventListener('mouseup', onUp);
    });
}

function selectChartWindow(id) {
    activeWindowId = id;
    document.querySelectorAll('.chart-window').forEach(box => box.classList.toggle('active', Number(box.dataset.windowId) === id));
    // Курсоры у каждого окна свои — в шапке показываем значения активного окна.
    if (activeTab === 'charts') refreshHeaderCursors();
}

function removeChartWindow(id) {
    if (chartWindows.length <= 1) return;
    const index = chartWindows.findIndex(w => w.id === id);
    if (index < 0) return;
    chartWindows[index].chart?.destroy();
    chartWindows[index].resizeObserver?.disconnect();
    document.querySelector(`.chart-window[data-window-id="${id}"]`)?.remove();
    chartWindows.splice(index, 1);
    if (activeWindowId === id) selectChartWindow(chartWindows[Math.max(0, index - 1)].id);
}

async function addSignal(tagId) {
    const win = getActiveWindow();
    if (!win || win.signals.some(s => s.tag.id === tagId)) return;
    const tag = allTags.find(t => t.id === tagId);
    if (!tag) return;
    try {
        const data = await apiGet(getChartsUrl(tagId));
        if (!data.times || data.times.length === 0) return;
        win.signals.push({ tag, times: data.times, values: data.values, t_start: data.t_start, t_end: data.t_end });
        rebuildChartWindow(win);
    } catch (e) { console.warn('charts load', e); }
}

function removeSignal(tagId) {
    const win = getActiveWindow();
    if (!win) return;
    win.signals = win.signals.filter(s => s.tag.id !== tagId);
    rebuildChartWindow(win);
}

function clearCharts() {
    const win = getActiveWindow();
    if (!win) return;
    win.signals = [];
    rebuildChartWindow(win);
}

function fmtHoverValue(v) {
    if (v === null || v === undefined || isNaN(v)) return '';
    if (Math.abs(v) >= 1 && Math.abs(v - Math.round(v)) < 1e-6) return String(Math.round(v));
    return String(parseFloat(v.toFixed(3)));
}

function stepValueAt(times, values, t) {
    if (!times || !times.length || t < times[0]) return null;
    let lo = 0, hi = times.length;
    while (lo < hi) { const m = (lo + hi) >> 1; if (times[m] <= t) lo = m + 1; else hi = m; }
    const idx = lo - 1;
    return idx >= 0 ? values[idx] : null;
}

function bindLegendValues(win, chart) {
    const over = chart && chart.over;
    if (!over) return;
    const clear = () => win.signals.forEach((s, i) => {
        const el = win.legendVals && win.legendVals[i];
        if (el) el.textContent = '';
    });
    over.addEventListener('mousemove', (e) => {
        const rect = over.getBoundingClientRect();
        const left = e.clientX - rect.left;
        if (left < 0 || left > rect.width) { clear(); return; }
        const t = chart.posToVal(left, "x");
        win.signals.forEach((s, i) => {
            const el = win.legendVals && win.legendVals[i];
            if (el) el.textContent = fmtHoverValue(stepValueAt(s.times, s.values, t));
        });
    });
    over.addEventListener('mouseleave', clear);
}

function renderLegend(win, legend) {
    legend.innerHTML = win.signals.map((s, idx) => {
        const color = COLORS[idx % COLORS.length];
        return `<div class="legend-row"><span class="legend-line" style="background-color:${color};"></span><span class="legend-name" style="color:${color};">${escHtml(s.tag.name)}</span><span class="legend-val" data-idx="${idx}" style="color:${color};"></span><button class="legend-del" onclick="removeSignal(${s.tag.id}); event.stopPropagation();">×</button></div>`;
    }).join('');
    win.legendVals = Array.from(legend.querySelectorAll('.legend-val'));
    legend.style.display = win.signals.length ? 'flex' : 'none';
}

async function reloadCharts(forceFit = false) {
    const isArchive = document.getElementById('charts-mode').value === 'Archive';
    const emptySignals = [];
    for (const win of chartWindows) {
        if (forceFit) win.zoomed = false;
        for (const s of win.signals) {
            try {
                const data = await apiGet(getChartsUrl(s.tag.id));
                if (data.times && data.times.length > 0) Object.assign(s, { times: data.times, values: data.values, t_start: data.t_start, t_end: data.t_end });
                else emptySignals.push(s.tag.name);
            } catch (e) { if (isArchive && forceFit) emptySignals.push(s.tag.name); }
        }
        rebuildChartWindow(win, forceFit);
    }
    // Раньше пустой ответ молча оставлял на экране старый график («тишина»).
    // Теперь при явной загрузке архива говорим, что данных в окне нет.
    if (isArchive && forceFit && emptySignals.length) {
        const start = document.getElementById('charts-arch-start').value;
        const dur = document.getElementById('charts-arch-dur').value;
        alertBox('Архив', `Нет данных за период ${start} + ${dur}\nСигналы: ${emptySignals.join(', ')}`);
    }
}

function rebuildChartWindow(win, forceFit = false) {
    const root = document.querySelector(`.chart-window[data-window-id="${win.id}"]`);
    const box = root?.querySelector('.chart-box');
    const legend = root?.querySelector('.chart-legend');
    if (!box || !legend) return;
    win.chart?.destroy();
    box.querySelectorAll('canvas, .uplot').forEach(el => el.remove());
    renderLegend(win, legend);
    if (!win.signals.length) return;

    const baseTimes = win.signals[0].times;
    const data = [baseTimes];
    const series = [{ label: "Time" }];
    win.signals.forEach((s, idx) => {
        series.push({ label: s.tag.name, stroke: COLORS[idx % COLORS.length], width: 2, paths: uPlot.paths.stepped({ align: 1 }) });
        data.push(s.values);
    });
    const tMin = win.signals[0].t_start || baseTimes[0];
    const tMax = win.signals[0].t_end || baseTimes[baseTimes.length - 1];
    const fit = !win.zoomed || forceFit;
    if (fit) initCursorsFromRange(win.cursors, tMin, tMax, forceFit);
    const tc = themeColors();
    const opts = {
        width: box.clientWidth || 1000, height: box.clientHeight || 400,
        scales: { x: { time: true, min: fit ? tMin : undefined, max: fit ? tMax : undefined } },
        plugins: [wheelZoomPlugin('charts', win.cursors)],
        axes: [{ stroke: tc.axis, grid: { stroke: tc.grid } }, { stroke: tc.axis, grid: { stroke: tc.grid } }],
        series, cursor: { drag: { x: true, y: false } }
    };
    win.chart = new uPlot(opts, data, box);
    bindLegendValues(win, win.chart);
    if (win.id === activeWindowId) { mainChart = win.chart; refreshHeaderCursors(); }
}

// ---------------------------------------------------------------- BITS
function toggleBitsSwap() {
    bitsByteSwap = !bitsByteSwap;
    localStorage.setItem('pda_bits_byteswap', bitsByteSwap ? '1' : '0');
    updateBitsSwapButton();
    reloadBits(false);
}

function updateBitsSwapButton() {
    const btn = document.getElementById('btn-swap-bits');
    if (!btn) return;
    btn.innerText = bitsByteSwap ? '↕ Lo-Hi' : '↕ Hi-Lo';
    // Как в bits_widget.py: swap актуален только для 16-битных слов (INT16/UINT16),
    // для BYTE кнопка неактивна; включённый swap подсвечивается зелёным «add»-чипсом.
    const tag = allTags.find(x => x.id === bitsSelectedId);
    const isByte = !!tag && (tag.data_type || '').toUpperCase() === 'BYTE';
    btn.disabled = isByte;
    btn.classList.toggle('btn-add', bitsByteSwap && !isByte);
    btn.classList.toggle('btn-action', !bitsByteSwap || isByte);
}

function getBitsUrl(tagId) {
    if (document.getElementById('bits-mode').value === 'Live') {
        return `/api/points/${tagId}?span=${encodeURIComponent(document.getElementById('bits-span').value)}`;
    }
    const start = document.getElementById('bits-arch-start').value;
    const dur = document.getElementById('bits-arch-dur').value;
    return `/api/archive/${tagId}?start=${encodeURIComponent(start)}&duration=${encodeURIComponent(dur)}`;
}

const BIT_COLORS = [
    '#FF6384', '#4BC0C0', '#FFCE56', '#36A2EB', '#FF9F40', '#9966FF',
    '#C9CBCF', '#00E676', '#E91E63', '#00BCD4', '#FF5722', '#8BC34A',
    '#FFEB3B', '#9C27B0', '#795548', '#607D8B'
];

async function reloadBits(forceFit = false) {
    if (!bitsSelectedId) return;
    if (forceFit) userZoomedBits = false;
    let payload;
    try { payload = await apiGet(getBitsUrl(bitsSelectedId)); }
    catch (e) { return; }
    const box = document.getElementById('uplot-bits');
    box.innerHTML = '';
    if (bitsChart) { bitsChart.destroy(); bitsChart = null; }
    if (!payload.times || payload.times.length === 0) return;

    const tag = allTags.find(x => x.id === bitsSelectedId);
    const nBits = (tag && (tag.data_type || '').toUpperCase() === 'BYTE') ? 8 : 16;

    const data = [payload.times];
    const series = [{ label: "Time" }];
    // Bit 0 — верхняя дорожка: y = nBits-1-b. Lo-Hi меняет байты слова.
    const prep = payload.values.map(v => {
        let w = Math.floor(v) & 0xFFFF;
        if (bitsByteSwap && nBits === 16) w = ((w >> 8) | (w << 8)) & 0xFFFF;
        return w;
    });
    for (let b = 0; b < nBits; b++) {
        const pos = nBits - 1 - b;
        series.push({ label: `Bit ${b}`, stroke: BIT_COLORS[b], width: 1.5, paths: uPlot.paths.stepped({ align: 1 }) });
        data.push(prep.map(w => pos + (((w >> b) & 1) * 0.75)));
    }

    let tMin = payload.t_start || payload.times[0];
    let tMax = payload.t_end || payload.times[payload.times.length - 1];
    const fit = !userZoomedBits || forceFit;
    if (fit) initCursorsFromRange(bitsCursors, tMin, tMax, forceFit);
    const tc = themeColors();
    const opts = {
        width: box.clientWidth || 1200, height: box.clientHeight || 600,
        scales: {
            x: { time: true, min: (!userZoomedBits || forceFit) ? tMin : undefined, max: (!userZoomedBits || forceFit) ? tMax : undefined },
            y: { min: -0.5, max: nBits + 0.5 }
        },
        plugins: [wheelZoomPlugin('bits', bitsCursors)],
        axes: [
            { stroke: tc.axis, grid: { stroke: tc.grid } },
            { stroke: tc.axis, grid: { stroke: tc.grid }, values: () => [], size: 90 }
        ],
        hooks: { draw: [(self) => drawBitsTickLabels(self, nBits)] },
        series, cursor: { drag: { x: true, y: false } }
    };
    bitsChart = new uPlot(opts, data, box);
    if (activeTab === 'bits') updateCursorPanel(bitsCursors);
}

// Экспорт активного окна графика в PNG (аналог «Сохранить как изображение» из десктопа).
// Chromium/WebView2 блокируют скачивание по data:URL (toDataURL + a.click = тишина),
// поэтому используем Blob + URL.createObjectURL — как в downloadFile из io.js.
// Файл уходит в папку «Загрузки» без диалога (политика WebView2), поэтому
// показываем подтверждение.
function exportActiveChartPng() {
    const win = getActiveWindow();
    if (!win || !win.chart || !win.signals.length) {
        alertBox('Экспорт PNG', 'На активном графике нет сигналов');
        return;
    }
    const name = `pda_chart_${Date.now()}.png`;
    win.chart.canvas.toBlob(blob => {
        if (!blob) { alertBox('Экспорт PNG', 'Не удалось сформировать изображение', true); return; }
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = name;
        document.body.appendChild(a);
        a.click();
        a.remove();
        setTimeout(() => URL.revokeObjectURL(url), 2000);
        alertBox('Экспорт PNG', `Файл сохранен в папку «Загрузки»: ${name}`);
    }, 'image/png');
}

function drawBitsTickLabels(self, nBits) {
    const ctx = self.ctx;
    const axis = self.axes[1];
    if (!axis || !axis._show) return;
    const dpr = uPlot.pxRatio;
    const gap = axis.gap * dpr;
    const tickSize = axis.ticks.show ? Math.round(axis.ticks.size * dpr) : 0;
    const x = Math.round(axis._pos * dpr) - (tickSize + gap);
    ctx.font = axis.font[0];
    ctx.fillStyle = axis.stroke(self, 1) || '#888';
    ctx.textAlign = 'right';
    ctx.textBaseline = 'middle';
    for (let t = 0; t < nBits; t++) {
        const y = self.valToPos(t, 'y', true);
        ctx.fillText(`Bit ${nBits - 1 - t}`, x, y - 7 * dpr);
    }
}

function resizeBits() {
    const box = document.getElementById('uplot-bits');
    if (bitsChart && box && box.clientWidth > 0 && box.clientHeight > 0) {
        bitsChart.setSize({ width: box.clientWidth, height: box.clientHeight });
    }
}

new ResizeObserver(resizeBits).observe(document.getElementById('uplot-bits'));

function resizeChart(chartInstance, win) {
    const root = win && document.querySelector(`.chart-window[data-window-id="${win.id}"]`);
    const box = root?.querySelector('.chart-box');
    if (chartInstance && box && box.clientWidth > 0 && box.clientHeight > 0) {
        chartInstance.setSize({ width: box.clientWidth, height: box.clientHeight });
    }
}

window.addEventListener('resize', () => {
    if (document.getElementById('page-charts').classList.contains('active')) {
        resizeChart(mainChart, getActiveWindow());
    }
    if (document.getElementById('page-bits').classList.contains('active')) resizeBits();
});

// ---------------------------------------------------------------- live WS
// Реестр живых значений: /api/live/ws толкает {"type":"live","rows":[...]}.
// Держим карту tag_id -> row; IO-таблица подписывается через onLiveUpdate.
const liveRows = new Map();
const liveListeners = [];
function onLiveUpdate(fn) { liveListeners.push(fn); }

let wsRetries = 0;
function connectLiveWs() {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const ws = new WebSocket(`${proto}//${location.host}/api/live/ws`);
    ws.onmessage = (ev) => {
        try {
            const msg = JSON.parse(ev.data);
            if (msg.type !== 'live') return;
            liveRows.clear();
            for (const r of msg.rows) liveRows.set(r.id, r);
            wsRetries = 0;
            liveListeners.forEach(fn => fn());
        } catch (e) { /* ignore */ }
    };
    ws.onclose = () => {
        // Экспоненциальный backoff: 1, 2, 4, ... максимум 30 секунд.
        const delay = Math.min(30000, 1000 * Math.pow(2, wsRetries++));
        setTimeout(connectLiveWs, delay);
    };
}

// ---------------------------------------------------------------- app status
async function refreshAppBadge() {
    try {
        const st = await apiGet('/api/status');
        const a = st.app || {};
        const el = document.getElementById('app-badge');
        el.title = `${st.running || 0} модулей работает`;
        el.textContent = `${a.name || 'PDA'} ${a.version || ''}`;
    } catch (e) { /* сервер ещё не поднялся */ }
}

// ---------------------------------------------------------------- modal utils
// Одна общая подложка; io.js/db.js строят формы через openForm.
function closeModal() {
    document.getElementById('modal-overlay').style.display = 'none';
    document.getElementById('modal-box').innerHTML = '';
}
function openModal(html, onReady) {
    const box = document.getElementById('modal-box');
    box.innerHTML = html;
    document.getElementById('modal-overlay').style.display = 'flex';
    if (onReady) onReady(box);
    box.querySelectorAll('input,select').forEach(el => el.addEventListener('keydown', e => { if (e.key === 'Escape') closeModal(); }));
}
document.getElementById('modal-overlay').addEventListener('click', e => { if (e.target.id === 'modal-overlay') closeModal(); });

// Подтверждение: resolve(true/false), cancel -> null.
function confirmBox(text, { yes = 'Да', no = 'Нет', cancel = null } = {}) {
    return new Promise(resolve => {
        const ov = document.getElementById('modal-confirm');
        document.getElementById('confirm-text').textContent = text;
        const bY = document.getElementById('confirm-yes'), bN = document.getElementById('confirm-no'), bC = document.getElementById('confirm-cancel');
        bY.textContent = yes; bN.textContent = no;
        bC.style.display = cancel ? '' : 'none';
        if (cancel) bC.textContent = cancel;
        ov.style.display = 'flex';
        const done = (val) => { ov.style.display = 'none'; bY.onclick = bN.onclick = bC.onclick = null; resolve(val); };
        bY.onclick = () => done(true);
        bN.onclick = () => done(false);
        bC.onclick = () => done(null);
    });
}

// Простое сообщение (аналог QMessageBox.information/critical)
function alertBox(title, message, isError = false) {
    openModal(`<h3>${escHtml(title)}</h3>
        <div class="confirm-text" style="color:${isError ? 'var(--error)' : 'var(--text)'}">${escHtml(message)}</div>
        <div class="confirm-btns"><button class="btn-action" onclick="closeModal()">ОК</button></div>`);
}

// ---------------------------------------------------------------- live-обновление
// Каждые 1000 мс опрашиваем /api/points для обновления статус-панели.
setInterval(async () => {
    const isChartsActive = document.getElementById('page-charts').classList.contains('active');
    const isBitsActive = document.getElementById('page-bits').classList.contains('active');
    const hasSignals = chartWindows.some(w => w.signals.length > 0);
    if (isChartsActive && hasSignals && !chartsPaused && document.getElementById('charts-mode').value === 'Live') {
        reloadCharts(false);
    }
    if (isBitsActive && !bitsPaused && document.getElementById('bits-mode').value === 'Live') {
        reloadBits(false);
    }
}, 1000);

setInterval(refreshAppBadge, 5000);

// ---------------------------------------------------------------- init
(async function init() {
    if (localStorage.getItem('pda_web_theme') === 'light') document.body.classList.add('light');
    updateThemeButton();
    initDateTimeInputs();
    if (bitsByteSwap) updateBitsSwapButton();
    await fetchTags();
    addChartWindow();
    connectLiveWs();
    refreshAppBadge();
    // Справочник драйверов нужен IO-вкладке сразу (конструкторы адресов)
    try { driverTypes = (await apiGet('/api/drivers/info')).types || []; } catch (e) { driverTypes = []; }
})();
