/* ---------------------------------------------------------------------------
   Full-screen overlay renderer.

   Rust decides *whether* to show this (setting on + turn ran ≥10 minutes);
   this file only plays the 5.6s sequence and then asks Rust to hide the window
   again. Nothing here takes input: the whole layer is pointer-events: none and
   the window itself is click-through.
--------------------------------------------------------------------------- */

import './overlay.css';
import { listen } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';

interface Payload {
  kind: 'done' | 'aborted';
  label: string;
  elapsed: number;
}

/** must match the CSS timeline (last animation ends at 5.4s) */
const PLAY_MS = 5_600;

const root = document.getElementById('overlay') as HTMLDivElement;
const elapsedEl = document.getElementById('elapsed') as HTMLDivElement;
const taskEl = document.getElementById('task') as HTMLDivElement;

function fmt(totalSeconds: number): string {
  const s = Math.max(0, Math.floor(totalSeconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const pad = (n: number) => String(n).padStart(2, '0');
  return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${pad(m)}:${pad(sec)}`;
}

let hideTimer = 0;

function play(payload: Payload): void {
  window.clearTimeout(hideTimer);

  root.dataset.kind = payload.kind;
  elapsedEl.textContent = fmt(payload.elapsed);
  taskEl.textContent = payload.label;
  (root.querySelector('.eyebrow-text') as HTMLElement).textContent =
    payload.kind === 'aborted' ? '任务中断' : '任务完成';

  // restart every CSS animation from frame zero
  root.classList.remove('run');
  void root.offsetWidth;
  root.classList.add('run');

  hideTimer = window.setTimeout(async () => {
    root.classList.remove('run');
    await invoke('hide_overlay');
  }, PLAY_MS);
}

void listen<Payload>('overlay://play', (event) => play(event.payload));

// a fresh window may have missed an emit that happened while it was loading
void invoke<Payload | null>('overlay_last').then((payload) => {
  if (payload) play(payload);
});