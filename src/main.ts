/* ---------------------------------------------------------------------------
   Codex Widget — renderer.

   Rust watches ~/.codex/sessions and pushes a snapshot roughly twice a second.
   This file owns presentation only:

     • running  — amber dot breathing + elapsed timer, aggregate ×N
     • finished — green/red dot, one halo, and it STAYS there until you
                  double-click it. No auto-dismiss: a completion you were not
                  looking at is still a completion.
     • idle     — invisible and click-through

   Long turns additionally hand over to the full-screen reminder (see overlay.ts),
   which any click or key dismisses.
--------------------------------------------------------------------------- */

import './styles.css';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from '@tauri-apps/plugin-notification';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { PhysicalPosition } from '@tauri-apps/api/dpi';

type Status = 'running' | 'done' | 'aborted';
type Mode = 'idle' | 'run' | 'done' | 'aborted';

interface ThreadView {
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

const STALE_SECS = 900; // no event for this long ⇒ stop pretending it is alive
const MAX_AGE_SECS = 86_400; // ignore turns older than a day

const pill = document.getElementById('pill') as HTMLDivElement;
const timeEl = document.getElementById('time') as HTMLSpanElement;
const countEl = document.getElementById('count') as HTMLSpanElement;

let mode: Mode = 'idle';
/** epoch ms the currently shown running turn started */
let runningSince = 0;
/** elapsed seconds to freeze on the capsule when a turn ends */
let frozenElapsed = 0;
let visible = false;
let settings: Settings = {
  notifyOnComplete: true,
  showWidget: true,
  overlayThresholdSecs: 600,
};
let lastSentSize = '';

const announced = new Set<string>();
const threadKey = (t: ThreadView) => `${t.thread_id}|${t.completed_at ?? 0}`;

/** what the widget is showing right now — used to honour a manual hide */
let shownKey = '';
/** the state the user double-clicked away; it stays hidden until this changes */
let suppressedKey: string | null = null;

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
  return fromTitle.length > 48 ? `${fromTitle.slice(0, 48)}…` : fromTitle || 'Codex';
}

/* ------------------------------------------------------------------ window */

async function syncSize(): Promise<void> {
  const rect = pill.getBoundingClientRect();
  const width = Math.ceil(rect.width) + 12; // body padding, keeps the shadow
  const height = Math.ceil(rect.height) + 12;
  const key = `${width}x${height}`;
  if (key === lastSentSize) return;
  lastSentSize = key;
  await invoke('resize_to', { width, height });
}

async function setVisible(next: boolean): Promise<void> {
  if (visible === next) return;
  visible = next;
  // The window itself never hides: hiding and re-showing it would steal focus
  // from whatever you are typing in. Idle = faded out + click-through instead.
  document.body.classList.toggle('hidden', !next);
  await invoke('set_visible', { visible: next });
}

/** width changes by a few pixels with ×N; re-measure rather than guess */
function settleSize(): void {
  void syncSize();
  window.setTimeout(() => void syncSize(), 120);
  window.setTimeout(() => void syncSize(), 420);
}

/* ------------------------------------------------------------------- state */

function applyMode(next: Mode): void {
  mode = next;
  pill.dataset.state = next;

}

/* ------------------------------------------------------------------- drag
   Dragging is done here rather than with `data-tauri-drag-region`, because that
   attribute makes the whole pill a window-caption: Windows turns the second
   click of a double-click into WM_NCLBUTTONDBLCLK, which never reaches the
   renderer — and the double-click is how you dismiss the capsule.

   Pointer capture keeps the events coming even after the capsule slides out
   from under the cursor, and `screenX/screenY` are absolute, so moving the
   window does not feed back into the delta. */

const appWindow = getCurrentWindow();
const DOUBLE_CLICK_MS = 350;

interface DragState {
  id: number;
  startX: number;
  startY: number;
  windowX: number;
  windowY: number;
  scale: number;
  moved: boolean;
}

let drag: DragState | null = null;
let lastPress = 0;

pill.addEventListener('pointerdown', (event) => {
  if (event.button !== 0) return;

  const now = Date.now();
  if (now - lastPress < DOUBLE_CLICK_MS) {
    lastPress = 0;
    drag = null;
    hideCapsule();
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
      pill.setPointerCapture(event.pointerId);
    },
  );
});

pill.addEventListener('pointermove', (event) => {
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
      pill.releasePointerCapture(event.pointerId);
    } catch {
      /* capture may already be gone */
    }
    drag = null;
  }
}

pill.addEventListener('pointerup', endDrag);
pill.addEventListener('pointercancel', endDrag);

/** Hide, never quit: the tray keeps running and the next turn comes back. */
function hideCapsule(): void {
  suppressedKey = shownKey || null;
  setIdle();
}

function showRunning(earliestStartMs: number, count: number): void {
  const key = `run:${earliestStartMs}:${count}`;
  if (suppressedKey === key) return;

  if (mode !== 'run') {
    applyMode('run');
    timeEl.textContent = '00:00';
    // a new turn means the previous reminder has been dealt with in practice
    void invoke('dismiss_overlay');
  }
  shownKey = key;
  runningSince = earliestStartMs;
  countEl.textContent = `×${count}`;
  pill.dataset.count = String(count);
  timeEl.textContent = fmt((Date.now() - runningSince) / 1000);
  settleSize();
  void setVisible(settings.showWidget);
}

function showFinished(t: ThreadView, alsoFinished: number): void {
  const key = `fin:${threadKey(t)}`;
  if (suppressedKey === key) return;

  const startMs = t.started_at * 1000;
  const endMs = (t.completed_at ?? t.last_activity) * 1000;
  frozenElapsed = Math.max(0, (endMs - startMs) / 1000);
  const finished = t.status === 'aborted' ? 'aborted' : 'done';

  shownKey = key;
  timeEl.textContent = fmt(frozenElapsed);
  countEl.textContent = `×${alsoFinished}`;
  pill.dataset.count = String(alsoFinished);
  applyMode(finished);

  // one-shot halo; replaying requires a reflow so the animation restarts
  pill.classList.remove('peak');
  void pill.offsetWidth;
  pill.classList.add('peak');
  window.setTimeout(() => pill.classList.remove('peak'), 950);

  settleSize();
  void setVisible(settings.showWidget);
  void notifyFinished(t, alsoFinished);
  void flashOverlayIfLong(t, finished, alsoFinished);
}

function setIdle(): void {
  applyMode('idle');
  pill.dataset.count = '1';
  countEl.textContent = '×1';
  shownKey = '';
  void setVisible(false);
}

/* -------------------------------------------------------------- toast hook */

async function notifyFinished(t: ThreadView, alsoFinished: number): Promise<void> {
  if (!settings.notifyOnComplete) return;
  if (!(await isPermissionGranted())) {
    const permission = await requestPermission();
    if (permission !== 'granted') return;
  }
  const verb = t.status === 'aborted' ? '中断' : '完成';
  const extra = alsoFinished > 1 ? `（另有 ${alsoFinished - 1} 个）` : '';
  sendNotification({
    title: `Codex ${verb} · ${fmt(frozenElapsed)}`,
    body: `${taskLabel(t)}${extra}`,
  });
}

/**
 * A capsule and a toast both assume you are looking at the screen. A turn that
 * ran past the configured threshold means you had long since looked away, so
 * hand it to the full-screen reminder — Rust owns the threshold and the toggle.
 */
function flashOverlayIfLong(
  t: ThreadView,
  kind: 'done' | 'aborted',
  alsoFinished: number,
): void {
  const extra = alsoFinished > 1 ? `　+${alsoFinished - 1}` : '';
  const model = t.model ? ` · ${t.model}` : '';
  void invoke<boolean>('maybe_flash_overlay', {
    kind,
    label: `${taskLabel(t)}${model}${extra}`,
    elapsed: Math.round(frozenElapsed),
  });
}

/* ------------------------------------------------------------------ intake */

function absorb(snapshot: Snapshot): void {
  const nowSec = Date.now() / 1000;

  const running = snapshot.threads
    .filter((t) => t.status === 'running' && nowSec - t.started_at < MAX_AGE_SECS)
    .sort((a, b) => a.started_at - b.started_at);

  if (running.length > 0) {
    const earliest = running[0];
    // a turn can look stalled: keep showing it, but stop the breathing
    const stale = nowSec - earliest.last_activity > STALE_SECS;
    pill.classList.toggle('stale', stale);
    showRunning(earliest.started_at * 1000, running.length);
    return;
  }
  pill.classList.remove('stale');

  // nothing running: surface the newest turn we have not announced yet
  const finished = snapshot.threads
    .filter((t) => t.status !== 'running' && t.completed_at !== null)
    .filter((t) => !announced.has(threadKey(t)))
    .sort((a, b) => (b.completed_at ?? 0) - (a.completed_at ?? 0));

  if (finished.length > 0) {
    for (const f of finished) announced.add(threadKey(f));
    showFinished(finished[0], finished.length);
    return;
  }

  // Nothing new. A finished capsule stays on screen indefinitely; only a turn
  // that vanished without a completion gets sent back to idle.
  if (mode === 'run') setIdle();
}

/* ------------------------------------------------------------------- clock */

function tick(): void {
  if (mode === 'run') {
    timeEl.textContent = fmt((Date.now() - runningSince) / 1000);
  }
  // finished states are frozen on purpose — no timer, no auto-hide
}

/* ------------------------------------------------------------------- boot */

async function boot(): Promise<void> {
  // adopt the current backlog without firing a toast for history
  const initial = await invoke<Snapshot>('snapshot');
  for (const t of initial.threads) {
    if (t.status !== 'running' && t.completed_at !== null) announced.add(threadKey(t));
  }
  settings = await invoke<Settings>('get_settings');
  if (settings.showWidget) applyMode('idle');

  await listen<Snapshot>('codex://update', (event) => absorb(event.payload));
  await listen<Settings>('settings://changed', (event) => {
    settings = event.payload;
    if (!settings.showWidget) setIdle();
  });

  window.setInterval(tick, 500);

  // let Rust park and reveal the (still faded-out) window now that paint exists
  await invoke('ready');
  await syncSize();
}

void boot().catch((error) => {
  void invoke('js_log', { message: `boot failed: ${String(error)}` });
  // last resort: put the error on screen instead of staying silently invisible
  document.body.classList.remove('hidden');
  pill.dataset.state = 'aborted';
  timeEl.textContent = 'ERR';
});

window.addEventListener('error', (event) => {
  void invoke('js_log', {
    message: `error: ${event.message} @ ${event.filename}:${event.lineno}`,
  });
});

window.addEventListener('unhandledrejection', (event) => {
  void invoke('js_log', { message: `rejection: ${String(event.reason)}` });
});