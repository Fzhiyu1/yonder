#!/usr/bin/env node
// Deterministic stand-in for `pi --mode rpc` used by the yonder end-to-end tests.
// Speaks the subset of pi's RPC protocol the yonder adapter uses:
// get_state / get_messages / prompt / abort / extension_ui_response.
//
// Behavior per prompt text:
//   "ask ..."   -> extension_ui_request confirm; on confirm replies "approved", else "denied"
//   "slow ..."  -> streams 40 deltas 50 ms apart (interruptible with abort)
//   otherwise   -> replies "echo: <text>" with a bash tool call in between
import { createInterface } from 'node:readline';

const out = (o) => process.stdout.write(JSON.stringify(o) + '\n');
const sessionId = 'fake-' + Math.random().toString(16).slice(2, 10);
// `--model provider/id` (what the host passes for a model picked in the UI) is reported back.
const modelArg = process.argv.indexOf('--model') > 0 ? process.argv[process.argv.indexOf('--model') + 1] : '';
const model = modelArg.includes('/')
  ? { provider: modelArg.slice(0, modelArg.indexOf('/')), id: modelArg.slice(modelArg.indexOf('/') + 1) }
  : { provider: 'fake', id: modelArg || 'fake-model' };
let busy = false;
let aborted = false;
let pendingUi = null;
let msgSeq = 0;
const history = [];

function state() {
  return { sessionId, model, isStreaming: busy };
}

function assistantText(text) {
  msgSeq += 1;
  out({ type: 'message_start', message: { role: 'assistant' } });
  out({ type: 'message_update', assistantMessageEvent: { type: 'text_start', contentIndex: 0 } });
  out({ type: 'message_update', assistantMessageEvent: { type: 'text_delta', contentIndex: 0, delta: text } });
  out({ type: 'message_update', assistantMessageEvent: { type: 'text_end', contentIndex: 0, content: text } });
  out({ type: 'message_end', message: { role: 'assistant', provider: 'fake', model: 'fake-model', stopReason: 'stop' } });
  history.push({ role: 'assistant', content: [{ type: 'text', text }] });
}

function endTurn() {
  busy = false;
  out({ type: 'agent_end' });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function runPrompt(text) {
  busy = true;
  aborted = false;
  history.push({ role: 'user', content: text });
  out({ type: 'agent_start' });
  if (text.startsWith('ask')) {
    pendingUi = 'ui-' + msgSeq;
    out({ type: 'extension_ui_request', id: pendingUi, method: 'confirm', title: 'Run fake command?', message: 'fake-cmd --dangerous' });
    return; // continues on extension_ui_response
  }
  if (text.startsWith('slow')) {
    msgSeq += 1;
    out({ type: 'message_start', message: { role: 'assistant' } });
    out({ type: 'message_update', assistantMessageEvent: { type: 'text_start', contentIndex: 0 } });
    for (let i = 0; i < 40 && !aborted; i++) {
      out({ type: 'message_update', assistantMessageEvent: { type: 'text_delta', contentIndex: 0, delta: `chunk${i} ` } });
      await sleep(50);
    }
    out({ type: 'message_end', message: { role: 'assistant', stopReason: aborted ? 'error' : 'stop', errorMessage: aborted ? 'Request was aborted' : undefined } });
    endTurn();
    return;
  }
  const tool = 'call-' + msgSeq;
  out({ type: 'tool_execution_start', toolCallId: tool, toolName: 'bash', args: { command: 'echo fake' } });
  out({ type: 'tool_execution_update', toolCallId: tool, partialResult: { content: [{ type: 'text', text: 'fake\n' }] } });
  out({ type: 'tool_execution_end', toolCallId: tool, toolName: 'bash', isError: false, result: { content: [{ type: 'text', text: 'fake\n' }] } });
  assistantText('echo: ' + text);
  endTurn();
}

const rl = createInterface({ input: process.stdin });
rl.on('line', (line) => {
  let m;
  try {
    m = JSON.parse(line);
  } catch {
    return;
  }
  switch (m.type) {
    case 'get_state':
      out({ type: 'response', id: m.id, command: 'get_state', success: true, data: state() });
      break;
    case 'get_messages':
      out({ type: 'response', id: m.id, command: 'get_messages', success: true, data: { messages: history } });
      break;
    case 'prompt':
      out({ type: 'response', id: m.id, command: 'prompt', success: true });
      runPrompt(m.message ?? '');
      break;
    case 'abort':
      aborted = true;
      out({ type: 'response', id: m.id, command: 'abort', success: true });
      if (pendingUi) {
        pendingUi = null;
        endTurn();
      }
      break;
    case 'extension_ui_response':
      if (pendingUi && m.id === pendingUi) {
        pendingUi = null;
        assistantText(m.confirmed ? 'approved' : 'denied');
        endTurn();
      }
      break;
    default:
      out({ type: 'response', id: m.id, command: m.type, success: false, error: 'unsupported' });
  }
});
rl.on('close', () => process.exit(0));
