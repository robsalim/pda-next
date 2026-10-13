// Вкладка I/O: дерево модулей/групп, таблица тегов (live), диалоги подключения
// и переменной с конструкторами адресов, экспорт/импорт дерева и CSV.
// Поведение перенесено из python_app/ui/io_widget.py.

let ioConns = [];            // /api/connections
let ioStatus = {};           // drivers_json по conn_id
let ioGroups = [];           // группы выбранного подключения [{group,tags}]
let ioTags = [];             // теги выбранного подключения
let ioSelConn = null;        // id выбранного подключения
let ioSelGroup = null;       // имя выбранной группы
let ioSelTagId = null;       // id выбранного тега (для «Изменить»)
let ioChecked = new Set();   // отмеченные строки (массовое удаление)

function ioStatusLine(text, isError = false) {
    const el = document.getElementById('io-status');
    if (!el) return;
    el.textContent = text || '';
    el.style.color = isError ? 'var(--error)' : 'var(--text-faint)';
}

// ------------------------------------------------------------------ загрузка
async function ioRefreshAll() {
    const el = document.getElementById('io-status');
    if (!el) return; // страница ещё не открыта
    try {
        ioConns = await apiGet('/api/connections');
        const st = await apiGet('/api/status');
        ioStatus = {};
        (st.drivers || []).forEach(d => { ioStatus[d.conn_id] = d; });
    } catch (e) {
        ioStatusLine(`Ошибка загрузки: ${e.message}`, true);
        return;
    }
    // Сбрасываем выбор, если подключение исчезло
    if (ioSelConn == null || !ioConns.some(c => c.id === ioSelConn)) {
        ioSelConn = ioConns.length ? ioConns[0].id : null;
        ioSelGroup = null;
    }
    if (ioSelConn != null) await ioSelectConn(ioSelConn);
    else { ioGroups = []; ioTags = []; ioRenderTree(); ioRenderTags(); }
    await fetchTags(); // обновляем общее дерево сигналов/Bit
}

function ioOnTagsChanged() { /* live-обновление таблицы делает onLiveUpdate */ }

async function ioSelectConn(connId) {
    ioSelConn = connId;
    const conn = ioConns.find(c => c.id === connId);
    try {
        const g = await apiGet(`/api/connections/${connId}/groups`);
        ioGroups = g.groups || [];
    } catch (e) { ioGroups = []; }
    try { ioTags = await apiGet(`/api/connections/${connId}/tags`); }
    catch (e) { ioTags = []; }
    if (ioSelGroup && !ioGroups.includes(ioSelGroup)) ioSelGroup = null;
    document.getElementById('io-conn-title').textContent = conn ? `Переменные — ${conn.name}` : 'Переменные';
    ioRenderTree();
    ioRenderTags();
}

// ------------------------------------------------------------------ дерево
function ioDotClass(connId) {
    const d = ioStatus[connId];
    if (!d) return '';
    const s = (d.status || '').toLowerCase();
    if (s.startsWith('error') || s.includes('failed') || s.includes('connect error') || s.includes('poll error')) return 'err';
    if (d.running && (s.startsWith('running') || s.startsWith('connected') || s.startsWith('polling') || s.startsWith('listen'))) return 'ok';
    return 'run';
}

function ioRenderTree() {
    const root = document.getElementById('io-tree');
    if (!ioConns.length) { root.innerHTML = `<div class="tree-empty">Нет модулей</div>`; return; }
    let html = '';
    for (const c of ioConns) {
        const selConn = c.id === ioSelConn ? ' selected' : '';
        html += `<div class="io-conn${selConn}" onclick="ioSelectConn(${c.id})">
            <span class="dot ${ioDotClass(c.id)}"></span>
            <span class="io-conn-name">🔌 ${escHtml(c.name)}</span>
            <span class="io-drv">${escHtml(c.driver_type)}${c.enabled ? '' : ' · выкл'}</span>
        </div>`;
        if (c.id !== ioSelConn) continue;
        const groups = [...new Set([...ioGroups, ...ioTags.map(t => t.group_name || 'Общие')])].sort();
        for (const g of groups) {
            const n = ioTags.filter(t => (t.group_name || 'Общие') === g).length;
            const selG = g === ioSelGroup ? ' selected' : '';
            html += `<div class="io-group${selG}" onclick="ioSelectGroup('${escAttr(g)}')">📁 ${escHtml(g)} <span class="io-gcount">(${n})</span></div>`;
        }
    }
    root.innerHTML = html;
}
function escAttr(s) { return String(s).replace(/'/g, "\\'").replace(/"/g, '&quot;'); }

function ioSelectGroup(g) {
    ioSelGroup = ioSelGroup === g ? null : g;
    ioRenderTree();
    ioRenderTags();
}

// ------------------------------------------------------------------ таблица
const IO_COLS = ['ID', 'Name', 'Группа', 'Address', 'Type', 'Scale', 'Offset', 'Unit', 'Value', 'Time'];
let ioSortCol = null, ioSortDir = 1;

function ioRenderTags() {
    const table = document.getElementById('io-tag-table');
    let tags = ioTags.slice();
    if (ioSelGroup) tags = tags.filter(t => (t.group_name || 'Общие') === ioSelGroup);
    const q = (document.getElementById('io-search').value || '').trim().toLowerCase();
    if (q) tags = tags.filter(t =>
        (t.name || '').toLowerCase().includes(q) ||
        (t.address_str || '').toLowerCase().includes(q) ||
        (t.group_name || '').toLowerCase().includes(q));
    if (ioSortCol != null) {
        tags.sort((a, b) => String(a[ioSortCol] ?? '').localeCompare(String(b[ioSortCol] ?? ''), undefined, { numeric: true }) * ioSortDir);
    }

    let html = `<thead><tr>` + IO_COLS.map(c => `<th onclick="ioSort('${c}')">${c}${ioSortCol === c ? (ioSortDir > 0 ? ' ▲' : ' ▼') : ''}</th>`).join('') + `</tr></thead><tbody>`;
    for (const t of tags) {
        const lv = liveRows.get(t.id);
        const checked = ioChecked.has(t.id) ? ' checked' : '';
        const sel = t.id === ioSelTagId ? ' selected' : '';
        const valCell = lv
            ? `<span class="${lv.quality === 0 ? 'v-good' : 'v-bad'}">${fmtLiveVal(lv.value)}</span>`
            : `<span style="color:var(--text-faint)">—</span>`;
        const timeCell = lv && lv.timestamp ? formatTime(lv.timestamp) : '';
        html += `<tr class="${sel.trim()}" ondblclick="ioEditTag(${t.id})">
            <td>${t.id}</td>
            <td><input type="checkbox"${checked} onclick="ioToggleCheck(${t.id}, this.checked); event.stopPropagation();"> ${escHtml(t.name)}</td>
            <td>${escHtml(t.group_name || 'Общие')}</td>
            <td>${escHtml(t.address_str)}</td>
            <td>${escHtml(t.data_type)}</td>
            <td>${t.scale}</td><td>${t.offset_val}</td><td>${escHtml(t.unit || '')}</td>
            <td>${valCell}</td><td>${timeCell}</td></tr>`;
    }
    html += `</tbody>`;
    table.innerHTML = html;
    document.getElementById('io-tag-count').textContent = `${tags.length} из ${ioTags.length}`;
}

function ioSort(col) {
    if (ioSortCol === col) ioSortDir = -ioSortDir;
    else { ioSortCol = col; ioSortDir = 1; }
    ioRenderTags();
}
function ioToggleCheck(id, on) { on ? ioChecked.add(id) : ioChecked.delete(id); }
function fmtLiveVal(v) {
    if (v === null || v === undefined || isNaN(v)) return '';
    if (Math.abs(v) >= 1 && Math.abs(v - Math.round(v)) < 1e-6) return String(Math.round(v));
    return String(parseFloat(v.toFixed(3)));
}

// live-обновление только значений в таблице (без полной перерисовки DOM)
onLiveUpdate(() => {
    if (activeTab !== 'io') return;
    ioRenderTags();
    ioRenderTree(); // статусы драйверов могли измениться
});

// ------------------------------------------------------------------ модуль
function ioConnForm(conn) {
    const c = conn || {};
    const cfg = c.config || {};
    const dt = c.driver_type || 'snap7';
    return `<h3>${conn ? 'Редактирование подключения' : 'Новое подключение'}</h3>
        <div class="modal-form-row"><label>Имя модуля:</label><input id="c-name" type="text" value="${escAttr(c.name || 'New module')}"></div>
        <div class="modal-form-row"><label>Тип драйвера:</label><select id="c-driver" onchange="ioConnDriverChanged()">${
            driverTypes.map(t => `<option value="${t.key}"${t.key === dt ? ' selected' : ''}>${escHtml(t.label)}</option>`).join('')
        }</select></div>
        <div class="modal-form-row"><label>IP-адрес контроллера:</label><input id="c-ip" type="text" value="${escAttr(cfg.ip || '192.168.0.1')}"></div>
        <div class="modal-form-row"><label>TCP Порт:</label><input id="c-port" type="number" value="${cfg.port ?? 102}"></div>
        <div class="modal-form-row" id="c-row-rack"><label>Стойка (Rack):</label><input id="c-rack" type="number" min="0" max="15" value="${cfg.rack ?? 0}"></div>
        <div class="modal-form-row" id="c-row-slot"><label>Слот ЦПУ (Slot):</label><input id="c-slot" type="number" min="0" max="15" value="${cfg.slot ?? 1}"></div>
        <div class="modal-form-row" id="c-row-slave"><label>Slave ID:</label><input id="c-slave" type="number" min="1" max="255" value="${cfg.slave_id ?? 1}"></div>
        <div class="modal-form-row"><label>Период опроса (мс):</label><input id="c-poll" type="number" min="1" max="60000" step="5" value="${c.poll_interval_ms ?? 100}"></div>
        <div class="modal-form-row"><label>Включено:</label><input id="c-enabled" type="checkbox"${conn ? (c.enabled ? ' checked' : '') : ' checked'} style="flex:0"></div>
        <div class="modal-btns">
            <button class="btn-del" onclick="closeModal()">Отмена</button>
            <button class="btn-action" onclick="ioConnSave(${conn ? conn.id : 0})">Сохранить</button>
        </div>`;
}

function isSnap7(key) { return key === 'snap7' || key === 'snap7_s200'; }
function ioConnDriverChanged() {
    const dt = document.getElementById('c-driver').value;
    const isS7 = isSnap7(dt);
    document.getElementById('c-row-rack').style.display = isS7 ? '' : 'none';
    document.getElementById('c-row-slot').style.display = isS7 ? '' : 'none';
    document.getElementById('c-row-slave').style.display = dt === 'modbus_client' ? '' : 'none';
    document.getElementById('c-port').value = isS7 ? 102 : 502;
}

function ioAddConn() { openModal(ioConnForm(null), () => ioConnDriverChanged()); }
function ioEditConn() {
    const c = ioConns.find(x => x.id === ioSelConn);
    if (!c) return ioStatusLine('Сначала выберите модуль', true);
    openModal(ioConnForm(c), () => ioConnDriverChanged());
}

async function ioConnSave(id) {
    const dt = document.getElementById('c-driver').value;
    const config = { ip: document.getElementById('c-ip').value.trim(), port: Number(document.getElementById('c-port').value) };
    if (isSnap7(dt)) { config.rack = Number(document.getElementById('c-rack').value); config.slot = Number(document.getElementById('c-slot').value); }
    else if (dt === 'modbus_client') { config.slave_id = Number(document.getElementById('c-slave').value); }
    const body = {
        name: document.getElementById('c-name').value.trim() || 'Connection',
        driver_type: dt,
        enabled: document.getElementById('c-enabled').checked,
        poll_interval_ms: Number(document.getElementById('c-poll').value),
        config,
    };
    try {
        if (id) await apiPut(`/api/connections/${id}`, body);
        else await apiPost('/api/connections', body);
        closeModal();
        ioStatusLine('Модуль сохранён, стек перезапущен');
        await ioRefreshAll();
    } catch (e) { ioStatusLine(`Ошибка: ${e.message}`, true); }
}

async function ioDelConn() {
    if (!ioSelConn) return ioStatusLine('Сначала выберите модуль', true);
    const c = ioConns.find(x => x.id === ioSelConn);
    if (await confirmBox(`Удалить модуль «${c.name}» со всеми сигналами?`) !== true) return;
    try { await apiDel(`/api/connections/${ioSelConn}`); ioSelGroup = null; await ioRefreshAll(); }
    catch (e) { ioStatusLine(`Ошибка: ${e.message}`, true); }
}

// ------------------------------------------------------------------ группы
function ioAddGroup() {
    if (!ioSelConn) return ioStatusLine('Сначала выберите модуль', true);
    openModal(`<h3>Новая группа</h3>
        <div class="modal-form-row"><label>Название группы:</label><input id="g-name" type="text" value=""></div>
        <div class="modal-btns"><button class="btn-del" onclick="closeModal()">Отмена</button>
        <button class="btn-action" onclick="ioGroupSave()">Создать</button></div>`);
}
async function ioGroupSave() {
    const name = document.getElementById('g-name').value.trim();
    if (!name) return;
    try { await apiPost(`/api/connections/${ioSelConn}/groups`, { name }); closeModal(); await ioSelectConn(ioSelConn); }
    catch (e) { ioStatusLine(`Ошибка: ${e.message}`, true); }
}
function ioRenameGroup() {
    if (!ioSelGroup) return ioStatusLine('Выберите группу в дереве', true);
    openModal(`<h3>Переименовать группу</h3>
        <div class="modal-form-row"><label>Новое название:</label><input id="g-new" type="text" value="${escAttr(ioSelGroup)}"></div>
        <div class="modal-btns"><button class="btn-del" onclick="closeModal()">Отмена</button>
        <button class="btn-action" onclick="ioGroupRename()">Применить</button></div>`);
}
async function ioGroupRename() {
    const name = document.getElementById('g-new').value.trim();
    if (!name) return;
    try { await apiPost(`/api/connections/${ioSelConn}/groups/rename`, { old: ioSelGroup, new: name }); closeModal(); ioSelGroup = name; await ioSelectConn(ioSelConn); }
    catch (e) { ioStatusLine(`Ошибка: ${e.message}`, true); }
}
async function ioDelGroup() {
    if (!ioSelGroup) return ioStatusLine('Выберите группу в дереве', true);
    const withTags = await confirmBox(`Удалить группу «${ioSelGroup}» вместе с сигналами?`, { yes: 'С сигналами', no: 'Только группу', cancel: 'Отмена' });
    if (withTags === null) return;
    try { await apiPost(`/api/connections/${ioSelConn}/groups/delete`, { name: ioSelGroup, with_tags: withTags === true }); ioSelGroup = null; await ioSelectConn(ioSelConn); await fetchTags(); }
    catch (e) { ioStatusLine(`Ошибка: ${e.message}`, true); }
}

// ------------------------------------------------------------------ переменная
function driverInfo(key) { return driverTypes.find(t => t.key === key) || {}; }

const BASE_TYPES = ['FLOAT', 'INT16', 'UINT16', 'BOOL'];
const S7_EXTRA = ['BYTE', 'DWORD'];
const MODBUS_TYPES = ['INT16', 'UINT16', 'FLOAT', 'BOOL'];
const SIZE_TYPES = { B: ['BYTE'], W: ['INT16', 'UINT16'], D: ['FLOAT', 'DWORD'], X: ['BOOL'] };
const SIZE_BY_TYPE = { BOOL: 'X', BYTE: 'B', FLOAT: 'D', INT16: 'W', UINT16: 'W', DWORD: 'D' };

// Парсеры адреса (обратный разбор для конструктора).
function parseS7(address, s200) {
    let m = /^DB(\d+)\.DB([BWDX])(\d+)(?:\.(\d+))?$/i.exec(address.trim());
    if (m) return { area: 'DB', db: +m[1], size: m[2].toUpperCase(), offset: +m[3], bit: +(m[4] ?? 0) };
    // V100.3 / V-память SMART
    m = /^V([BWDX])?(\d+)(?:\.(\d+))?$/i.exec(address.trim());
    if (m && s200) return { area: 'V', db: 1, size: (m[1] || 'B').toUpperCase(), offset: +m[2], bit: +(m[3] ?? 0) };
    m = /^([MIQ])([BWDX]?)(\d+)(?:\.(\d+))?$/i.exec(address.trim());
    if (m) return { area: m[1].toUpperCase(), db: 1, size: (m[2] || 'B').toUpperCase(), offset: +m[3], bit: +(m[4] ?? 0) };
    return null;
}
function buildS7(p) {
    if (p.area === 'DB') return `DB${p.db}.DB${p.size}${p.offset}` + (p.size === 'X' ? `.${p.bit}` : '');
    if (p.size === 'X') return `${p.area}${p.offset}.${p.bit}`;   // M3.4, I0.1, V100.3
    return `${p.area}${p.size}${p.offset}`;
}
function parseModbus(address) {
    const a = address.trim().toUpperCase();
    let m = /^(HR|IR|C|DI)(\d+)$/.exec(a);
    if (m) return { area: m[1], offset: +m[2] };
    if (/^\d+$/.test(a)) return { area: 'HR', offset: +a };
    return null;
}
function buildModbus(p) { return p.area === 'HR' ? String(p.offset) : `${p.area}${p.offset}`; }

function ioTagForm(tag, conn) {
    const t = tag || {};
    const dtKey = conn.driver_type;
    const info = driverInfo(dtKey);
    const addr = info.address || {};
    const areas = addr.areas || [];
    const isS7 = isSnap7(dtKey), isMb = (dtKey === 'modbus_client' || dtKey === 'modbus_server');
    let types = BASE_TYPES.concat(isS7 ? S7_EXTRA : []);
    if (isMb) types = MODBUS_TYPES;
    if (t.data_type && !types.includes(t.data_type)) types.unshift(t.data_type);
    const curType = t.data_type || types[0];

    const groups = [...new Set([...ioGroups, ...ioTags.map(x => x.group_name || 'Общие'), 'Общие'])].sort();

    let html = `<h3>${tag ? 'Редактирование переменной' : 'Создать переменную'}</h3>
        <div class="modal-form-row"><label>Имя (Name):</label><input id="t-name" type="text" value="${escAttr(t.name || 'New_Signal')}"></div>
        <div class="modal-form-row"><label>Группа:</label><select id="t-group">${groups.map(g => `<option${g === (t.group_name || 'Общие') ? ' selected' : ''}>${escHtml(g)}</option>`).join('')}</select></div>
        <div class="modal-form-row"><label>Адрес:</label><input id="t-addr" type="text" value="${escAttr(t.address_str ?? '0')}" oninput="ioTagAddrChanged()" placeholder="${escAttr(addr.hint || '')}"></div>`;

    if (isS7) {
        const p = parseS7(t.address_str ?? 'MW0', dtKey === 'snap7_s200') || { area: areas[0]?.[0] || 'M', db: 1, size: 'W', offset: 0, bit: 0 };
        html += `<div class="modal-addr-box"><div class="addr-title">🧩 Конструктор S7</div>
            <div class="modal-form-row"><label>Область:</label><select id="s-area" onchange="ioS7Changed()">${areas.map(a => `<option value="${a[0]}"${a[0] === p.area ? ' selected' : ''}>${escHtml(a[1])}</option>`).join('')}</select></div>
            <div class="modal-form-row" id="s-row-db"><label>Номер DB:</label><input id="s-db" type="number" min="1" max="9999" value="${p.db}" oninput="ioS7Changed()"></div>
            <div class="modal-form-row"><label>Размер:</label><select id="s-size" onchange="ioS7Changed()">${['B','W','D','X'].map(s => `<option value="${s}"${s === p.size ? ' selected' : ''}>${s}</option>`).join('')}</select></div>
            <div class="modal-form-row"><label>Смещение:</label><input id="s-off" type="number" min="0" max="65535" value="${p.offset}" oninput="ioS7Changed()"></div>
            <div class="modal-form-row" id="s-row-bit"><label>Бит:</label><select id="s-bit" onchange="ioS7Changed()">${[0,1,2,3,4,5,6,7].map(b => `<option${b === p.bit ? ' selected' : ''}>${b}</option>`).join('')}</select></div>
        </div>`;
    } else if (isMb) {
        const p = parseModbus(t.address_str ?? '0') || { area: 'HR', offset: 0 };
        html += `<div class="modal-addr-box"><div class="addr-title">🧩 Конструктор Modbus</div>
            <div class="modal-form-row"><label>Область:</label><select id="m-area" onchange="ioMbChanged()">${areas.map(a => `<option value="${a[0]}"${a[0] === p.area ? ' selected' : ''}>${escHtml(a[1])}</option>`).join('')}</select></div>
            <div class="modal-form-row"><label>Номер:</label><input id="m-off" type="number" min="0" max="65535" value="${p.offset}" oninput="ioMbChanged()"></div>
        </div>`;
    }

    html += `<div id="t-hint" class="modal-hint"></div>
        <div class="modal-form-row"><label>Тип данных:</label><select id="t-type" onchange="ioTagTypeChanged()">${types.map(x => `<option${x === curType ? ' selected' : ''}>${x}</option>`).join('')}</select></div>
        <div class="modal-form-row"><label>Масштаб (Scale):</label><input id="t-scale" type="number" step="0.0001" value="${t.scale ?? 1.0}"></div>
        <div class="modal-form-row"><label>Смещение (Offset):</label><input id="t-offset" type="number" step="0.01" value="${t.offset_val ?? 0.0}"></div>
        <div class="modal-form-row"><label>Ед. изм.:</label><input id="t-unit" type="text" value="${escAttr(t.unit || '')}"></div>
        <div class="modal-form-row"><label>Период записи (мс):</label><input id="t-log" type="number" min="0" step="50" value="${t.log_interval_ms ?? 1000}"></div>
        <div class="modal-form-row"><label>Мёртвая зона:</label><input id="t-dead" type="number" step="0.001" value="${t.deadband ?? 0.0}"></div>
        <div class="modal-btns"><button class="btn-del" onclick="closeModal()">Отмена</button>
        <button class="btn-action" onclick="ioTagSave(${tag ? tag.id : 0}, ${conn.id})">Сохранить</button></div>`;
    return html;
}

function ioTagOpen(tag) {
    const conn = ioConns.find(c => c.id === ioSelConn);
    if (!conn) return ioStatusLine('Сначала выберите модуль', true);
    openModal(ioTagForm(tag, conn), () => { ioTagAddrValidate(); ioS7Gating(); });
}
function ioAddTag() { ioTagOpen(null); }
function ioEditTag(id) {
    if (id == null) id = ioSelTagId;
    const t = ioTags.find(x => x.id === (id != null ? id : ioSelTagId));
    if (!t) return ioStatusLine('Выберите переменную', true);
    ioSelTagId = t.id;
    ioTagOpen(t);
}

function ioS7Gating() {
    const area = document.getElementById('s-area'); if (!area) return;
    document.getElementById('s-row-db').style.display = area.value === 'DB' ? '' : 'none';
    document.getElementById('s-row-bit').style.display = document.getElementById('s-size').value === 'X' ? '' : 'none';
}
function ioS7Changed() {
    const p = { area: document.getElementById('s-area').value, db: Number(document.getElementById('s-db').value) || 1,
        size: document.getElementById('s-size').value, offset: Number(document.getElementById('s-off').value),
        bit: Number(document.getElementById('s-bit').value) };
    document.getElementById('t-addr').value = buildS7(p);
    const allow = SIZE_TYPES[p.size];
    if (allow) ioRestrictTypes(allow);
    ioS7Gating();
    ioTagAddrValidate();
}
function ioMbChanged() {
    const p = { area: document.getElementById('m-area').value, offset: Number(document.getElementById('m-off').value) };
    document.getElementById('t-addr').value = buildModbus(p);
    ioTagAddrValidate();
}
function ioTagAddrChanged() {
    // Ручной ввод -> обратно в конструктор, не пересобирая поле адреса
    const dtKey = (ioConns.find(c => c.id === ioSelConn) || {}).driver_type;
    const a = document.getElementById('t-addr').value;
    if (isSnap7(dtKey)) {
        const p = parseS7(a, dtKey === 'snap7_s200');
        if (p) {
            ioSetSelect('s-area', p.area); ioSetVal('s-db', p.db); ioSetSelect('s-size', p.size);
            ioSetVal('s-off', p.offset); ioSetSelect('s-bit', String(p.bit));
            const allow = SIZE_TYPES[p.size]; if (allow) ioRestrictTypes(allow);
            ioS7Gating();
        }
    } else if (dtKey === 'modbus_client' || dtKey === 'modbus_server') {
        const p = parseModbus(a); if (p) { ioSetSelect('m-area', p.area); ioSetVal('m-off', p.offset); }
    }
    ioTagAddrValidate();
}
function ioSetSelect(id, v) { const el = document.getElementById(id); if (el) [...el.options].forEach(o => o.selected = o.value === v); }
function ioSetVal(id, v) { const el = document.getElementById(id); if (el) el.value = v; }
function ioRestrictTypes(allow) {
    const sel = document.getElementById('t-type'); if (!sel) return;
    const keep = sel.value;
    const cur = [...sel.options].map(o => o.value);
    if (cur.join() !== allow.join()) {
        sel.innerHTML = allow.map(x => `<option>${x}</option>`).join('');
    }
    if (allow.includes(keep)) sel.value = keep;
}
function ioTagTypeChanged() {
    const dtKey = (ioConns.find(c => c.id === ioSelConn) || {}).driver_type;
    if (isSnap7(dtKey)) {
        const want = SIZE_BY_TYPE[document.getElementById('t-type').value];
        if (want && document.getElementById('s-size')) { ioSetSelect('s-size', want); ioS7Changed(); return; }
    }
    ioTagAddrValidate();
}

async function ioTagAddrValidate() {
    const hint = document.getElementById('t-hint'); if (!hint) return;
    const conn = ioConns.find(c => c.id === ioSelConn) || {};
    const address = document.getElementById('t-addr').value.trim();
    const dataType = document.getElementById('t-type').value;
    try {
        const r = await apiPost('/api/validate/address', { driver_type: conn.driver_type, address, data_type: dataType });
        hint.className = 'modal-hint ' + (r.ok ? 'ok' : 'error');
        hint.textContent = r.ok ? '✓ Адрес разобран корректно' : ('⚠ ' + (r.error || 'некорректный адрес'));
        ioHint._ok = r.ok;
    } catch (e) { ioHint._ok = undefined; }
}
const ioHint = {};

async function ioTagSave(id, connId) {
    if (ioHint._ok === false) { ioStatusLine('Адрес некорректен — сохранение отменено', true); return; }
    const body = {
        id, connection_id: connId,
        name: document.getElementById('t-name').value.trim() || 'Unnamed',
        address_str: document.getElementById('t-addr').value.trim() || '0',
        data_type: document.getElementById('t-type').value,
        scale: Number(document.getElementById('t-scale').value),
        offset_val: Number(document.getElementById('t-offset').value),
        unit: document.getElementById('t-unit').value.trim(),
        group_name: document.getElementById('t-group').value.trim() || 'Общие',
        log_interval_ms: Number(document.getElementById('t-log').value),
        deadband: Number(document.getElementById('t-dead').value),
    };
    try {
        if (id) await apiPut(`/api/tags/${id}`, body);
        else await apiPost('/api/tags', body);
        closeModal();
        await ioSelectConn(connId); await fetchTags();
    } catch (e) { ioStatusLine(`Ошибка: ${e.message}`, true); }
}

async function ioDelTags() {
    const ids = [...ioChecked];
    if (!ids.length && ioSelTagId != null) ids.push(ioSelTagId);
    if (!ids.length) return ioStatusLine('Отметьте переменные для удаления', true);
    if (await confirmBox(`Удалить ${ids.length} переменн. (сигналы)?`) !== true) return;
    try { await apiPost('/api/tags/delete', { ids }); ioChecked.clear(); ioSelTagId = null; await ioSelectConn(ioSelConn); await fetchTags(); }
    catch (e) { ioStatusLine(`Ошибка: ${e.message}`, true); }
}

// ------------------------------------------------------------------ экспорт/импорт дерева
async function ioExportTree() {
    try {
        const conns = await apiGet('/api/connections');
        const out = { version: '2.0', connections: [] };
        for (const c of conns) {
            const g = await apiGet(`/api/connections/${c.id}/groups`);
            const tags = await apiGet(`/api/connections/${c.id}/tags`);
            out.connections.push({
                name: c.name, driver_type: c.driver_type, enabled: c.enabled,
                poll_interval_ms: c.poll_interval_ms, config: c.config,
                groups: g.groups || [],
                tags: tags.map(t => ({ name: t.name, group_name: t.group_name || 'Общие',
                    address_str: t.address_str, data_type: t.data_type,
                    scale: t.scale, offset_val: t.offset_val, unit: t.unit,
                    log_interval_ms: t.log_interval_ms, deadband: t.deadband })),
            });
        }
        downloadFile('pda_io_tree_backup.json', JSON.stringify(out, null, 2));
        ioStatusLine(`Экспортировано модулей: ${out.connections.length}`);
    } catch (e) { ioStatusLine(`Ошибка экспорта: ${e.message}`, true); }
}

async function ioImportTree(file) {
    if (!file) return;
    try {
        const tree = JSON.parse(await file.text());
        if (!tree.connections) return alertBox('Ошибка', 'Файл не содержит connections!', true);
        const mode = await confirmBox('Очистить старое дерево и записать новое?\n\nДа — очистить, Нет — добавить к текущим', { yes: 'Очистить', no: 'Добавить', cancel: 'Отмена' });
        if (mode === null) { document.getElementById('io-import-file').value = ''; return; }
        if (mode === true) for (const c of ioConns) await apiDel(`/api/connections/${c.id}`);
        let nTags = 0;
        for (const c of tree.connections) {
            const r = await apiPost('/api/connections', {
                name: c.name, driver_type: c.driver_type, enabled: c.enabled ?? true,
                poll_interval_ms: c.poll_interval_ms ?? 100, config: c.config || {},
            });
            const cid = r.id;
            for (const grp of c.groups || []) { try { await apiPost(`/api/connections/${cid}/groups`, { name: grp }); } catch (e) {} }
            for (const t of c.tags || []) {
                await apiPost('/api/tags', { id: 0, connection_id: cid, name: t.name, address_str: String(t.address_str ?? '0'),
                    data_type: t.data_type ?? 'FLOAT', scale: t.scale ?? 1.0, offset_val: t.offset_val ?? 0.0,
                    unit: t.unit ?? '', group_name: t.group_name ?? 'Общие',
                    log_interval_ms: t.log_interval_ms ?? 1000, deadband: t.deadband ?? 0.0 });
                nTags++;
            }
        }
        document.getElementById('io-import-file').value = '';
        ioSelGroup = null;
        await ioRefreshAll();
        alertBox('Успех', `Импортировано модулей: ${tree.connections.length}, переменных: ${nTags}`);
    } catch (e) { ioStatusLine(`Ошибка импорта: ${e.message}`, true); }
}

// ------------------------------------------------------------------ CSV
function csvSplit(sample) { return sample.includes(';') ? ';' : ','; }

function ioExportCsv() {
    if (!ioSelConn) return ioStatusLine('Сначала выберите модуль', true);
    const lines = [['Name', 'Group', 'Address', 'Type', 'Scale', 'Offset', 'Unit'].join(';')];
    for (const t of ioTags) lines.push([t.name, t.group_name || 'Общие', t.address_str, t.data_type, t.scale, t.offset_val, t.unit || ''].map(csvCell).join(';'));
    const conn = ioConns.find(c => c.id === ioSelConn);
    downloadFile(`${conn.name}_tags.csv`, '\ufeff' + lines.join('\r\n'));
}
function csvCell(v) { v = String(v ?? ''); return /[;"]/.test(v) ? '"' + v.replace(/"/g, '""') + '"' : v; }

async function ioImportCsv(file) {
    if (!file) return;
    if (!ioSelConn) { document.getElementById('io-csv-file').value = ''; return ioStatusLine('Сначала выберите модуль загрузки', true); }
    try {
        const text = (await file.text()).replace(/^\ufeff/, '');
        const rows = parseCsv(text, csvSplit(text.slice(0, 2048)));
        const header = rows.shift().map(h => h.trim().toLowerCase());
        const idx = (keys) => header.findIndex(h => keys.includes(h));
        const col = { name: idx(['name']), group: idx(['group', 'группа']), addr: idx(['address', 'адрес']),
            type: idx(['type', 'тип']), scale: idx(['scale', 'масштаб']), off: idx(['offset', 'смещение']), unit: idx(['unit', 'ед']) };
        const existing = Object.fromEntries(ioTags.map(t => [t.name, t]));
        let n = 0;
        for (const r of rows) {
            const get = (i) => (i >= 0 ? (r[i] || '').trim() : '');
            const name = get(col.name); if (!name) continue;
            const body = { connection_id: ioSelConn, name, id: existing[name] ? existing[name].id : 0,
                address_str: get(col.addr) || '0', data_type: get(col.type) || 'FLOAT',
                scale: Number(get(col.scale)) || 1.0, offset_val: Number(get(col.off)) || 0.0,
                unit: get(col.unit), group_name: get(col.group) || 'Общие',
                log_interval_ms: 1000, deadband: 0.0 };
            if (existing[name]) await apiPut(`/api/tags/${body.id}`, body);
            else await apiPost('/api/tags', body);
            n++;
        }
        document.getElementById('io-csv-file').value = '';
        await ioSelectConn(ioSelConn); await fetchTags();
        alertBox('Успех', `Обработано переменных: ${n}`);
    } catch (e) { ioStatusLine(`Ошибка импорта CSV: ${e.message}`, true); }
}

function parseCsv(text, delim) {
    const out = []; let row = [], field = '', q = false;
    for (let i = 0; i < text.length; i++) {
        const ch = text[i];
        if (q) { if (ch === '"' && text[i + 1] === '"') { field += '"'; i++; } else if (ch === '"') q = false; else field += ch; }
        else if (ch === '"') q = true;
        else if (ch === delim) { row.push(field); field = ''; }
        else if (ch === '\n') { row.push(field); out.push(row); row = []; field = ''; }
        else if (ch !== '\r') field += ch;
    }
    if (field || row.length) { row.push(field); out.push(row); }
    return out;
}

function downloadFile(name, content) {
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([content], { type: 'application/octet-stream' }));
    a.download = name; a.click();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
}
