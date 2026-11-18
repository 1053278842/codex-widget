#!/usr/bin/env node
/**
 * Replays a synthetic Codex session so the widget can be demonstrated (and
 * regression-tested) without inviting a real agent to burn ten minutes.
 *
 *   node tools/fake-session.mjs --dir .tmp/codex --scenario complete
 *   CODEX_WIDGET_HOME=.tmp/codex npm run app:dev
 *
 * It writes exactly the records the real CLI writes — session_meta, task_started,
 * token_usage_record, task_complete / turn_aborted — into
 * `<dir>/sessions/YYYY/MM/DD/rollout-<timestamp>-<uuid>.jsonl`.
 */

import { mkdirSync, appendFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';

const args = new Map();
for (let i = 2; i < process.argv.length; i += 2) {
  const key = process.argv[i].replace(/^--/, '');
  args.set(key, process.argv[i + 1] ?? '');
}

const root = args.get('dir') ?? '.tmp/codex';
const scenario = args.get('scenario') ?? 'complete';
const runSeconds = Number(args.get('run-seconds') ?? 12);
const cwd = args.get('cwd') ?? join(process.cwd(), 'service');
const model = args.get('model') ?? 'your-model';
const provider = args.get('provider') ?? 'custom';
const title = args.get('title') ?? '给订单列表加一个指标条';

const now = new Date();
const day = String(now.getDate()).padStart(2, '0');
const month = String(now.getMonth() + 1).padStart(2, '0');
const year = String(now.getFullYear());
const dir = join(root, 'sessions', year, month, day);
mkdirSync(dir, { recursive: true });

const stamp = now.toISOString().replace(/[:.]/g, '-').slice(0, 19);
const file = join(dir, `rollout-${stamp}-${randomUUID()}.jsonl`);
writeFileSync(file, '', 'utf8');

const threadId = randomUUID();
const turnId = randomUUID();
const iso = () => new Date().toISOString();
const unix = () => Math.floor(Date.now() / 1000);

function emit(record) {
  appendFileSync(file, `${JSON.stringify(record)}\n`, 'utf8');
}

emit({
  timestamp: iso(),
  type: 'session_meta',
  payload: {
    id: threadId,
    session_id: threadId,
    timestamp: iso(),
    cwd,
    originator: 'codex_vscode',
    source: 'vscode',
    model_provider: provider,
    base_instructions: { provenance: { type: 'model', model } },
  },
});

emit({
  timestamp: iso(),
  type: 'event_msg',
  payload: { type: 'user_message', message: `## 任务\n${title}` },
});

const startedAt = unix();
emit({
  timestamp: iso(),
  type: 'event_msg',
  payload: { type: 'task_started', turn_id: turnId, started_at: startedAt },
});

let elapsed = 0;
const beat = setInterval(() => {
  elapsed += 1;
  emit({
    timestamp: iso(),
    type: 'token_usage_record',
    payload: { total_tokens: 1200 + elapsed * 350 },
  });
  emit({
    timestamp: iso(),
    type: 'event_msg',
    payload: {
      type: 'token_count',
      info: { total_token_usage: { total_tokens: 1200 + elapsed * 350 } },
    },
  });
}, 1000);

setTimeout(() => {
  clearInterval(beat);
  const completedAt = unix();
  if (scenario === 'abort') {
    emit({
      timestamp: iso(),
      type: 'event_msg',
      payload: { type: 'turn_aborted', turn_id: turnId, started_at: startedAt, completed_at: completedAt },
    });
    console.log(`[fake-session] aborted after ${runSeconds}s → ${file}`);
  } else {
    emit({
      timestamp: iso(),
      type: 'event_msg',
      payload: {
        type: 'task_complete',
        turn_id: turnId,
        started_at: startedAt,
        completed_at: completedAt,
        last_agent_message: '做完了。',
      },
    });
    console.log(`[fake-session] completed after ${runSeconds}s → ${file}`);
  }
  process.exit(0);
}, runSeconds * 1000);

console.log(`[fake-session] running, writing ${file}`);
console.log('[fake-session] 并发演示：同时启动多个本进程即可（每个进程写各自的文件，控件显示 ×N）');