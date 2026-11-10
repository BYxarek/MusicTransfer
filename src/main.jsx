import { Component, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { Channel, invoke } from '@tauri-apps/api/core';
import {
  Activity, ArrowLeft, ArrowRight, AudioLines, Check, ChevronRight, ClipboardPaste,
  Cloud, Download, ExternalLink, FileText, History, Link, ListMusic, LucideProvider, Moon, Music2, RotateCcw,
  Search, SkipForward, Square, Sun, Terminal, Trash2, Volume2,
} from 'lucide-react';
import { version } from '../package.json';
import './style.css';

const defaultOptions = { autoExact: true, autoSingle: true, autoClosest: true, fullAuto: false, sound: true, notifications: true };
const optionLabels = [
  ['autoExact', 'Точные совпадения'],
  ['autoSingle', 'Единственный подходящий вариант'],
  ['autoClosest', 'Наиболее похожий вариант'],
  ['fullAuto', 'Полностью автоматически'],
  ['sound', 'Звук при ожидании действия'],
  ['notifications', 'Уведомления Windows в фоне'],
];
const emptyProgress = { index: 0, total: 0, added: 0, skipped: 0, added_now: 0, already_present: 0, legacy_added: 0 };

function stored(key, fallback) {
  try {
    return JSON.parse(localStorage.getItem(key)) ?? fallback;
  } catch {
    return fallback;
  }
}

function persist(key, value) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // The current selection is still usable with storage disabled.
  }
}

function Button({ icon: Icon, kind = 'normal', children, ...props }) {
  return <button type="button" className={'button ' + kind} {...props}>
    {Icon && <Icon />}{children}
  </button>;
}

function Theme({ theme, onChange }) {
  return <div className="theme" role="group" aria-label="Тема оформления">
    <button type="button" title="Светлая тема" aria-label="Светлая тема" aria-pressed={theme === 'light'}
      onClick={() => onChange('light')}><Sun size={16} /></button>
    <button type="button" title="Тёмная тема" aria-label="Тёмная тема" aria-pressed={theme === 'dark'}
      onClick={() => onChange('dark')}><Moon size={16} /></button>
  </div>;
}

function duration(seconds) {
  if (!Number.isFinite(seconds) || seconds <= 0) return '';
  const minutes = Math.floor(seconds / 60);
  return minutes + ':' + String(seconds % 60).padStart(2, '0');
}

function TrackMeta({ seconds, tags = [], difference }) {
  return <span className="track-meta">
    {duration(seconds) && <span className="tag">{duration(seconds)}</span>}
    {Number.isFinite(difference) && difference > 0 && <span className="tag">Разница {difference} сек.</span>}
    {tags.map(tag => <span className="tag version" key={tag}>{tag}</span>)}
  </span>;
}

function TitleDiff({ title, original }) {
  if (!original) return title;
  const normalize = value => value.normalize('NFKC').toLocaleLowerCase().replaceAll('ё', 'е');
  const words = new Set(normalize(original).match(/[\p{L}\p{N}]+/gu));
  return title.split(/([\p{L}\p{N}]+)/u).map((token, index) =>
    index % 2 && !words.has(normalize(token)) ? <mark key={index}>{token}</mark> : token);
}

function CandidatePicker({ event, reply, command }) {
  const [selection, setSelection] = useState(null);
  const [query, setQuery] = useState(event.query ?? '');
  const retry = event.retry;
  const review = event.review;
  const radios = useRef([]);
  useEffect(() => {
    radios.current[0]?.focus({ preventScroll: true });
  }, []);

  function pickKey(key, index) {
    if (key.key === 'ArrowRight' && key.ctrlKey && event.defer) {
      key.preventDefault();
      reply({ defer: true });
    } else if (key.key === 'Enter') {
      key.preventDefault();
      if (selection === index) reply(index);
      else setSelection(index);
    } else if (['ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(key.key) &&
        !key.ctrlKey && !key.altKey && !key.metaKey) {
      key.preventDefault();
      const last = event.options.length - 1;
      const next = key.key === 'Home' ? 0 : key.key === 'End' ? last :
        Math.max(0, Math.min(last, index + (['ArrowUp', 'ArrowLeft'].includes(key.key) ? -1 : 1)));
      setSelection(next);
      radios.current[next]?.focus();
    }
  }
  return <>
    <h2>{review ? 'Проверка добавленного трека' : retry ? 'Ручной повтор поиска' : 'Выберите версию в SoundCloud'}</h2>
    <div className="track-summary">
      <div className="row">
        <span><strong>Яндекс Музыка</strong> · {event.label}</span>
        <TrackMeta seconds={event.originalDuration} tags={event.originalTags} />
        {event.originalUrl && <Button icon={ExternalLink}
          onClick={() => command('open_track', { url: event.originalUrl })}>Открыть оригинал</Button>}
      </div>
      {event.previous && <div className="row"><span><strong>Сейчас в плейлисте</strong> · {event.previous}</span>
        {event.previousUrl && <Button icon={ExternalLink}
          onClick={() => command('open_track', { url: event.previousUrl })}>Открыть текущую версию</Button>}</div>}
    </div>
    <form className="row" onSubmit={submit => { submit.preventDefault(); if (query.trim()) reply(query.trim()); }}>
      <input className="grow" aria-label="Запрос поиска" value={query} onChange={e => setQuery(e.target.value)}
        autoFocus={!event.options.length} />
      <button className="button" disabled={!query.trim()}><Search size={16} />Искать снова</button>
    </form>
    {event.options.length ? <div className="choices" role="radiogroup" aria-label="Результаты поиска">
      {event.options.map((track, index) => <div className="choice" key={track.index}>
        <label className="choice-label">
          <input type="radio" name="candidate" aria-label={track.title + ' ' + track.artist}
            ref={element => { radios.current[index] = element; }} tabIndex={index === (selection ?? 0) ? 0 : -1}
            onKeyDown={event => pickKey(event, index)}
            checked={index === selection} onChange={() => setSelection(index)} />
          <span><strong><TitleDiff title={track.title} original={event.originalTitle} /></strong><small>{track.artist}</small>
            <TrackMeta seconds={track.duration} tags={track.tags}
              difference={event.originalDuration && track.duration ? Math.abs(event.originalDuration - track.duration) : null} /></span>
        </label>
        {track.url && <Button icon={ExternalLink} aria-label={'Открыть в SoundCloud: ' + track.title}
          onClick={() => command('open_track', { url: track.url })}>Открыть</Button>}
      </div>)}
    </div> : <p className="notice">Доступных результатов нет. {review
      ? 'Измените запрос или оставьте текущую версию.' : 'Измените запрос или пропустите трек.'}</p>}
    {event.options.length > 0 && <p className="key-hint">↑ / ↓ — выбрать, Enter — подтвердить
      {event.defer && ', Ctrl + → — отложить'}</p>}
    <div className="row">
      <Button kind="primary" icon={Check} disabled={selection === null} onClick={() => reply(selection)}>
        {review ? 'Выбрать для замены' : 'Добавить выбранный'}
      </Button>
      {event.defer && <Button icon={ArrowRight} onClick={() => reply({ defer: true })}>Отложить</Button>}
      <Button icon={SkipForward} onClick={() => reply(null)}>
        {review ? 'Оставить текущую версию' : retry ? 'Оставить пропущенным' : 'Пропустить'}
      </Button>
    </div>
  </>;
}

function DestinationPicker({ event, reply }) {
  const [url, setUrl] = useState('');
  return <>
    <h2>Куда перенести треки</h2>
    <p>Создайте закрытый плейлист или добавьте треки в один из своих плейлистов SoundCloud.</p>
    <div className="row">
      <Button kind="primary" icon={ListMusic} onClick={() => reply('new')}>Новый закрытый плейлист</Button>
      <Button icon={Cloud} onClick={() => reply('library')}>Загрузить мои плейлисты</Button>
    </div>
    {event.options.length > 0 && <div className="choices playlist-choices">
      {event.options.map(option => <Button key={option.url} icon={ListMusic} onClick={() => reply(option.url)}>
        <span>{option.name}<small>{option.url}</small></span>
      </Button>)}
    </div>}
    <form className="row destination-url" onSubmit={submit => { submit.preventDefault(); if (url.trim()) reply(url.trim()); }}>
      <input className="grow" aria-label="Ссылка на плейлист SoundCloud" placeholder="https://soundcloud.com/…/sets/…"
        value={url} onChange={event => setUrl(event.target.value)} />
      <button className="button" disabled={!url.trim()}><Check size={16} />Проверить плейлист</button>
    </form>
    {event.error && <p className="notice" role="status">{event.error}</p>}
  </>;
}

function PlaylistLinks({ links, command }) {
  const [expanded, setExpanded] = useState(false);
  if (!links.length) return null;
  return <>
    <Button icon={ExternalLink} aria-expanded={links.length > 1 ? expanded : undefined}
      onClick={() => links.length === 1 ? command('open_playlist', { url: links[0][1] }) : setExpanded(!expanded)}>
      Открыть в SoundCloud
    </Button>
    {expanded && <div className="link-list">
      {links.map(([name, url]) => <Button key={url} icon={ExternalLink}
        onClick={() => command('open_playlist', { url })}>{name}</Button>)}
    </div>}
  </>;
}

function HistoryPage({ rows, loaded, selected, select, refresh, busy, start, skipped, added, openDetails, command, notify, deleteHistory }) {
  const entry = rows.find(row => row.id === selected);
  const [confirmId, setConfirmId] = useState(null);
  const [deleting, setDeleting] = useState(false);
  function choose(id) {
    setConfirmId(null);
    select(id);
  }
  return <section className="card">
    <div className="section-head">
      <div><h2>История переносов</h2><p>Сохранённые отчёты Яндекс Музыки и плейлисты SoundCloud</p></div>
      <Button icon={RotateCcw} disabled={deleting} onClick={refresh}>Обновить</Button>
    </div>
    {rows.length ? <div className="table-wrap"><table>
      <thead><tr><th>Плейлист</th><th>Дата</th><th>Добавлено</th><th>Статус</th></tr></thead>
      <tbody>{rows.map(row => <tr key={row.id} className={selected === row.id ? 'selected' : ''}
        onClick={() => !deleting && choose(row.id)}>
        <td><button type="button" className="table-pick" aria-pressed={selected === row.id}
          disabled={deleting} onClick={event => { event.stopPropagation(); choose(row.id); }}>{row.title || 'Без названия'}</button></td>
        <td>{row.date}</td><td>{row.added} из {row.total}</td><td><span className="tag">{row.status}</span></td>
      </tr>)}</tbody>
    </table></div> : <div className="empty"><History size={28} />
      <p>{loaded ? 'Сохранённых переносов пока нет.' : 'Загружаю историю…'}</p></div>}
    {entry && <div className="history-detail" aria-label="Действия для выбранного переноса">
      <div><strong>{entry.title}</strong><p>{reportCounts(entry)}</p></div>
      <div className="row history-actions">
        <Button icon={ArrowRight} kind="primary" disabled={busy || deleting || !entry.resumable}
          onClick={() => start({ id: entry.id })}>Продолжить перенос</Button>
        <Button icon={ListMusic} onClick={() => skipped(entry)}>Пропущенные треки</Button>
        <Button icon={Check} onClick={() => added(entry)} disabled={!entry.added}>Добавленные треки</Button>
        <Button icon={ListMusic} onClick={() => openDetails(entry)}>Отчёт по трекам</Button>
        <Button icon={FileText} onClick={() => command('open_report', { id: entry.id })}>Открыть отчёт</Button>
        <Button icon={Link} onClick={async () => {
          if (await command('copy_link', { id: entry.id })) notify('Ссылка на Яндекс Музыку скопирована.');
        }}>Скопировать ссылку</Button>
        <Button icon={Trash2} kind="danger" disabled={busy || deleting}
          onClick={() => setConfirmId(entry.id)}>Удалить из истории</Button>
        <PlaylistLinks key={entry.id} links={entry.links} command={command} />
      </div>
      {confirmId === entry.id && <div className="history-delete" role="group" aria-label="Подтверждение удаления">
        <p><strong>Удалить «{entry.title || 'Без названия'}» из истории?</strong> Локальный отчёт будет удалён, и продолжить этот перенос будет нельзя. Плейлисты на музыкальных сервисах сохранятся.</p>
        <div className="row">
          <Button autoFocus disabled={deleting} onClick={() => setConfirmId(null)}>Отмена</Button>
          <Button icon={Trash2} kind="danger" disabled={busy || deleting} onClick={async () => {
            setDeleting(true);
            try {
              await deleteHistory(entry.id);
              setConfirmId(null);
            } finally {
              setDeleting(false);
            }
          }}>{deleting ? 'Удаляю…' : 'Удалить отчёт'}</Button>
        </div>
      </div>}
    </div>}
  </section>;
}

const trackStatuses = {
  added: 'Добавлено переносом', existing: 'Уже было', legacy: 'В старом отчёте',
  skipped: 'Пропущено', reconcile: 'Нужна сверка', deferred: 'Отложено',
  pending: 'Не обработано', error: 'Сбой поиска',
};

function reportCounts(report) {
  const legacy = report.legacy_added ?? report.added ?? 0;
  return 'Добавлено переносом: ' + (report.added_now ?? 0) + ' · Уже было: ' + (report.already_present ?? 0) +
    (legacy ? ' · Без данных о добавлении: ' + legacy : '') + ' · Пропущено: ' + report.skipped;
}

function ReportPage({ entry, rows, busy, back, start, command, notify, error }) {
  const [filter, setFilter] = useState('all');
  const [query, setQuery] = useState('');
  const search = query.trim().toLocaleLowerCase();
  const visible = rows.filter(row => (filter === 'all' || row.status === filter || row.method === filter) &&
    [row.title, row.artist, row.detail, row.playlist?.[0] ?? ''].some(value => value.toLocaleLowerCase().includes(search)));
  const [exporting, setExporting] = useState(false);
  async function exportCsv(numbers) {
    setExporting(true);
    try {
      if (await invoke('export_report_csv', { id: entry.id, numbers })) notify('Отчёт сохранён в CSV.');
    } catch (cause) {
      error(cause);
    } finally {
      setExporting(false);
    }
  }
  return <section className="card">
    <div className="section-head">
      <div><h2>{entry.title}</h2><p>{reportCounts(entry)}</p></div>
      <Button icon={ArrowLeft} onClick={back}>К истории</Button>
    </div>
    <div className="row report-filters">
      <input className="grow" aria-label="Поиск по отчёту" placeholder="Трек, исполнитель, версия…" value={query}
        onChange={event => setQuery(event.target.value)} />
      <select aria-label="Фильтр отчёта" value={filter} onChange={event => setFilter(event.target.value)}>
        <option value="all">Все треки</option>
        <option value="auto">Автовыбор</option>
        <option value="manual">Выбрано вручную</option>
        {Object.entries(trackStatuses).map(([key, text]) => <option value={key} key={key}>{text}</option>)}
      </select>
      <span className="tag" role="status">Показано: {visible.length} из {rows.length}</span>
    </div>
    <div className="row report-filters">
      <Button icon={Download} disabled={exporting || !rows.length} onClick={() => exportCsv(null)}>Весь отчёт в CSV</Button>
      {visible.length !== rows.length && <Button icon={Download} disabled={exporting || !visible.length}
        onClick={() => exportCsv(visible.map(row => row.number))}>Выборку в CSV</Button>}
    </div>
    {visible.length ? <div className="table-wrap"><table>
      <thead><tr><th>Трек</th><th>Результат</th><th>Действия</th></tr></thead>
      <tbody>{visible.map(row => <tr key={row.number}>
        <td><strong>{row.title}</strong><br />{row.artist}</td>
        <td><span className="tag">{trackStatuses[row.status]}</span>{' '}
          {row.method && <span className="tag">{row.method === 'auto' ? 'Автовыбор' : 'Вручную'}</span>}
          <p>{row.detail}</p>{row.playlist && <small>{row.playlist[0]}</small>}</td>
        <td><div className="row">
          {row.soundcloud_url && <Button icon={ExternalLink}
            onClick={() => command('open_track', { url: row.soundcloud_url })}>Запись в SoundCloud</Button>}
          {row.source_url && <Button icon={ExternalLink}
            onClick={() => command('open_track', { url: row.source_url })}>Оригинал</Button>}
          {row.playlist?.[1] && <Button icon={ListMusic}
            onClick={() => command('open_playlist', { url: row.playlist[1] })}>Плейлист</Button>}
          {['added', 'existing', 'legacy'].includes(row.status) && <Button icon={Search} disabled={busy || entry.reconcile}
            onClick={() => start({ id: entry.id, review: row.number })}>Проверить / заменить</Button>}
          {['pending', 'skipped', 'deferred', 'error'].includes(row.status) && <Button icon={Search}
            disabled={busy || entry.reconcile} onClick={() => start({ id: entry.id, retry: [row.number] })}>Искать снова</Button>}
          {row.status === 'reconcile' && <Button icon={RotateCcw} disabled={busy}
            onClick={() => start({ id: entry.id })}>Продолжить и сверить</Button>}
        </div></td>
      </tr>)}</tbody>
    </table></div> : <div className="empty"><Search size={24} /><p>По этим условиям треков нет.</p></div>}
  </section>;
}

function AddedPage({ entry, rows, busy, back, start, command }) {
  return <section className="card">
    <div className="section-head">
      <div><h2>Добавленные треки</h2><p>{entry.title} · {rows.length}</p></div>
      <Button icon={ArrowLeft} onClick={back}>К истории</Button>
    </div>
    {entry.reconcile && <p className="notice">Сначала выполните сверку через «Продолжить перенос».</p>}
    {rows.length ? <div className="table-wrap"><table>
      <thead><tr><th>Исходный трек</th><th>Версия в SoundCloud</th><th>Действия</th></tr></thead>
      <tbody>{rows.map(row => <tr key={row.number}>
        <td><strong>{row.title}</strong><br />{row.artist}</td><td>{row.version}</td>
        <td><div className="row">
          <Button icon={Search} disabled={busy || entry.reconcile}
            onClick={() => start({ id: entry.id, review: row.number })}>Проверить / заменить</Button>
          {row.url && <Button icon={ExternalLink} onClick={() => command('open_track', { url: row.url })}>Открыть трек</Button>}
          {row.playlist && <Button icon={ListMusic}
            onClick={() => command('open_playlist', { url: row.playlist })}>Плейлист</Button>}
        </div></td>
      </tr>)}</tbody>
    </table></div> : <div className="empty"><ListMusic size={26} /><p>В этом отчёте пока нет добавленных треков.</p></div>}
  </section>;
}

function SkippedPage({ entry, rows, busy, back, refresh, start }) {
  const [selected, setSelected] = useState([]);
  const blocked = busy || entry.reconcile;
  const available = selected.filter(number => rows.some(row => row.number === number));
  return <section className="card">
    <div className="section-head">
      <div><h2>Пропущенные треки</h2><p>{entry.title} · {rows.length}</p></div>
      <Button icon={ArrowLeft} onClick={back}>К истории</Button>
    </div>
    {entry.reconcile && <p className="notice">Сначала выполните сверку через «Продолжить перенос».</p>}
    {rows.length ? <div className="table-wrap"><table>
      <thead><tr><th>Выбор</th><th>Трек</th><th>Исполнитель</th><th>Причина пропуска</th></tr></thead>
      <tbody>{rows.map(row => <tr key={row.number} className={available.includes(row.number) ? 'selected' : ''}>
        <td><input type="checkbox" checked={available.includes(row.number)} aria-label={'Выбрать ' + row.title}
          onChange={event => setSelected(event.target.checked
            ? [...selected, row.number] : selected.filter(number => number !== row.number))} /></td>
        <td>{row.title}</td><td>{row.artist}</td><td>{row.reason}</td>
      </tr>)}</tbody>
    </table></div> : <div className="empty"><Check size={26} /><p>В этом отчёте нет пропущенных треков.</p></div>}
    <div className="row actions">
      <Button icon={Search} kind="primary" disabled={blocked || !available.length}
        onClick={() => start({ id: entry.id, retry: available })}>Искать выбранные{available.length ? ' (' + available.length + ')' : ''}</Button>
      <Button icon={ListMusic} disabled={blocked || !rows.length}
        onClick={() => start({ id: entry.id, retry: rows.map(row => row.number) })}>Искать все пропущенные</Button>
      <Button icon={RotateCcw} onClick={refresh}>Обновить</Button>
    </div>
  </section>;
}

function Journal({ lines, command }) {
  const scroll = useRef(null);
  useEffect(() => {
    const element = scroll.current;
    if (element) element.scrollTop = element.scrollHeight;
  }, [lines]);
  return <section className="card journal">
    <div className="section-head">
      <h2><Terminal size={18} /> Журнал действий</h2>
      <div className="row">
        <Button icon={FileText} onClick={() => command('open_journal')}>Открыть журнал</Button>
        <Button icon={Terminal} onClick={() => command('open_diagnostics')}>Папка диагностики</Button>
      </div>
    </div>
    <div className="log" ref={scroll} tabIndex={0} aria-label="Записи журнала">
      {lines.map(([level, text], index) => <div key={index} className={level.toLowerCase()}>{text}</div>)}
    </div>
  </section>;
}

function App() {
  const [theme, setTheme] = useState(() => stored('theme', matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'));
  const [options, setOptions] = useState(() => ({ ...defaultOptions, ...stored('options', {}) }));
  const [page, setPage] = useState('home');
  const [url, setUrl] = useState('');
  const [state, setState] = useState({ kind: 'ready' });
  const [progress, setProgress] = useState(emptyProgress);
  const [entries, setEntries] = useState([]);
  const [loaded, setLoaded] = useState(false);
  const [selected, setSelected] = useState(null);
  const [skipView, setSkipView] = useState({ entry: null, rows: [] });
  const [addedView, setAddedView] = useState({ entry: null, rows: [] });
  const [reportView, setReportView] = useState({ entry: null, rows: [] });
  const [lines, setLines] = useState([]);
  const [notice, setNotice] = useState('');
  const [problem, setProblem] = useState('');
  const [busy, setBusy] = useState(false);
  const [updateVersion, setUpdateVersion] = useState('');
  const [updateAction, setUpdateAction] = useState('');
  const channel = useRef(null);
  const sending = useRef(false);
  const stopping = useRef(false);
  const sound = useRef(null);
  const preferences = useRef(options);
  const checkingUpdate = useRef(false);
  const phase = state.prompt ? 'waiting' : state.kind === 'stopping' ? 'stopping' : busy ? 'busy' : 'ready';

  useLayoutEffect(() => {
    document.documentElement.dataset.theme = theme;
    persist('theme', theme);
    invoke('set_window_theme', { theme }).catch(() => {});
  }, [theme]);
  useEffect(() => {
    persist('options', options);
    preferences.current = options;
  }, [options]);
  useEffect(() => { invoke('journal').then(setLines).catch(error => setProblem(String(error))); }, []);
  useEffect(() => { checkUpdates(false); }, []);

  function error(error) {
    setProblem(String(error));
    invoke('report_ui_error', { message: String(error) }).catch(() => {});
  }

  async function command(name, args) {
    try {
      await invoke(name, args);
      return true;
    } catch (cause) {
      error(cause);
      return false;
    }
  }

  async function checkUpdates(interactive = true) {
    if (checkingUpdate.current) return;
    checkingUpdate.current = true;
    setUpdateAction('check');
    if (interactive) {
      setUpdateVersion('');
      setProblem('');
      setNotice('');
    }
    try {
      const update = await invoke('check_update');
      if (update.status === 'available') setUpdateVersion(update.version);
      else if (interactive) setNotice('Установлена актуальная версия.');
    } catch (cause) {
      if (interactive) error(cause);
      else invoke('report_ui_error', { message: String(cause) }).catch(() => {});
    } finally {
      checkingUpdate.current = false;
      setUpdateAction('');
    }
  }

  async function installUpdate() {
    setUpdateAction('install');
    setProblem('');
    await command('install_update', { version: updateVersion });
    setUpdateAction('');
  }

  async function refreshHistory() {
    try {
      const rows = await invoke('transfer_history');
      setEntries(rows);
      setLoaded(true);
      setSelected(current => rows.some(row => row.id === current) ? current : rows[0]?.id ?? null);
      return rows;
    } catch (cause) {
      error(cause);
      return [];
    }
  }

  function historyPage() {
    setProblem('');
    setNotice('');
    setPage('history');
    refreshHistory();
  }

  async function deleteHistory(id) {
    setProblem('');
    setNotice('');
    if (await command('delete_transfer_history', { id })) {
      if (state.report?.id === id) {
        setState({ kind: 'ready' });
        setProgress(emptyProgress);
      }
      setNotice('Запись удалена из истории.');
    }
    await refreshHistory();
  }

  async function showSkipped(entry) {
    try {
      const rows = await invoke('skipped_tracks', { id: entry.id });
      setSkipView({ entry, rows });
      setPage('skipped');
      setNotice('');
    } catch (cause) {
      error(cause);
    }
  }

  async function refreshSkipped() {
    const rows = await refreshHistory();
    await showSkipped(rows.find(row => row.id === skipView.entry.id) ?? skipView.entry);
  }

  async function showAdded(entry) {
    try {
      const rows = await invoke('added_tracks', { id: entry.id });
      setAddedView({ entry, rows });
      setPage('added');
      setNotice('');
    } catch (cause) {
      error(cause);
    }
  }

  async function showReport(entry) {
    try {
      const rows = await invoke('transfer_report', { id: entry.id });
      setReportView({ entry, rows });
      setPage('report');
      setNotice('');
    } catch (cause) {
      error(cause);
    }
  }

  function onEvent(event) {
    if (event.kind === 'log') {
      setLines(lines => [...lines.slice(-499), [event.level, event.line]]);
    } else if (event.kind === 'progress') {
      setProgress(event);
    } else if (!stopping.current || event.kind === 'done') {
      sending.current = false;
      setState(event);
      if (event.prompt) {
        setPage('home');
        if (preferences.current.sound) {
          sound.current ??= new Audio('/attention.wav');
          sound.current.currentTime = 0;
          sound.current.play().catch(() => {});
        }
      }
      if (preferences.current.notifications && (event.prompt || event.kind === 'done')) {
        invoke('desktop_notification', {
          title: event.prompt ? event.title ?? 'Нужно действие в MusicTransfer' : 'MusicTransfer',
          text: event.prompt ? 'Откройте приложение, чтобы продолжить перенос.' : event.text,
        }).catch(cause => invoke('report_ui_error', { message: String(cause) }).catch(() => {}));
      }
      if (event.kind === 'done') {
        stopping.current = false;
        setBusy(false);
        channel.current = null;
        if (event.report) {
          setProgress({ ...emptyProgress, ...event.report });
          setSelected(event.report.id);
        }
      }
    }
  }

  async function start(request) {
    if (channel.current || updateAction === 'install') return;
    setProblem('');
    setNotice('');
    setProgress(emptyProgress);
    setPage('home');
    setBusy(true);
    setState({ kind: 'working', text: 'Подготовка к переносу…' });
    const events = new Channel(onEvent);
    channel.current = events;
    if (!await command('start_transfer', { request: { ...options, ...request }, events })) {
      channel.current = null;
      setBusy(false);
      setState({ kind: 'ready' });
    }
  }

  async function reply(answer) {
    if (!state.prompt || sending.current) return;
    sending.current = true;
    const previous = state;
    setState({ kind: 'working', text: 'Продолжаю…' });
    if (!await command('reply_transfer', { prompt: previous.prompt, answer })) {
      setState(current => current.kind === 'working' ? previous : current);
      sending.current = false;
    }
  }

  function stop() {
    stopping.current = true;
    setState({ kind: 'stopping', text: 'Дожидаюсь завершения текущего действия.' });
    command('stop_transfer');
  }

  async function paste() {
    try {
      setUrl((await invoke('paste_link')).trim());
      setProblem('');
    } catch (cause) {
      error(cause);
    }
  }

  const screen = page === 'history' ? ['БИБЛИОТЕКА', 'История'] :
    page === 'report' ? ['ПРОВЕРКА ПЕРЕНОСА', 'Отчёт по трекам'] :
    page === 'added' ? ['ПРОВЕРКА ПЕРЕНОСА', 'Добавленные треки'] :
    page === 'skipped' ? ['ПОВТОРНЫЙ ПОИСК', 'Пропущенные треки'] : ['РАБОЧАЯ ОБЛАСТЬ', 'Перенос плейлистов'];

  return <div className="shell">
    <aside className="sidebar">
      <div className="brand"><span className="brand-mark"><AudioLines size={26} /></span>
        <div><strong>MusicTransfer</strong><small>Яндекс Музыка → SoundCloud</small></div></div>
      <nav aria-label="Основная навигация">
        <button type="button" aria-current={page === 'home' ? 'page' : undefined} onClick={() => setPage('home')}>
          <ListMusic size={19} />Перенос<ChevronRight size={15} className="chevron" /></button>
        <button type="button" aria-current={page !== 'home' ? 'page' : undefined} onClick={historyPage}>
          <History size={19} />История<ChevronRight size={15} className="chevron" /></button>
      </nav>
      <div className="sidebar-foot"><span>DESKTOP</span><span>{version}</span></div>
    </aside>
    <main>
      <header className="page-head">
        <div><span className="eyebrow">{screen[0]}</span><h1>{screen[1]}</h1></div>
        <div className="row">
          <Button icon={Download} disabled={!!updateAction || busy} onClick={() => checkUpdates()}>
            {updateAction === 'check' ? 'Проверяю…' : 'Проверить обновления'}
          </Button>
          <Theme theme={theme} onChange={setTheme} />
        </div>
      </header>
      {problem && <div className="banner error-banner" role="alert">{problem}<Button onClick={() => setProblem('')}>Закрыть</Button></div>}
      {notice && <div className="banner" role="status">{notice}<Button onClick={() => setNotice('')}>Закрыть</Button></div>}
      {updateVersion && <div className="banner" role="status">Доступна версия {updateVersion}
        <Button icon={Download} disabled={busy || !!updateAction} onClick={installUpdate}>
          {updateAction === 'install' ? 'Загрузка и проверка подписи…' : 'Скачать и установить'}
        </Button>
      </div>}
      {page === 'history' && <HistoryPage rows={entries} loaded={loaded} selected={selected}
        select={setSelected} refresh={refreshHistory} busy={busy} start={start} skipped={showSkipped} added={showAdded}
        openDetails={showReport} command={command} notify={setNotice} deleteHistory={deleteHistory} />}
      {page === 'report' && <ReportPage key={reportView.entry.id} {...reportView} busy={busy} back={historyPage}
        start={start} command={command} notify={setNotice} error={error} />}
      {page === 'skipped' && <SkippedPage key={skipView.entry.id} {...skipView} busy={busy} back={historyPage}
        refresh={refreshSkipped} start={start} />}
      {page === 'added' && <AddedPage {...addedView} busy={busy} back={historyPage} start={start} command={command} />}
      {page === 'home' && <>
        <section className="card source-card">
          <div className="route"><span><Music2 size={15} />Яндекс Музыка</span><ArrowRight size={16} />
            <span><Cloud size={17} />SoundCloud</span></div>
          <form onSubmit={event => { event.preventDefault(); if (url.trim() && !busy) start({ url: url.trim() }); }}>
            <label className="field-label" htmlFor="source-url">Ссылка на плейлист</label>
            <div className="row">
              <input className="grow" id="source-url" type="url" placeholder="https://music.yandex.by/playlists/…" value={url}
                onChange={event => setUrl(event.target.value)} disabled={busy} />
              <Button icon={ClipboardPaste} disabled={busy} onClick={paste}>Вставить</Button>
              <button className="button primary" disabled={busy || !url.trim()}>Открыть плейлист<ArrowRight size={16} /></button>
            </div>
          </form>
          <details className="preferences">
            <summary>Настройки подбора и уведомлений</summary>
            <div className="option-grid">{optionLabels.map(([key, text]) => <label key={key}>
              <input type="checkbox" disabled={busy && key !== 'sound' && key !== 'notifications'} checked={options[key]}
                onChange={event => setOptions({ ...options, [key]: event.target.checked })} />
              {key === 'sound' && <Volume2 size={16} />}{text}
            </label>)}</div>
          </details>
        </section>
        <section className="card workspace">
          <div className="section-head">
            <span className={'activity ' + phase}><Activity size={15} />{
              { ready: 'Готово', busy: 'В работе', waiting: 'Ждёт действия', stopping: 'Остановка' }[phase]
            }</span>
            {busy && <Button icon={Square} disabled={phase === 'stopping'} onClick={stop}>Остановить</Button>}
          </div>
          <div className="stage" key={state.prompt ?? state.kind}>
            {state.kind === 'ready' && <><h2>Начните с плейлиста</h2>
              <p>Вставьте ссылку выше или откройте историю, чтобы продолжить сохранённый перенос.</p></>}
            {(state.kind === 'working' || state.kind === 'stopping') && <><h2>{state.text}</h2>
              <div className="loading-line" aria-hidden="true" /></>}
            {state.kind === 'action' && <><h2>{state.title}</h2><p>{state.text}</p><div className="row">
              {state.actions.map(([text, value], index) => <Button key={value} kind={index === 0 ? 'primary' : 'normal'}
                onClick={() => reply(value)}>{text}</Button>)}</div></>}
            {state.kind === 'source' && <><h2>{state.title}</h2><p>Считано треков: {state.tracks.length}. Сверьте список с оригиналом.</p>
              <ol className="track-list">{state.tracks.map((track, index) => <li key={index}>{track}</li>)}</ol>
              <Button kind="primary" icon={ArrowRight} onClick={() => reply('continue')}>Перейти к SoundCloud</Button></>}
            {state.kind === 'candidates' && <CandidatePicker event={state} reply={reply} command={command} />}
            {state.kind === 'destination' && <DestinationPicker event={state} reply={reply} />}
            {state.kind === 'done' && <><h2>{state.text}</h2>
              {state.report && <><p>{state.report.title}</p><div className="row">
                {state.report.resumable && <Button icon={ArrowRight} kind="primary"
                  onClick={() => start({ id: state.report.id })}>Продолжить перенос</Button>}
                <Button icon={ListMusic} onClick={() => showSkipped(state.report)}>Пропущенные треки</Button>
                <Button icon={Check} onClick={() => showAdded(state.report)} disabled={!state.report.added}>Добавленные треки</Button>
                <Button icon={ListMusic} onClick={() => showReport(state.report)}>Отчёт по трекам</Button>
                <Button icon={FileText} onClick={() => command('open_report', { id: state.report.id })}>Открыть отчёт</Button>
                <PlaylistLinks links={state.report.links} command={command} />
              </div></>}</>}
          </div>
          {progress.total > 0 && <div className="progress">
            <progress value={progress.added + progress.skipped} max={progress.total} />
            <div><span>{progress.index ? 'Трек ' + progress.index + ' из ' : 'Всего: '}{progress.total}</span>
              <span>{reportCounts(progress)}</span></div>
          </div>}
        </section>
        <Journal lines={lines} command={command} />
      </>}
    </main>
  </div>;
}

class Boundary extends Component {
  state = { error: null };
  static getDerivedStateFromError(error) { return { error }; }
  componentDidCatch(error, details) {
    invoke('report_ui_error', { message: error.stack + '\n' + details.componentStack }).catch(() => {});
  }
  render() {
    if (!this.state.error) return this.props.children;
    return <main><div className="banner error-banner" role="alert">Ошибка интерфейса. Подробности записаны в журнал.
      <Button onClick={async () => {
        await invoke('stop_transfer').catch(() => {});
        window.location.reload();
      }}>Перезагрузить окно</Button></div></main>;
  }
}

createRoot(document.getElementById('root')).render(
  <Boundary><LucideProvider size={16}><App /></LucideProvider></Boundary>,
);
