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
// Нативная шапка окна (WebView2 в pda-desktop) рисуется Windows и сама про
// тему страницы ничего не знает — иначе над тёмным интерфейсом остаётся
// светлая полоса заголовка. pda-desktop слушает это сообщение и ставит
// окну тёмный/светлый режим (tao -> DWMWA_USE_IMMERSIVE_DARK_MODE).
function syncWindowTheme() {
    try {
        if (window.ipc) window.ipc.postMessage(document.body.classList.contains('light') ? 'theme:light' : 'theme:dark');
    } catch (e) { /* в браузере IPC нет — не ошибка */ }
}

function toggleTheme() {
    const light = document.body.classList.toggle('light');
    localStorage.setItem('pda_web_theme', light ? 'light' : 'dark');
    updateThemeButton();
    syncWindowTheme();
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

async function onModeChange(tab) {
    let isLive = document.getElementById(`${tab}-mode`).value === 'Live';
    document.getElementById(`${tab}-live-panel`).style.display = isLive ? 'inline-flex' : 'none';
    document.getElementById(`${tab}-arch-panel`).style.display = isLive ? 'none' : 'inline-flex';
    // Смена режима сбрасывает окно, выбранное на мини-обзоре: дальше грузим
    // по полям «Начало» + «Длительность».
    clearArchWindow();
    if (!isLive) {
        // В Archive знать, где конец истории, полезнее чем «now − 24 часа»:
        // подставляем начало окна от последней записи, если дату не выбрали вручную.
        // Обзор суток перерисует reloadCharts(true) ниже.
        await refreshLastData();
        if (archLastMs != null && !userSetArchStart.has(tab)) {
            const durMs = getDurationMs(document.getElementById(`${tab}-arch-dur`).value);
            setArchStartValue(tab, archLastMs - durMs);
        }
    }
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
    userSetArchStart.add(tab);
    clearArchWindow();
    if (tab === 'charts') reloadCharts(true);
    else reloadBits(true);
}

// ------------------------------------------------- Архив: метка последних данных + обзор суток
// archLastMs — epoch-мс самой свежей записи в data_points (null — база пуста).
// В режиме Archive показываем её в панели: видно, до какого момента вообще
// есть история, и не надо гадать с датой загрузки.
let archLastMs = null;
let archOverviewChart = null;
let archOverviewResize = null;
const userSetArchStart = new Set(); // где «Начало» выставили вручную — не подставляем
// Точное окно, выбранное мышью на мини-обзоре: {t0, t1} в epoch-секундах.
// Пока оно задано, архив грузится по нему, а не по «Начало + Длительность»
// (иначе выбор на обзоре «съезжал» до ближайшего значения списка).
let archWindow = null;

function clearArchWindow() { archWindow = null; }

/// Подставляет окно из обзора в панель: «Начало» = левая граница, а
/// «Длительность» — ближайший вариант списка (чтобы панель не врала).
function setArchWindow(t0, t1) {
    if (!(t1 > t0)) return;
    archWindow = { t0, t1 };
    setArchStartValue('charts', Math.round(t0 * 1000));
    const sel = document.getElementById('charts-arch-dur');
    if (sel) {
        const spanMs = (t1 - t0) * 1000;
        let best = null, bestDiff = Infinity;
        for (const o of sel.options) {
            const d = Math.abs(getDurationMs(o.value) - spanMs);
            if (d < bestDiff) { bestDiff = d; best = o; }
        }
        if (best) sel.value = best.value;
    }
    userSetArchStart.add('charts');
}

function fmtArchTs(ms) {
    const d = new Date(ms);
    const p = n => String(n).padStart(2, '0');
    return `${p(d.getDate())}.${p(d.getMonth() + 1)}.${d.getFullYear()} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/// Текущее архивное окно в epoch-мс: точное выделение с мини-обзора, иначе
/// «Начало» + «Длительность» из панели. Мини-обзор рисуется по этим же
/// границам — иначе его ось расходилась с датой, которую видно сверху.
function archWindowMs(tab) {
    if (tab === 'charts' && archWindow)
        return { t0: archWindow.t0 * 1000, t1: archWindow.t1 * 1000 };
    const startMs = new Date(document.getElementById(`${tab}-arch-start`).value).getTime();
    const durMs = getDurationMs(document.getElementById(`${tab}-arch-dur`).value);
    if (isNaN(startMs)) return null;
    return { t0: startMs, t1: startMs + durMs };
}

function setLastDataHint(tab) {
    const el = document.getElementById(`${tab}-last-data`);
    if (!el) return;
    if (archLastMs == null) {
        el.textContent = 'Последних данных в базе нет';
        el.classList.add('st-warn');
    } else {
        el.textContent = `Последние данные: ${fmtArchTs(archLastMs)}`;
        el.classList.remove('st-warn');
    }
}

async function refreshLastData() {
    try {
        const r = await apiGet('/api/last');
        archLastMs = (r.t_last == null) ? null : Math.round(r.t_last * 1000);
    } catch (e) { /* БД может быть недоступна — не считаем это ошибкой графика */ }
    setLastDataHint('charts');
    setLastDataHint('bits');
}

function setArchStartValue(tab, ms) {
    const toIsoStr = d => new Date(d.getTime() - d.getTimezoneOffset() * 60000).toISOString().slice(0, 16);
    document.getElementById(`${tab}-arch-start`).value = toIsoStr(new Date(ms));
}

function hideArchOverview() {
    archOverviewGaps = [];
    archOverviewRange = null;
    archOverviewUpdateSelHint();
    if (archOverviewChart) {
        archOverviewChart.destroy();
        archOverviewChart = null;
    }
    const wrap = document.getElementById('charts-arch-overview');
    if (wrap) wrap.style.display = 'none';
}

/// Мини-график по ТЕКУЩЕМУ архивному окну («Начало» + «Длительность» либо
/// точное выделение с обзора): сервер прореживает окно до MAX_POINTS бакетов,
/// поэтому обзор строится быстро даже на миллионах строк.
/// Раньше обзор был жёстко «сутки от последней записи» и его ось расходилась с
/// датой в панели — пользователь видел 10:00..19:00 при «Начало 30.10 20:15».
/// Выделение мышью + кнопка «⤓ Выбранное» сужают окно до выбранного участка.
async function loadArchOverview() {
    const wrap = document.getElementById('charts-arch-overview');
    const box = document.getElementById('uplot-arch-overview');
    if (!wrap || !box) return;
    const win = getActiveWindow();
    const tags = win ? win.signals.map(s => s.tag.id) : [];
    const range = archWindowMs('charts');
    if (document.getElementById('charts-mode').value !== 'Archive' || !tags.length || !range) {
        hideArchOverview();
        return;
    }
    wrap.style.display = 'flex';
    const startMs = range.t0, endMs = range.t1;
    archOverviewSetTitle(startMs, endMs);
    const url = `/api/series?tags=${encodeURIComponent(tags.join(','))}&t0=${(startMs / 1000).toFixed(3)}&t1=${(endMs / 1000).toFixed(3)}`;
    let resp = null;
    try { resp = await apiGet(url); } catch (e) { /* не критично: основной график не зависит от обзора */ }
    const byTag = new Map();
    for (const s of (resp && resp.series) || []) byTag.set(s.tag_id, s);
    const list = win.signals
        .map(sg => ({ tag: sg.tag, s: byTag.get(sg.tag.id) }))
        .filter(x => x.s && x.s.times && x.s.times.length);

    if (archOverviewChart) {
        archOverviewChart.destroy();
        archOverviewChart = null;
        box.querySelectorAll('canvas, .uplot').forEach(el => el.remove());
    }
    const data = list.length
        ? [list[0].s.times, ...list.map(x => x.s.values)]
        : [[], []];
    const seriesCfg = list.length
        ? [{ label: 'Time' }, ...list.map((x, i) => ({
            label: x.tag.name, stroke: COLORS[i % COLORS.length], width: 1,
        }))]
        // Окно без данных вовсе: линия не нужна, но обзор остаётся — весь он
        // залит красным, и это честная картинка «данных нет».
        : [{ label: 'Time' }, { label: 'нет данных', stroke: 'transparent' }];
    const tc = themeColors();
    // Окно обзора фиксируем до создания графика: хук draw рисует гэпы
    // сразу при первой отрисовке.
    archOverviewRange = { t0: startMs / 1000, t1: endMs / 1000 };
    archOverviewGaps = [];
    const u = new uPlot({
        width: box.clientWidth || 800,
        height: 110,
        scales: { x: { time: true, min: startMs / 1000, max: endMs / 1000 } },
        axes: [
            // Одна короткая ось времени внизу; Y скрываем — обзорные подписи только мешают.
            // uPlot ждёт от values() массив пар [метка, разделитель].
            { side: 2, size: 20, gap: 2, stroke: tc.axis, grid: { show: false },
              values: (u, ticks) => ticks.map(t => [fmtShortTick(t, (endMs - startMs) / 1000), ""]) },
            { show: false },
        ],
        series: seriesCfg,
        legend: { show: false },
        hooks: {
            // Рамка выделения живёт в DOM и остаётся поверх любой перерисовки,
            // а гэпы рисуем на canvas — после каждой отрисовки обзора.
            draw: [archOverviewDrawGaps],
        },
        // setScale:false — выделение мышью только рисует рамку (её и переносим
        // кнопкой «⤓ Выбранное»), а не зумит сам обзор суток.
        cursor: { drag: { x: true, y: false, setScale: false }, point: { show: false }, focus: { dist: () => 1e9 } },
    }, data, box);
    archOverviewChart = u;

    // Ресайз зоны графика должен тянуть за собой и мини-обзор.
    if (!archOverviewResize) {
        archOverviewResize = new ResizeObserver(() => {
            if (archOverviewChart && box.clientWidth)
                archOverviewChart.setSize({ width: box.clientWidth, height: 110 });
        });
        archOverviewResize.observe(box);
    }

    archOverviewWireDblClick(box);

    // Подсветка интервалов БЕЗ данных за то же окно. Ответ маленький
    // (пустые бакеты склеены в гэпы на сервере), поэтому обзор не тормозит.
    // Шаг бакета держим ~2.4 мин как в исходных сутках: сетка мельче была бы
    // гуще периода опроса, и «пропуском» выглядел бы каждый второй бакет.
    const gapBuckets = Math.max(20, Math.min(600, Math.round((endMs - startMs) / 144000)));
    try {
        const g = await apiGet(
            `/api/gaps?tags=${encodeURIComponent(tags.join(','))}` +
            `&t0=${(startMs / 1000).toFixed(3)}&t1=${(endMs / 1000).toFixed(3)}` +
            `&buckets=${gapBuckets}`
        );
        // Обзор могли пересоздать, пока шёл запрос — проверяем, что рисовать
        // гэпы всё ещё куда.
        if (archOverviewChart === u) {
            archOverviewGaps = (g && g.gaps) || [];
            u.redraw();
        }
    } catch (e) { /* нет подсветки — не беда: основной график от неё не зависит */ }
    archOverviewUpdateSelHint();
}

/// гэпы [[t0,t1], ...] и окно обзора — в epoch-секундах
let archOverviewGaps = [];
let archOverviewRange = null;

/// Прямоугольники гэпов в координатах canvas (device px) + индекс гэпа.
/// ВАЖНО: uPlot НЕ масштабирует ctx на devicePixelRatio (canvas шире в pxRatio
/// раз), а bbox и valToPos(..., can=true) — уже в device-пикселях.
/// Индекс нужен, чтобы двойной клик искал пропуск по тем же прямоугольникам,
/// которые реально нарисованы, а не по отдельному пересчёту времени.
function archOverviewGapRects(u) {
    if (!u || !archOverviewRange || !archOverviewGaps.length) return [];
    const { t0, t1 } = archOverviewRange;
    if (!(t1 > t0)) return [];
    const bb = u.bbox;
    const out = [];
    for (let i = 0; i < archOverviewGaps.length; i++) {
        const g = archOverviewGaps[i];
        const a = Math.max(g[0], t0), b = Math.min(g[1], t1);
        if (!(b > a)) continue;
        const x0 = Math.max(u.valToPos(a, 'x', true), bb.left);
        const x1 = Math.min(u.valToPos(b, 'x', true), bb.left + bb.width);
        if (x1 > x0) out.push({ i, x0, x1 });
    }
    return out;
}

/// Минимальная ширина подсветки: пропуск в один бакет суток это ~2 px, он
/// должен и видеться, и попадаться кликом.
const GAP_MIN_W_CSS = 3;
/// Допуск двойного клика по подсветке в CSS-пикселях в стороны от полосы.
const GAP_HIT_TOL_CSS = 8;

function archOverviewDrawGaps(u) {
    const rects = archOverviewGapRects(u);
    if (!rects.length) return;
    const ctx = u.ctx;
    const bb = u.bbox;
    const pr = uPlot.pxRatio || 1;
    const minW = GAP_MIN_W_CSS * pr;
    ctx.save();
    ctx.globalAlpha = 0.35;
    ctx.fillStyle = '#ff3b30';
    for (const r of rects) {
        const w = Math.max(r.x1 - r.x0, minW);
        ctx.fillRect(r.x0 - (w - (r.x1 - r.x0)) / 2, bb.top, w, bb.height);
    }
    ctx.globalAlpha = 0.9;
    ctx.strokeStyle = '#ff3b30';
    ctx.lineWidth = pr;
    for (const r of rects) {
        const w = Math.max(r.x1 - r.x0, minW);
        const dx = r.x0 - (w - (r.x1 - r.x0)) / 2;
        ctx.strokeRect(dx + pr / 2, bb.top + pr / 2, Math.max(w - pr, 1), Math.max(bb.height - pr, 1));
    }
    ctx.restore();
}

/// Индекс пропуска под курсором: координату считаем в тех же device-пикселях,
/// что и нарисованная подсветка (canvas.width/getBoundingClientRect().width —
/// это и есть pxRatio, так что любой CSS zoom/масштаб ОС не страшен), и берём
/// ближайшую полосу с допуском ±8 px. Без допуска суточный пропуск в 2 px
/// мышью не поймать: клик приходился рядом и «не работал».
function archOverviewGapIndexAt(u, clientX) {
    const rects = archOverviewGapRects(u);
    if (!rects.length) return -1;
    const cv = u.ctx.canvas;
    const cr = cv.getBoundingClientRect();
    if (!cr.width) return -1;
    const x = (clientX - cr.left) * (cv.width / cr.width);
    const tol = GAP_HIT_TOL_CSS * (uPlot.pxRatio || 1);
    let best = -1, bestD = Infinity;
    for (const r of rects) {
        const d = x < r.x0 ? r.x0 - x : (x > r.x1 ? x - r.x1 : 0);
        if (d <= tol && d < bestD) { bestD = d; best = r.i; }
    }
    return best;
}

/// Двойной клик по красной подсветке — выделить весь пропуск рамкой, чтобы
/// «⤓ Выбранное» сразу применило окно по гэпу. Слушатель вешается на box, а не
/// на over: uPlot пересоздаёт свой DOM при каждой перерисовке обзора.
function archOverviewWireDblClick(box) {
    if (box._ovlWired) return;
    box._ovlWired = true;
    box.addEventListener('dblclick', (e) => {
        const u = archOverviewChart;
        if (!u || !u.over || !archOverviewGaps.length) return;
        const i = archOverviewGapIndexAt(u, e.clientX);
        if (i < 0) return;
        const g = archOverviewGaps[i];
        const l = u.valToPos(g[0], 'x');
        const r2 = u.valToPos(g[1], 'x');
        // select.left/width/top/height — CSS-пиксели (рамка выделения в DOM),
        // поэтому здесь valToPos без can=true и u.height вместо bbox.height.
        u.setSelect({ left: l, width: Math.max(r2 - l, 2), top: 0, height: u.height }, true);
    });
    // Курсор-подсказка: там, где двойной клик выделяет пропуск, он «рука».
    box.addEventListener('mousemove', (e) => {
        const u = archOverviewChart;
        const on = !!u && archOverviewGaps.length > 0 && archOverviewGapIndexAt(u, e.clientX) >= 0;
        const c = on ? 'pointer' : '';
        if (box.style.cursor !== c) box.style.cursor = c;
    });
}

/// Подпись под заголовком обзора: сколько интервалов без данных.
function archOverviewUpdateSelHint() {
    const el = document.getElementById('charts-arch-overview-note');
    if (!el) return;
    const n = archOverviewGaps.length;
    el.textContent = n
        ? `Красным подсвечены интервалы без данных: ${n}. Двойной клик по красной полосе (можно рядом) — выделить пропуск.`
        : '';
}

/// Заголовок обзора: показываем те же границы, что грузятся на основных
/// графиках, — по ним же строится ось и подсветка пропусков.
function archOverviewSetTitle(startMs, endMs) {
    const el = document.getElementById('charts-arch-overview-title');
    if (!el) return;
    const spanH = (endMs - startMs) / 3600e3;
    el.textContent = `Обзор окна ${fmtArchTs(startMs)} — ${fmtArchTs(endMs)} ` +
        `(${spanH >= 1 ? spanH.toFixed(spanH >= 10 ? 0 : 1) + ' ч' : Math.round(spanH * 60) + ' мин'}, прорежено). ` +
        `Выделите мышью область и нажмите «⤓ Выбранное».`;
}

/// Подпись метки оси обзора: при сутках достаточно «ЧЧ:ММ», при большем
/// окне (если когда-нибудь расширим) — «ДД.МЧ».
function fmtShortTick(sec, spanSec) {
    const d = new Date(sec * 1000);
    const p = n => String(n).padStart(2, '0');
    if (spanSec > 48 * 3600) return `${p(d.getDate())}.${p(d.getMonth() + 1)}`;
    return `${p(d.getHours())}:${p(d.getMinutes())}`;
}

/// Левая граница выделения на обзоре -> «Начало» архивного окна.
/// В uPlot 1.6 выделение лежит в u.select (свойства u.selection нет —
/// из-за этого кнопка раньше ничего не делала).
function applyArchOverviewSelection() {
    const u = archOverviewChart;
    const sel = u && u.select;
    if (!sel || !(sel.width > 2)) {
        alertBox('Обзор', 'Сначала выделите область мышью на мини-графике обзора');
        return;
    }
    const t0 = u.posToVal(sel.left, 'x');
    const t1 = u.posToVal(sel.left + sel.width, 'x');
    if (t0 == null || t1 == null || isNaN(t0) || isNaN(t1) || !(t1 > t0)) return;
    setArchWindow(t0, t1);
    // Снимаем рамку выделения: окно уже перенесено в панель.
    u.setSelect({ left: 0, width: 0, top: 0, height: 0 }, false);
    reloadCharts(true);
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
                // Debounce: колесико генерирует десятки событий в секунду.
                // Собираем координаты и применяем setScale один раз с задержкой 150мс.
                let wheelTimer = null;
                let wheelDelta = 0;
                plot.addEventListener("wheel", e => {
                    e.preventDefault();
                    if (chartType === 'charts') userZoomedCharts = true;
                    else userZoomedBits = true;
                    wheelDelta += e.deltaY;
                    if (wheelTimer) clearTimeout(wheelTimer);
                    wheelTimer = setTimeout(() => {
                        wheelTimer = null;
                        if (Math.abs(wheelDelta) < 1) { wheelDelta = 0; return; }
                        let factor = 0.85;
                        let left = u.posToVal(e.clientX - plot.getBoundingClientRect().left, "x");
                        let xMin = u.scales.x.min, xMax = u.scales.x.max;
                        let delta = xMax - xMin;
                        let nextDelta = wheelDelta < 0 ? delta * factor : delta / factor;
                        if (nextDelta < 0.1) nextDelta = 0.1;
                        let ratio = (left - xMin) / delta;
                        u.setScale("x", { min: left - ratio * nextDelta, max: left + (1 - ratio) * nextDelta });
                        wheelDelta = 0;
                    }, 150);
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
    // Ищем окно графика: сброс масштаба должен вернуть ПОЛНЫЙ интервал последней
    // загрузки (span/archive), а не только авто-фит к уже суженным данным.
    const win = chartWindows.find(w => w.chart === u);
    if (win) {
        if (win.id === activeWindowId) userZoomedCharts = false;
        win.zoomed = false;
        // Полная перезагрузка для одного окна: reloadWindowRange не подходит,
        // он тянет видимый зум. Дёргаем reloadCharts(true) — он пересоздаст
        // все окна, но это единственный источник полного диапазона.
        reloadCharts(true);
        return;
    }
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
    // Окно, выбранное на мини-обзоре, передаём точными границами t0/t1 —
    // иначе оно округлялось бы до ближайшей «Длительности» из списка.
    if (archWindow) {
        return `/api/archive/${tagId}?t0=${archWindow.t0.toFixed(3)}&t1=${archWindow.t1.toFixed(3)}`;
    }
    const start = document.getElementById('charts-arch-start').value;
    const dur = document.getElementById('charts-arch-dur').value;
    return `/api/archive/${tagId}?start=${encodeURIComponent(start)}&duration=${encodeURIComponent(dur)}`;
}

/// URL batch-эндпоинта для всех тегов окна одновременно.
function getSeriesUrl(tags) {
    const mode = document.getElementById('charts-mode').value;
    const tagStr = tags.map(t => t.tag.id).join(',');
    if (mode === 'Live') {
        return `/api/series?tags=${encodeURIComponent(tagStr)}&span=${encodeURIComponent(document.getElementById('charts-span').value)}`;
    }
    if (archWindow) {
        return `/api/series?tags=${encodeURIComponent(tagStr)}&t0=${archWindow.t0.toFixed(3)}&t1=${archWindow.t1.toFixed(3)}`;
    }
    const start = document.getElementById('charts-arch-start').value;
    const dur = document.getElementById('charts-arch-dur').value;
    return `/api/series?tags=${encodeURIComponent(tagStr)}&start=${encodeURIComponent(start)}&duration=${encodeURIComponent(dur)}`;
}

/// Подгружает данные для нового видимого диапазона после зума/пана.
/// tMin/tMax — epoch-секунды (float), как их хранит uPlot при scale.time=true.
/// Сервер принимает их напрямую через t0/t1, минуя текстовые span/ISO-форматы.
/// Возвращает true, если данные обновлены.
async function reloadWindowRange(win, tMin, tMax) {
    if (!win.signals.length || !(tMax > tMin)) return false;
    const tagStr = win.signals.map(s => s.tag.id).join(',');
    const url = `/api/series?tags=${encodeURIComponent(tagStr)}&t0=${tMin.toFixed(3)}&t1=${tMax.toFixed(3)}`;
    try {
        const resp = await apiGet(url);
        if (!resp.series || !resp.series.length) return false;
        const byTag = new Map();
        for (const s of resp.series) byTag.set(s.tag_id, s);
        for (const sig of win.signals) {
            const s = byTag.get(sig.tag.id);
            if (s && s.times && s.times.length > 0) {
                Object.assign(sig, { times: s.times, values: s.values, t_start: tMin, t_end: tMax });
            }
        }
        return true;
    } catch (e) {
        return false;
    }
}

/// Debounce-таймер для ленивой подгрузки при зуме.
const zoomReloadTimer = new Map(); // winId -> setTimeout

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
        // Обзор архива строится по сигналам активного окна — перерисовываем.
        if (document.getElementById('charts-mode').value === 'Archive') loadArchOverview();
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
        // Live-тик не должен «затоптать» пользовательский зум: интервал в окне
        // фиксирован, его данные уже записаны в БД. Пересчёт выборки при смене
        // масштаба делает хук setScale; полный live-вид — по двойному клику
        // (reloadCharts(true)). Здесь просто пропускаем приближённое окно.
        if (!forceFit && win.zoomed) continue;
        // Batch-запрос на окно: сервер решает о прореживании ОДИН РАЗ и
        // возвращает выровненные серии. Иначе uPlot построит все сигналы по
        // оси X первого тега, и значения второго уедут при разном числе точек.
        if (win.signals.length > 0) {
            try {
                const resp = await apiGet(getSeriesUrl(win.signals));
                // t_last — момент последней записи во всей базе (сервер кладёт
                // его в каждый /api/series-ответ); подсказка в панели Archive.
                if (resp.t_last != null) {
                    archLastMs = Math.round(resp.t_last * 1000);
                    setLastDataHint('charts');
                    setLastDataHint('bits');
                }
                const byTag = new Map();
                for (const s of resp.series) byTag.set(s.tag_id, s);
                const tStart = resp.t_start;
                const tEnd = resp.t_end;
                for (const sig of win.signals) {
                    const s = byTag.get(sig.tag.id);
                    if (s && s.times && s.times.length > 0) {
                        Object.assign(sig, { times: s.times, values: s.values, t_start: tStart, t_end: tEnd });
                    } else {
                        emptySignals.push(sig.tag.name);
                    }
                }
            } catch (e) {
                if (isArchive && forceFit) {
                    for (const sig of win.signals) emptySignals.push(sig.tag.name);
                }
            }
        }
        rebuildChartWindow(win, forceFit);
    }
    // Обзор суток рисуется по тегам активного окна — пересобираем его при
    // явной загрузке архива (смена даты/режима/состава сигналов).
    if (isArchive && forceFit) loadArchOverview();
    else if (!isArchive) hideArchOverview();
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
        series, cursor: { drag: { x: true, y: false } },
        // ВАЖНО: значения opts.hooks uPlot вызывает как массивы (hooks[name].forEach),
        // в отличие от хуков плагина, которые он сам нормализует через concat. Голая
        // функция здесь роняет инициализацию графика (TypeError: forEach is not a
        // function) — канва остаётся пустой, сигнал на график не добавляется.
        hooks: {
            // Динамическая подгрузка при смене масштаба. uPlot 1.6 вызывает этот
            // хук сигнатурой (self, scaleKey) — границы читаем из u.scales.x.
            // Программные перерисовки (init, fit, live-тик, свой setData) помечены
            // win.suppressReload — на них НЕ реагируем. Реагируем только на
            // пользовательский wheel/drag-zoom, который сам ставит setScale.
            setScale: [(u, scaleKey) => {
                if (scaleKey !== 'x') return;
                const w = chartWindows.find(x => x.chart === u);
                if (!w || w.suppressReload) return;
                w.zoomed = true;
                const prev = zoomReloadTimer.get(w.id);
                if (prev) clearTimeout(prev);
                zoomReloadTimer.set(w.id, setTimeout(async () => {
                    zoomReloadTimer.delete(w.id);
                    // Данные могли уйти: окно удалено или пересоздано за debounce.
                    if (w.chart !== u || w.suppressReload || !w.signals.length) return;
                    const vm = u.scales.x.min, vx = u.scales.x.max;
                    if (await reloadWindowRange(w, vm, vx)) applySeriesData(w, u);
                }, 200));
            }]
        }
    };
    // На время конструирования гасим хук: uPlot на init сам дёргает setScale
    // (асинхронно, в microtask), и мы должны это проигнорировать.
    win.suppressReload = true;
    win.chart = new uPlot(opts, data, box);
    // Снимаем флаг уже ПОСЛЕ того, как отработает microtask инициализации uPlot.
    Promise.resolve().then(() => { if (win.chart) win.suppressReload = false; });
    bindLegendValues(win, win.chart);
    if (win.id === activeWindowId) { mainChart = win.chart; refreshHeaderCursors(); }
}

/// Перезаписать массивы графика без пересоздания uPlot и без сброса масштаба.
/// setData(data, false) сохраняет текущий zoomed-интервал по X.
function applySeriesData(win, u) {
    const newData = [win.signals[0].times];
    win.signals.forEach((s) => newData.push(s.values));
    win.suppressReload = true;
    u.setData(newData, false);
    Promise.resolve().then(() => { if (win.chart === u) win.suppressReload = false; });
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

// ---------------------------------------------------------------- экспорт PNG/PDF

// Метка времени для имён выгружаемых файлов: 20261102_143005.
// Функция вызывается из exportActiveChartPng — раньше её не было вовсе,
// и экспорт PNG падал с ReferenceError.
function exportStamp() {
    const d = new Date();
    const p = n => String(n).padStart(2, '0');
    return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}_${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`;
}

// Светлая тема для экспорта: белый фон, тёмные оси/сетка.
const LIGHT_THEME = { axis: '#303030', grid: '#E0E0E0', plotBg: '#FFFFFF' };

/** Захватить PNG графика win. lightMode=true → перерисовать в белой теме. */
function chartPngBlob(win, lightMode = false) {
    return new Promise(resolve => {
        const src = win.chart?.ctx?.canvas;
        if (!src || !src.width || !src.height) {
            resolve(null);
            return;
        }
        if (!lightMode) {
            // Текущая тема: берём canvas + фон из CSS-переменной
            const bg = getComputedStyle(document.body).getPropertyValue('--plot-bg').trim() || '#181818';
            const out = document.createElement('canvas');
            out.width = src.width;
            out.height = src.height;
            const ctx = out.getContext('2d');
            ctx.fillStyle = bg;
            ctx.fillRect(0, 0, out.width, out.height);
            ctx.drawImage(src, 0, 0);
            out.toBlob(b => resolve(b), 'image/png');
            return;
        }
        // Светлая тема: временно пересоздаём uPlot с белой темой
        const root = document.querySelector(`.chart-window[data-window-id="${win.id}"]`);
        const box = root?.querySelector('.chart-box');
        if (!box) { resolve(null); return; }

        // Сохраняем данные
        const baseTimes = win.signals[0].times;
        const data = [baseTimes];
        const series = [{ label: "Time" }];
        win.signals.forEach((s, idx) => {
            series.push({ label: s.tag.name, stroke: COLORS[idx % COLORS.length], width: 2, paths: uPlot.paths.stepped({ align: 1 }) });
            data.push(s.values);
        });
        const tMin = win.signals[0].t_start || baseTimes[0];
        const tMax = win.signals[0].t_end || baseTimes[baseTimes.length - 1];
        const origChart = win.chart;

        // Создаём временный график с белой темой
        const opts = {
            width: box.clientWidth || 1000, height: box.clientHeight || 400,
            scales: { x: { time: true, min: tMin, max: tMax } },
            plugins: [],
            axes: [
                { stroke: LIGHT_THEME.axis, grid: { stroke: LIGHT_THEME.grid } },
                { stroke: LIGHT_THEME.axis, grid: { stroke: LIGHT_THEME.grid } }
            ],
            series, cursor: false,
        };
        const tmpChart = new uPlot(opts, data, box);
        // Даем uPlot отрисовать
        requestAnimationFrame(() => {
            const tmpCanvas = tmpChart.ctx.canvas;
            const out = document.createElement('canvas');
            out.width = tmpCanvas.width;
            out.height = tmpCanvas.height;
            const ctx = out.getContext('2d');
            ctx.fillStyle = LIGHT_THEME.plotBg;
            ctx.fillRect(0, 0, out.width, out.height);
            ctx.drawImage(tmpCanvas, 0, 0);
            out.toBlob(b => {
                tmpChart.destroy();
                resolve(b);
            }, 'image/png');
        });
    });
}

// Экспорт активного окна графика в PNG (аналог «Сохранить как изображение»).
// Файл пишет сервер (POST /api/export/png), а не браузер: скачивание через
// <a download> в WebView2 оболочки pda-desktop молча не доходило до
// пользователя. Сервер отвечает реальным путём, и мы его показываем.
async function exportActiveChartPng() {
    const win = getActiveWindow();
    if (!win || !win.chart || !win.signals.length) {
        alertBox('Экспорт PNG', 'На активном графике нет сигналов');
        return;
    }
    try {
        const blob = await chartPngBlob(win, true); // light mode
        if (!blob) throw new Error('canvas не вернул изображение');
        const name = `pda_chart_${exportStamp()}.png`;
        const r = await fetch(`/api/export/png?name=${encodeURIComponent(name)}`, {
            method: 'POST', headers: { 'Content-Type': 'image/png' }, body: blob,
        });
        const text = await r.text();
        if (!r.ok) throw new Error(text || `HTTP ${r.status}`);
        const j = JSON.parse(text);
        alertBox('Экспорт PNG', `Сохранено (${Math.round(j.bytes / 1024)} КБ):\n${j.path}`);
    } catch (e) {
        alertBox('Экспорт PNG', `Не удалось сохранить: ${e.message}`, true);
    }
}

// Сбор легенды из DOM-элемента легенды окна.
function collectLegendRows(win) {
    const root = document.querySelector(`.chart-window[data-window-id="${win.id}"]`);
    const legendEl = root?.querySelector('.chart-legend');
    const rows = [];
    if (legendEl) {
        for (const row of legendEl.querySelectorAll('.legend-row')) {
            const nameEl = row.querySelector('.legend-name');
            const lineEl = row.querySelector('.legend-line');
            if (nameEl && lineEl) {
                rows.push({ name: nameEl.textContent, color: lineEl.style.backgroundColor });
            }
        }
    }
    return rows;
}

// Заголовок страницы PDF для окна графика: имя первого сигнала + счётчик
// остальных (окно графика не имеет собственного названия в UI).
function chartWindowTitle(win) {
    const first = win.signals[0]?.tag?.name;
    if (!first) return '';
    const extra = win.signals.length - 1;
    return extra > 0 ? `${first} +${extra}` : first;
}

// Экспорт PDF — ВСЕ окна графиков. Каждое окно — одна страница в PDF.
async function exportAllChartsPdf() {
    const wins = chartWindows.filter(w => w.chart && w.signals.length);
    if (!wins.length) {
        alertBox('Экспорт PDF', 'Нет графиков для экспорта');
        return;
    }
    const title = `PDA — ${wins.length} график(ов)`;
    try {
        // 1) Захватываем все окна в светлой теме
        const charts = [];
        for (const win of wins) {
            const pngBlob = await chartPngBlob(win, true); // light mode
            if (pngBlob) {
                charts.push({ win, pngBlob });
            }
        }
        if (!charts.length) throw new Error('Не удалось захватить графики');

        // 2) Отправляем на сервер: chart_i.png, title_i, legend_name_i[]/legend_color_i[]
        const formData = new FormData();
        formData.append('title', title);
        for (let i = 0; i < charts.length; i++) {
            const { win, pngBlob } = charts[i];
            formData.append(`chart_${i}`, pngBlob, `chart_${i}.png`);
            formData.append(`title_${i}`, chartWindowTitle(win));
            for (const row of collectLegendRows(win)) {
                formData.append(`legend_name_${i}[]`, row.name);
                formData.append(`legend_color_${i}[]`, row.color);
            }
        }

        // 3) Запрашиваем PDF
        const r = await fetch('/api/export/pdf?count=' + charts.length, {
            method: 'POST', body: formData,
        });
        const text = await r.text();
        if (!r.ok) throw new Error(text || `HTTP ${r.status}`);
        const j = JSON.parse(text);
        alertBox('Экспорт PDF', `Сохранено (${Math.round(j.bytes / 1024)} КБ):\n${j.path}`);
    } catch (e) {
        alertBox('Экспорт PDF', `Не удалось сохранить: ${e.message}`, true);
    }
}

// ---------------------------------------------------------------- BITS
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
// Режим опроса: true — модули опрашиваются, false — «только просмотр».
let pollingOn = true;

function updateViewOnlyButton(running) {
    const btn = document.getElementById('btn-view-only');
    if (!btn) return;
    btn.textContent = pollingOn ? '⏸ Опрос' : '▶ Опрос';
    btn.classList.toggle('btn-add', !pollingOn);
    btn.classList.toggle('btn-action', pollingOn);
    btn.title = pollingOn
        ? 'Остановить опрос всех модулей — приложение останется только для просмотра истории'
        : `Опрос остановлен (только просмотр). Модулей запущено: ${running ?? 0}`;
}

// Остановить/возобновить опрос. Остановка освобождает порты (в т.ч. Modbus
// 502), поэтому рядом можно запустить вторую копию «только для просмотра».
async function toggleViewOnly() {
    const stop = pollingOn;
    if (stop) {
        const ok = await confirmBox(
            'Остановить опрос всех модулей?\n\nОпрос ПЛК прекратится, порты освободятся, история и графики останутся доступны. Так можно запустить вторую копию приложения только для просмотра.',
            { yes: 'Остановить опрос', no: 'Отмена' });
        if (ok !== true) return;
    }
    try {
        const r = await apiPost('/api/polling', { enabled: !stop });
        pollingOn = !!r.polling;
        updateViewOnlyButton(r.running);
        if (typeof ioRefreshAll === 'function') ioRefreshAll();
        alertBox('Режим опроса', pollingOn
            ? `Опрос включён, модулей запущено: ${r.running ?? 0}`
            : 'Опрос остановлен — режим «только просмотр»');
    } catch (e) {
        alertBox('Режим опроса', `Не удалось переключить режим: ${e.message}`, true);
    }
}

async function refreshAppBadge() {
    try {
        const st = await apiGet('/api/status');
        const a = st.app || {};
        const el = document.getElementById('app-badge');
        el.title = `${st.running || 0} модулей работает`;
        el.textContent = `${a.name || 'PDA'} ${a.version || ''}`;
        pollingOn = !st.view_only;
        updateViewOnlyButton(st.running || 0);
    } catch (e) { /* сервер ещё не поднялся */ }
}

// ---------------------------------------------------------------- modal utils
// Одна общая подложка для ФОРМ (io.js/db.js строят формы через openModal);
// сообщения и подтверждения — на отдельных подложках (modal-alert,
// modal-confirm), иначе ОК в алерте затирал бы открытую под ним форму.
function closeModal() {
    const ov = document.getElementById('modal-overlay');
    ov.style.display = 'none';
    document.getElementById('modal-box').innerHTML = '';
}
function isFormOpen() {
    return document.getElementById('modal-overlay').style.display !== 'none';
}
function openModal(html, onReady) {
    const box = document.getElementById('modal-box');
    box.innerHTML = html;
    document.getElementById('modal-overlay').style.display = 'flex';
    if (onReady) onReady(box);
    // Escape закрывает только если фокус внутри модалки
    box.querySelectorAll('input,select').forEach(el => el.addEventListener('keydown', e => { if (e.key === 'Escape') closeModal(); }));
}

// Закрытие по клику вне формы — только если и нажатие, и отпускание были на
// затемнении. Иначе любой «лишний» клик (WebView2 отдаёт click элементу под
// закрывающейся подсказкой/селектом, либо пользователь дотащил выделение текста
// за край формы) мгновенно закрывал форму и терял введённые данные.
let modalDownOnOverlay = false;
document.getElementById('modal-overlay').addEventListener('mousedown', e => {
    modalDownOnOverlay = e.target.id === 'modal-overlay';
});
document.getElementById('modal-overlay').addEventListener('click', e => {
    const hit = e.target.id === 'modal-overlay';
    if (hit && modalDownOnOverlay) closeModal();
    modalDownOnOverlay = false;
});
document.getElementById('modal-box').addEventListener('click', e => e.stopPropagation());

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

// Простое сообщение (аналог QMessageBox.information/critical).
// Своя подложка: сообщение может появиться поверх открытой формы (например
// «Сохранено» или текст ошибки) — форма под ним обязана остаться живой.
function alertBox(title, message, isError = false) {
    document.getElementById('alert-box').innerHTML = `<h3>${escHtml(title)}</h3>
        <div class="confirm-text" style="color:${isError ? 'var(--error)' : 'var(--text)'}">${escHtml(message)}</div>
        <div class="confirm-btns"><button class="btn-action" onclick="closeAlertBox()">ОК</button></div>`;
    document.getElementById('modal-alert').style.display = 'flex';
}
function closeAlertBox() {
    document.getElementById('modal-alert').style.display = 'none';
    document.getElementById('alert-box').innerHTML = '';
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
    // Шапка окна должна соответствовать теме, выбранной в прошлый раз.
    syncWindowTheme();
    initDateTimeInputs();
    if (bitsByteSwap) updateBitsSwapButton();
    // Очистка автозаполнения WebView2: если браузер подставил что-то в поле
    // поиска (например, логин из вкладки Database), сбрасываем.
    const ioSearch = document.getElementById('io-search');
    if (ioSearch) ioSearch.value = '';
    await fetchTags();
    addChartWindow();
    connectLiveWs();
    refreshAppBadge();
    // Справочник драйверов нужен IO-вкладке сразу (конструкторы адресов)
    try { driverTypes = (await apiGet('/api/drivers/info')).types || []; } catch (e) { driverTypes = []; }
})();
