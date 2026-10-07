#!/usr/bin/env node
// Deterministic stand-in for `codex app-server` used by the yonder end-to-end tests.
// Speaks the subset of the JSON-RPC protocol the yonder adapter uses: initialize,
// thread/start|resume, turn/start (with approvalPolicy / sandboxPolicy overrides),
// turn/interrupt, and item/commandExecution/requestApproval.
//
// A prompt "run <cmd>" runs a command: with approval policy "never" it runs right away,
// otherwise it asks first. The reply says which way it went ("ran <cmd> without asking",
// "ran <cmd> after approval", "skipped <cmd>") plus the policy and sandbox in effect.
// "slow <x>" takes 3 s before it answers. `turn/steer` adds text to a running turn, unless
// FAKE_CODEX_NO_STEER=1 (like Codex 0.72, which rejects it as an unknown method).
import { createInterface } from 'node:readline';

const out = (o) => process.stdout.write(JSON.stringify(o) + '\n');
const thread = 'fake-thread-' + Math.random().toString(16).slice(2, 8);
const noSteer = process.env.FAKE_CODEX_NO_STEER === '1';
let policy = 'on-request';
let sandbox = 'workspace-write';
let turnSeq = 0;
let rpcSeq = 1000;
const waiting = new Map(); // server request id -> resume(decision)
let running = null; // { id, steered: [] } while a slow turn runs

function notify(method, params) {
  out({ jsonrpc: '2.0', method, params });
}

function reply(id, result) {
  out({ jsonrpc: '2.0', id, result });
}

function agentMessage(turn, text) {
  const id = `msg-${turn}`;
  notify('item/started', { threadId: thread, turnId: turn, item: { type: 'agentMessage', id, text: '' } });
  notify('item/completed', { threadId: thread, turnId: turn, item: { type: 'agentMessage', id, text } });
}

function command(turn, cmd, then) {
  const id = `cmd-${turn}`;
  notify('item/started', { threadId: thread, turnId: turn, item: { type: 'commandExecution', id, command: cmd, cwd: '/tmp', status: 'inProgress' } });
  const finish = (ran) => {
    notify('item/completed', {
      threadId: thread,
      turnId: turn,
      item: { type: 'commandExecution', id, command: cmd, cwd: '/tmp', status: ran ? 'completed' : 'declined', exitCode: ran ? 0 : null, aggregatedOutput: ran ? 'ok\n' : '' },
    });
    then(ran);
  };
  if (policy === 'never') return finish(true);
  const rid = rpcSeq++;
  waiting.set(rid, (decision) => finish(decision === 'accept' || decision === 'acceptForSession'));
  out({ jsonrpc: '2.0', id: rid, method: 'item/commandExecution/requestApproval', params: { threadId: thread, turnId: turn, itemId: id, command: cmd, cwd: '/tmp', reason: 'fake approval' } });
}

function startTurn(text) {
  const turn = `turn-${++turnSeq}`;
  notify('turn/started', { threadId: thread, turn: { id: turn, status: 'inProgress', items: [] } });
  const done = (msg) => {
    agentMessage(turn, `${msg} [policy=${policy} sandbox=${sandbox}]`);
    notify('turn/completed', { threadId: thread, turn: { id: turn, status: 'completed', items: [] } });
  };
  const slow = /^slow (.+)$/.exec(text.trim());
  if (slow) {
    running = { id: turn, steered: [] };
    setTimeout(() => {
      const steered = running.steered.length ? ` + steered: ${running.steered.join(' | ')}` : '';
      running = null;
      done(`slow done: ${slow[1]}${steered}`);
    }, 3000);
    return turn;
  }
  const m = /^run (.+)$/.exec(text.trim());
  if (!m) {
    done(`echo: ${text}`);
    return turn;
  }
  const asked = policy !== 'never';
  command(turn, m[1], (ran) => done(ran ? (asked ? `ran ${m[1]} after approval` : `ran ${m[1]} without asking`) : `skipped ${m[1]}`));
  return turn;
}

const rl = createInterface({ input: process.stdin });
rl.on('line', (line) => {
  let m;
  try {
    m = JSON.parse(line);
  } catch {
    return;
  }
  if (m.method === undefined && waiting.has(m.id)) {
    const resume = waiting.get(m.id);
    waiting.delete(m.id);
    resume(m.result?.decision);
    return;
  }
  const p = m.params ?? {};
  switch (m.method) {
    case 'initialize':
      reply(m.id, { userAgent: 'fake-codex/0' });
      break;
    case 'initialized':
      break;
    case 'thread/start':
    case 'thread/resume':
      if (p.approvalPolicy) policy = p.approvalPolicy;
      if (p.sandbox) sandbox = p.sandbox;
      reply(m.id, { thread: { id: p.threadId ?? thread, turns: [] }, model: p.model ?? 'fake-model', approvalPolicy: policy, sandbox: { type: sandbox } });
      break;
    case 'turn/start': {
      if (p.approvalPolicy) policy = p.approvalPolicy;
      if (p.sandboxPolicy?.type) sandbox = { dangerFullAccess: 'danger-full-access', workspaceWrite: 'workspace-write', readOnly: 'read-only' }[p.sandboxPolicy.type] ?? sandbox;
      const text = (p.input ?? []).map((i) => i.text ?? '').join('');
      reply(m.id, { turn: { id: `turn-${turnSeq + 1}`, status: 'inProgress', items: [] } });
      startTurn(text);
      break;
    }
    case 'turn/steer': {
      if (noSteer) {
        out({ jsonrpc: '2.0', id: m.id, error: { code: -32600, message: 'Invalid request: unknown variant `turn/steer`, expected one of `initialize`, `thread/start`' } });
        break;
      }
      if (!running || running.id !== p.expectedTurnId) {
        out({ jsonrpc: '2.0', id: m.id, error: { code: -32600, message: `expected active turn id ${p.expectedTurnId} but found none` } });
        break;
      }
      running.steered.push((p.input ?? []).map((i) => i.text ?? '').join(''));
      reply(m.id, {});
      break;
    }
    case 'turn/interrupt':
      reply(m.id, {});
      break;
    default:
      if (m.id !== undefined) out({ jsonrpc: '2.0', id: m.id, error: { code: -32601, message: `unsupported: ${m.method}` } });
  }
});
rl.on('close', () => process.exit(0));
