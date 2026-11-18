/* ---------------------------------------------------------------------------
   Visual layer renderer (no card, nothing clickable here).

   Shows the vignette + pulsing rim, and keeps showing it. Rust hides this
   window only when the card is clicked or a new turn starts.
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

function play(payload: Payload): void {
  root.dataset.kind = payload.kind;
  root.classList.remove('closing');
  // restart the pulse from the top if it is already running
  root.classList.remove('run');
  void root.offsetWidth;
  root.classList.add('run');
}

void listen<Payload>('overlay://play', (event) => play(event.payload));

void listen('overlay://closing', () => {
  root.classList.remove('run');
  root.classList.add('closing');
});

// only answers while the overlay is actually open, so a late-loading webview
// cannot resurrect a dismissed overlay
void invoke<Payload | null>('overlay_last').then((payload) => {
  if (payload) play(payload);
});