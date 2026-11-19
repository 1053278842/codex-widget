/* ---------------------------------------------------------------------------
   Full-screen reminder renderer.

   The whole window is one giant dismiss button: a click anywhere, or any key,
   ends it. Rust focuses the window when it appears, which is what makes keys
   arrive here instead of at whatever you were typing in.
--------------------------------------------------------------------------- */

import './overlay.css';
import { listen } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';

interface Payload {
  kind: 'done' | 'aborted';
  label: string;
  elapsed: number;
}

const root = document.getElementById('overlay') as HTMLDivElement;
const elapsedEl = document.getElementById('elapsed') as HTMLDivElement;
const taskEl = document.getElementById('task') as HTMLDivElement;
const eyebrowEl = document.getElementById('eyebrow') as HTMLElement;

let armed = false;

function fmt(totalSeconds: number): string {
  const s = Math.max(0, Math.floor(totalSeconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const pad = (n: number) => String(n).padStart(2, '0');
  return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${pad(m)}:${pad(sec)}`;
}

function play(payload: Payload): void {
  root.dataset.kind = payload.kind;
  eyebrowEl.textContent = payload.kind === 'aborted' ? '任务中断' : '任务完成';
  elapsedEl.textContent = fmt(payload.elapsed);
  taskEl.textContent = payload.label;

  root.classList.remove('closing');
  root.classList.remove('show');
  void root.offsetWidth;
  root.classList.add('show');
  armed = true;
}

function dismiss(reason: string): void {
  // the window appears a moment before the first paint of `show`; ignoring the
  // very first input prevents a stray key from a previous window closing it
  if (!armed) return;
  armed = false;
  void invoke('js_log', { message: `overlay dismissed (${reason})` });
  root.classList.remove('show');
  root.classList.add('closing');
  void invoke('dismiss_overlay');
}

window.addEventListener('mousedown', (event) => dismiss(`click ${event.button}`));

window.addEventListener('keydown', (event) => {
  // An IME (Sogou, Microsoft Pinyin, …) emits keydown with an empty or
  // "Process" key the instant a window takes focus. Treating that as "any key"
  // made the reminder dismiss itself the moment it appeared.
  if (event.isComposing || event.keyCode === 229) return;
  if (!event.key || event.key === 'Process' || event.key === 'Unidentified') return;
  if (!event.code || event.code === 'Unidentified') return;
  dismiss(`key ${event.key}`);
});

window.addEventListener('auxclick', () => dismiss('auxclick'));

window.addEventListener('auxclick', () => dismiss('auxclick'));

void listen<Payload>('overlay://play', (event) => play(event.payload));
void listen('overlay://closing', () => {
  armed = false;
  root.classList.remove('show');
  root.classList.add('closing');
});

void invoke<Payload | null>('overlay_last').then((payload) => {
  if (payload) play(payload);
});