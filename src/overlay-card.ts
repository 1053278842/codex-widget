/* ---------------------------------------------------------------------------
   Card renderer.

   One interaction, deliberately: clicking the card ends the whole overlay.
   Rust owns the windows, so this file just reports the click.
--------------------------------------------------------------------------- */

import './overlay-card.css';
import { listen } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';

interface Payload {
  kind: 'done' | 'aborted';
  label: string;
  elapsed: number;
}

const root = document.getElementById('root') as HTMLDivElement;
const card = document.getElementById('card') as HTMLDivElement;
const elapsedEl = document.getElementById('elapsed') as HTMLDivElement;
const taskEl = document.getElementById('task') as HTMLDivElement;
const eyebrowEl = root.querySelector('.eyebrow-text') as HTMLElement;

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
}

card.addEventListener('click', () => {
  if (!root.classList.contains('show')) return;
  // fade first so the click feels acknowledged, Rust hides the windows
  root.classList.remove('show');
  root.classList.add('closing');
  void invoke('dismiss_overlay');
});

void listen<Payload>('overlay://play', (event) => play(event.payload));
void listen('overlay://closing', () => {
  root.classList.remove('show');
  root.classList.add('closing');
});

void invoke<Payload | null>('overlay_last').then((payload) => {
  if (payload) play(payload);
});