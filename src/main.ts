/* ---------------------------------------------------------------------------
   Codex Widget — renderer.

   Rust pushes a snapshot of *all* turns from both agents (Codex via its jsonl
   logs, DSH via its projection cache). This file renders them as two stacked
   capsules, in a fixed order:

       DSH   on top   (blue hairline under it, whale glyph if you installed one)
       Codex below

   Each row owns its own state machine, its own ×N and its own five-minute
   hold, so one finishing never disturbs the other. Only presentation lives
   here — which turn is "running" is decided in Rust.
--------------------------------------------------------------------------- */

import './styles.css';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { PhysicalPosition } from '@tauri-apps/api/dpi';
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from '@tauri-apps/plugin-notification';

type Status = 'running' | 'done' | 'aborted';
type Mode = 'idle' | 'run' | 'done' | 'aborted';
type Source = 'dsh' | 'codex';

interface ThreadView {
  source: Source;
  thread_id: string;
  cwd: string;
  title: string;
  model: string;
  provider: string;
  status: Status;
  /** unix seconds */
  started_at: number;
  /** unix seconds, present once the turn ends */
  completed_at: number | null;
  /** unix seconds of the newest event for this turn */
  last_activity: number;
  tokens: number;
}

interface Snapshot {
  threads: ThreadView[];
  now: number;
}

interface Settings {
  notifyOnComplete: boolean;
  showWidget: boolean;
  overlayThresholdSecs: number;
}

const STALE_SECS = 900;
const MAX_AGE_SECS = 86_400;
/** how long a finished capsule stays on screen before it leaves on its own */
const FINISHED_HOLD_MS = 5 * 60_000;
const DOUBLE_CLICK_MS = 350;

const SOURCES: Source[] = ['dsh', 'codex'];
const SOURCE_LABEL: Record<Source, string> = { dsh: 'DSH', codex: 'Codex' };

const rowsEl = document.getElementById('rows') as HTMLDivElement;

let visible = false;
let settings: Settings = {
  notifyOnComplete: true,
  showWidget: true,
  overlayThresholdSecs: 600,
};
let lastSentSize = '';
const announced = new Set<string>();
const threadKey = (t: ThreadView) => `${t.source}|${t.thread_id}|${t.completed_at ?? 0}`;

/* ------------------------------------------------------------------ format */

function fmt(totalSeconds: number): string {
  const s = Math.max(0, Math.floor(totalSeconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const pad = (n: number) => String(n).padStart(2, '0');
  return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${pad(m)}:${pad(sec)}`;
}

/** last path segment — "service" reads better than a 90-character path */
function taskLabel(t: ThreadView): string {
  const fromCwd = t.cwd.split(/[\\/]/).filter(Boolean).pop();
  const fromTitle = t.title.trim();
  if (fromCwd && fromCwd.length <= 40) return fromCwd;
  return fromTitle.length > 48 ? `${fromTitle.slice(0, 48)}…` : fromTitle || SOURCE_LABEL[t.source];
}

/* -------------------------------------------------------------------- row */

class Row {
  readonly el: HTMLDivElement;
  private readonly timeEl: HTMLSpanElement;
  private readonly countEl: HTMLSpanElement;
  mode: Mode = 'idle';
  private runningSince = 0;
  frozenElapsed = 0;
  private shownKey = '';
  private suppressedKey: string | null = null;
  private holdTimer = 0;
  private leaveTimer = 0;
  /** wall-clock deadline for the finished hold — see `tick()` */
  private hideAt = 0;

  constructor(el: HTMLDivElement) {
    this.el = el;
    this.timeEl = el.querySelector('.time') as HTMLSpanElement;
    this.countEl = el.querySelector('.count') as HTMLSpanElement;
  }

  private cancelHold(): void {
    window.clearTimeout(this.holdTimer);
    window.clearTimeout(this.leaveTimer);
    this.holdTimer = 0;
    this.leaveTimer = 0;
    this.hideAt = 0;
    this.el.classList.remove('leaving', 'settling');
  }

  setIdle(): void {
    this.cancelHold();
    this.setMode('idle');
    this.el.dataset.count = '1';
    this.countEl.textContent = '×1';
    this.shownKey = '';
  }

  private setMode(next: Mode): void {
    this.mode = next;
    this.el.dataset.state = next;
  }

  showRunning(startMs: number, count: number): void {
    const key = `run:${startMs}:${count}`;
    if (this.suppressedKey === key) return;

    this.cancelHold();
    if (this.mode !== 'run') {
      this.setMode('run');
      this.timeEl.textContent = '00:00';
      // a new turn means the previous reminder has been dealt with in practice
      void invoke('dismiss_overlay');
    }
    this.shownKey = key;
    this.runningSince = startMs;
    this.countEl.textContent = `×${count}`;
    this.el.dataset.count = String(count);
    this.timeEl.textContent = fmt((Date.now() - startMs) / 1000);
    settleSize();
  }

  showFinished(t: ThreadView, alsoFinished: number): void {
    const key = `fin:${threadKey(t)}`;
    if (this.suppressedKey === key) return;

    this.cancelHold();
    const startMs = t.started_at * 1000;
    const endMs = (t.completed_at ?? t.last_activity) * 1000;
    this.frozenElapsed = Math.max(0, (endMs - startMs) / 1000);

    this.shownKey = key;
    this.timeEl.textContent = fmt(this.frozenElapsed);
    this.countEl.textContent = `×${alsoFinished}`;
    this.el.dataset.count = String(alsoFinished);
    this.setMode(t.status === 'aborted' ? 'aborted' : 'done');

    this.el.classList.remove('peak');
    void this.el.offsetWidth;
    this.el.classList.add('peak');
    window.setTimeout(() => this.el.classList.remove('peak'), 950);

    settleSize();
    void notifyFinished(t, this.frozenElapsed, alsoFinished);
    void flashOverlayIfLong(t, this.frozenElapsed, alsoFinished);
    this.startHold();
  }

  /**
   * Five minutes, dimming almost imperceptibly, then it fades out.
   *
   * The deadline is a timestamp checked by the 500ms heartbeat rather than one
   * long `setTimeout`: Windows/Chromium throttle timers in occluded windows, and
   * this window sits under the full-screen reminder exactly when a second turn
   * finishes — which made a five-minute timer arrive minutes late (or never).
   */
  private startHold(): void {
    this.el.style.setProperty('--hold', `${FINISHED_HOLD_MS}ms`);
    this.el.classList.add('settling');
    // one mechanism only: the heartbeat compares against this deadline, so a
    // throttled timer cannot postpone the exit
    this.hideAt = Date.now() + FINISHED_HOLD_MS;
  }

  private expire(): void {
    if (this.mode === 'idle' || this.hideAt === 0) return;
    this.hideAt = 0;
    this.el.classList.add('leaving');
    this.leaveTimer = window.setTimeout(() => {
      this.el.classList.remove('leaving', 'settling');
      this.setIdle();
      syncVisibility();
    }, 520);
  }

  tick(): void {
    if (this.mode === 'run') {
      this.timeEl.textContent = fmt((Date.now() - this.runningSince) / 1000);
      return;
    }
    if (this.hideAt !== 0 && Date.now() >= this.hideAt) this.expire();
  }

  suppressCurrent(): void {
    this.suppressedKey = this.shownKey || null;
  }

  isIdle(): boolean {
    return this.mode === 'idle';
  }
}

const rows: Record<Source, Row> = {
  dsh: new Row(document.getElementById('row-dsh') as HTMLDivElement),
  codex: new Row(document.getElementById('row-codex') as HTMLDivElement),
};

/* ------------------------------------------------------------------ window */

async function syncSize(): Promise<void> {
  const rect = rowsEl.getBoundingClientRect();
  const width = Math.ceil(rect.width) + 12;
  const height = Math.ceil(rect.height) + 12;
  const key = `${width}x${height}`;
  if (key === lastSentSize) return;
  lastSentSize = key;
  await invoke('resize_to', { width, height });
}

function settleSize(): void {
  void syncSize();
  window.setTimeout(() => void syncSize(), 120);
  window.setTimeout(() => void syncSize(), 420);
}

async function setVisible(next: boolean): Promise<void> {
  if (visible === next) return;
  visible = next;
  document.body.classList.toggle('hidden', !next);
  await invoke('set_visible', { visible: next });
}

/** the window is on screen while at least one row has something to say */
function syncVisibility(): void {
  const any = SOURCES.some((source) => !rows[source].isIdle());
  settleSize();
  void setVisible(any && settings.showWidget);
}

/* -------------------------------------------------------------- toast hook */

async function notifyFinished(
  t: ThreadView,
  elapsed: number,
  alsoFinished: number,
): Promise<void> {
  if (!settings.notifyOnComplete) return;
  if (!(await isPermissionGranted())) {
    const permission = await requestPermission();
    if (permission !== 'granted') return;
  }
  const verb = t.status === 'aborted' ? '中断' : '完成';
  const extra = alsoFinished > 1 ? `（另有 ${alsoFinished - 1} 个）` : '';
  sendNotification({
    title: `${SOURCE_LABEL[t.source]} ${verb} · ${fmt(elapsed)}`,
    body: `${taskLabel(t)}${extra}`,
  });
}

/**
 * A capsule and a toast both assume you are looking at the screen. A turn that
 * ran past the configured threshold means you had long since looked away.
 */
function flashOverlayIfLong(
  t: ThreadView,
  elapsed: number,
  alsoFinished: number,
): void {
  const extra = alsoFinished > 1 ? `　+${alsoFinished - 1}` : '';
  const model = t.model ? ` · ${t.model}` : '';
  void invoke<boolean>('maybe_flash_overlay', {
    kind: t.status === 'aborted' ? 'aborted' : 'done',
    source: t.source,
    label: `${taskLabel(t)}${model}${extra}`,
    elapsed: Math.round(elapsed),
  });
}

/* ------------------------------------------------------------------ intake */

function applySource(source: Source, threads: ThreadView[]): void {
  const row = rows[source];
  const nowSec = Date.now() / 1000;

  const running = threads
    .filter((t) => t.status === 'running' && nowSec - t.started_at < MAX_AGE_SECS)
    .sort((a, b) => a.started_at - b.started_at);

  if (running.length > 0) {
    const earliest = running[0];
    // a turn can look stalled: keep showing it, but stop the breathing
    const stale = nowSec - earliest.last_activity > STALE_SECS;
    row.el.classList.toggle('stale', stale);
    row.showRunning(earliest.started_at * 1000, running.length);
    return;
  }
  row.el.classList.remove('stale');

  const finished = threads
    .filter((t) => t.status !== 'running' && t.completed_at !== null)
    .filter((t) => !announced.has(threadKey(t)))
    .sort((a, b) => (b.completed_at ?? 0) - (a.completed_at ?? 0));

  if (finished.length > 0) {
    for (const f of finished) announced.add(threadKey(f));
    row.showFinished(finished[0], finished.length);
    return;
  }

  // nothing new: leave the row as it is — a finished capsule keeps its five
  // minutes; only a turn that vanished without a completion goes back to idle
  if (row.mode === 'run') {
    row.setIdle();
    syncVisibility();
  }
}

function absorb(snapshot: Snapshot): void {
  const grouped: Record<Source, ThreadView[]> = { dsh: [], codex: [] };
  for (const thread of snapshot.threads) {
    if (thread.source === 'dsh' || thread.source === 'codex') {
      grouped[thread.source].push(thread);
    }
  }
  for (const source of SOURCES) applySource(source, grouped[source]);
  syncVisibility();
}

/* ------------------------------------------------------------------- clock */

function tick(): void {
  for (const source of SOURCES) rows[source].tick();
}

/* -------------------------------------------------------------------- drag
   Native `data-tauri-drag-region` would make the capsule a window caption and
   swallow the second click of a double-click, so dragging is done here: pointer
   capture, absolute screen coordinates, and a 4px threshold that leaves a still
   press as a click. */

const appWindow = getCurrentWindow();
let drag: {
  id: number;
  startX: number;
  startY: number;
  windowX: number;
  windowY: number;
  scale: number;
  moved: boolean;
} | null = null;
let lastPress = 0;

rowsEl.addEventListener('pointerdown', (event) => {
  if (event.button !== 0) return;

  const now = Date.now();
  if (now - lastPress < DOUBLE_CLICK_MS) {
    lastPress = 0;
    drag = null;
    hideAll();
    return;
  }
  lastPress = now;

  void Promise.all([appWindow.outerPosition(), appWindow.scaleFactor()]).then(
    ([position, scale]) => {
      drag = {
        id: event.pointerId,
        startX: event.screenX,
        startY: event.screenY,
        windowX: position.x,
        windowY: position.y,
        scale,
        moved: false,
      };
      rowsEl.setPointerCapture(event.pointerId);
    },
  );
});

rowsEl.addEventListener('pointermove', (event) => {
  if (drag === null || drag.id !== event.pointerId) return;
  const dx = (event.screenX - drag.startX) * drag.scale;
  const dy = (event.screenY - drag.startY) * drag.scale;
  if (!drag.moved && Math.abs(dx) < 4 && Math.abs(dy) < 4) return;
  drag.moved = true;
  void appWindow.setPosition(
    new PhysicalPosition(Math.round(drag.windowX + dx), Math.round(drag.windowY + dy)),
  );
});

function endDrag(event: PointerEvent): void {
  if (drag !== null && drag.id === event.pointerId) {
    try {
      rowsEl.releasePointerCapture(event.pointerId);
    } catch {
      /* capture may already be gone */
    }
    drag = null;
  }
}

rowsEl.addEventListener('pointerup', endDrag);
rowsEl.addEventListener('pointercancel', endDrag);

/** Hide everything, never quit: the tray keeps running. */
function hideAll(): void {
  for (const source of SOURCES) {
    rows[source].suppressCurrent();
    rows[source].setIdle();
  }
  syncVisibility();
}

/* ------------------------------------------------------------------- boot */

async function boot(): Promise<void> {
  const initial = await invoke<Snapshot>('snapshot');
  for (const t of initial.threads) {
    if (t.status !== 'running' && t.completed_at !== null) announced.add(threadKey(t));
  }
  settings = await invoke<Settings>('get_settings');

  // optional whale glyph: shipped separately because it is not ours to ship
  const whale = await invoke<string | null>('whale_asset').catch(() => null);
  if (whale) {
    document.documentElement.style.setProperty('--whale', `url(${whale})`);
    document.body.classList.add('has-whale');
  }

  await listen<Snapshot>('codex://update', (event) => absorb(event.payload));
  await listen<Settings>('settings://changed', (event) => {
    settings = event.payload;
    syncVisibility();
  });

  window.setInterval(tick, 500);

  await invoke('ready');
  await syncSize();
  absorb(initial);
}

void boot().catch((error) => {
  void invoke('js_log', { message: `boot failed: ${String(error)}` });
  document.body.classList.remove('hidden');
  const first = document.getElementById('row-codex') as HTMLDivElement;
  first.dataset.state = 'aborted';
  (first.querySelector('.time') as HTMLElement).textContent = 'ERR';
});

window.addEventListener('error', (event) => {
  void invoke('js_log', {
    message: `error: ${event.message} @ ${event.filename}:${event.lineno}`,
  });
});

window.addEventListener('unhandledrejection', (event) => {
  void invoke('js_log', { message: `rejection: ${String(event.reason)}` });
});