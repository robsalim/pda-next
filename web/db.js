// Вкладка Database: форма движка + статус-панель. Логика перенесена
// из python_app/ui/db_settings_widget.py (_load_current_config,
// _refresh_status_panel, _test/_create/_save_and_reconnect).

let dbPanelTimer = null;

function dbStartPanel() {
    if (dbPanelTimer) return;
    dbLoadConfig();
    dbRefreshPanel();
    dbPanelTimer = setInterval(dbRefreshPanel, 2000);
}

function dbEngine() { return document.getElementById('db-engine').value; }

function dbShowEngineFields() {
    const isSqlite = dbEngine() === 'sqlite';
    document.querySelectorAll('.db-net').forEach(el => el.style.display = isSqlite ? 'none' : 'flex');
    document.querySelectorAll('.db-sqlite').forEach(el => el.style.display = isSqlite ? 'flex' : 'none');
    // Кнопка «Создать базу» осмысленна только для сетевых движков
    const btnCreate = document.getElementById('db-btn-create');
    if (btnCreate) btnCreate.style.display = isSqlite ? 'none' : '';
}

function dbOnEngineChanged() {
    dbShowEngineFields();
    // Дефолты пользователя/порта как в Python _on_engine_changed
    const eng = dbEngine(), port = document.getElementById('db-port'), user = document.getElementById('db-user');
    if (eng === 'postgres') { port.value = 5432; if (!user.value.trim() || user.value === 'root') user.value = 'postgres'; }
    else if (eng === 'mariadb') { port.value = 3306; if (!user.value.trim() || user.value === 'postgres') user.value = 'root'; }
}

// ------------------------------------------------------------------ конфиг
async function dbLoadConfig() {
    try {
        const s = await apiGet('/api/db/status');
        const c = s.config || {};
        const eng = c.engine === 'mysql' ? 'mariadb' : (c.engine || 'sqlite');
        document.getElementById('db-engine').value = ['postgres', 'mariadb', 'sqlite'].includes(eng) ? eng : 'sqlite';
        document.getElementById('db-host').value = c.host || 'localhost';
        document.getElementById('db-port').value = c.port || (eng === 'mariadb' ? 3306 : 5432);
        document.getElementById('db-name').value = c.dbname || 'pda_data';
        document.getElementById('db-user').value = c.user || (eng === 'mariadb' ? 'root' : 'postgres');
        document.getElementById('db-sqlite-path').value = c.sqlite_path || 'data/pda_local.db';
        dbShowEngineFields();
    } catch (e) { dbSetStatus('Не удалось загрузить конфиг БД: ' + e.message, 'error'); }
}

function dbFormConfig() {
    const eng = dbEngine();
    if (eng === 'sqlite') return { engine: 'sqlite', sqlite_path: document.getElementById('db-sqlite-path').value.trim() };
    return {
        engine: eng,
        host: document.getElementById('db-host').value.trim(),
        port: Number(document.getElementById('db-port').value),
        dbname: document.getElementById('db-name').value.trim(),
        user: document.getElementById('db-user').value.trim(),
        password: document.getElementById('db-pass').value,
    };
}

function dbSetStatus(text, cls = '') {
    const el = document.getElementById('db-status-line');
    el.textContent = text;
    el.className = 'db-status-line' + (cls ? ' ' + cls : '');
}

async function dbTest() {
    dbSetStatus('Тестирование соединения...', 'warn');
    try {
        const r = await apiPost('/api/db/test', dbFormConfig());
        if (r.ok) { dbSetStatus(`🟢 Подключение успешно! Найдено модулей: ${r.connections}`, 'ok'); }
        else { dbSetStatus('🔴 Ошибка соединения! ' + (r.error || ''), 'error'); }
    } catch (e) { dbSetStatus('🔴 Ошибка соединения! ' + e.message, 'error'); }
}

async function dbCreate() {
    const cfg = dbFormConfig();
    if (cfg.engine === 'sqlite') { dbSetStatus('Для SQLite база создаётся автоматически при применении.', 'warn'); return; }
    dbSetStatus('Создание базы данных...', 'warn');
    try {
        const r = await apiPost('/api/db/create', cfg);
        dbSetStatus('🟢 База ' + (r.created ? 'создана' : 'уже существует') + '. Нажмите «Применить и переключить».', 'ok');
    } catch (e) { dbSetStatus('🔴 Не удалось создать базу: ' + e.message, 'error'); }
}

async function dbSwitch() {
    dbSetStatus('Проверка подключения...', 'warn');
    try {
        await apiPost('/api/db/switch', dbFormConfig());
        dbSetStatus('🟢 База переключена. Сервисы перезапущены.', 'ok');
        await fetchTags();
        if (typeof ioRefreshAll === 'function') ioRefreshAll();
    } catch (e) { dbSetStatus('🔴 Ошибка переключения базы! ' + e.message + ' (текущее подключение сохранено)', 'error'); }
}

async function dbApplyRetention() {
    const days = Number(document.getElementById('db-retention').value);
    try { await apiPost('/api/retention', { days }); dbSetStatus(`🟢 Глубина хранения: ${days} сут`, 'ok'); }
    catch (e) { dbSetStatus('🔴 Не удалось применить глубину хранения: ' + e.message, 'error'); }
}

// ------------------------------------------------------------------ панель статуса
function dbIsErrorStatus(s) {
    const sl = (s || '').toLowerCase();
    return sl.startsWith('error') || sl.startsWith('connect error') || sl.startsWith('connection failed')
        || sl.startsWith('poll error') || sl.includes('error') || sl.includes('failed');
}

async function dbRefreshPanel() {
    if (activeTab !== 'db') return;
    let st, conns = [];
    try { st = await apiGet('/api/status'); } catch (e) { return; }
    try { conns = await apiGet('/api/connections'); } catch (e) { /* игнорируем */ }
    const db = st.db || {};
    const drivers = st.drivers || [];

    // 1. БД
    const p1 = document.getElementById('db-p-db');
    if (db.available) {
        const eng = (db.engine || '').toUpperCase();
        const c = db.config || {};
        const target = c.engine === 'sqlite' ? c.sqlite_path : `${c.host}:${c.port}/${c.dbname}`;
        p1.innerHTML = `<span class="st-ok">БД: подключена</span> [${eng}] ${escHtml(target || '')}`;
    } else {
        p1.innerHTML = `<span class="st-error">БД: недоступна${db.error ? ': ' + escHtml(db.error) : ''}</span>`;
    }

    // 2. Модули
    const running = drivers.filter(d => d.running).length;
    const errs = drivers.filter(d => dbIsErrorStatus(d.status));
    document.getElementById('db-p-modules').innerHTML =
        `Модули: всего ${conns.length}, запущено ${drivers.length}, работают ${running}, требуют внимания ${errs.length}`;

    // 3. Журнал (история)
    const j = st.journal || {};
    const retField = document.getElementById('db-retention');
    if (document.activeElement !== retField && typeof j.retention_days === 'number') {
        retField.value = j.retention_days;
    }
    let line = `Журнал: в очереди ${j.queued ?? 0} · записано ${j.written ?? 0} пачками ${j.batches ?? 0} · ` +
        `потеряно ${j.dropped ?? 0} · ошибок записи ${j.errors ?? 0}`;
    if (j.last_error) line += ' | ' + String(j.last_error).slice(0, 80);
    const bad = (j.dropped ?? 0) > 0 || (j.errors ?? 0) > 0;
    document.getElementById('db-p-journal').innerHTML = `<span class="${bad ? 'st-warn' : ''}">${escHtml(line)}</span>`;

    // 4. Ошибки драйверов
    const errList = errs.map(d => `${d.name || ('Модуль ' + d.conn_id)}: ${d.status}`).join('; ');
    document.getElementById('db-p-errors').innerHTML = 'Ошибки: ' +
        `<span class="${errList ? 'st-error' : 'st-ok'}">${escHtml(errList || 'нет активных ошибок')}</span>`;

    const now = new Date();
    const pad = n => String(n).padStart(2, '0');
    document.getElementById('db-p-updated').textContent = 'Последнее обновление: ' +
        `${pad(now.getDate())}.${pad(now.getMonth() + 1)}.${now.getFullYear()} ` +
        `${pad(now.getHours())}:${pad(now.getMinutes())}:${pad(now.getSeconds())}`;
}
