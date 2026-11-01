// Private bounded stdio actor. Input/state/errors are never logged.
import { createInterface } from 'node:readline';
import { LoginEngine } from './engine.mjs';
let ending = false, queued = 0, chain = Promise.resolve(), closing;
const emit = value => {
  if (ending) return;
  if (value.type === 'frame' && process.stdout.writableLength > 512 * 1024) return;
  process.stdout.write(JSON.stringify(value) + '\n');
};
const engine = new LoginEngine(emit);
function stop(code = 0) {
  if (closing) return closing;
  ending = true;
  closing = engine.close().finally(() => process.exit(code));
  return closing;
}
const safeCodes = new Set(['navigation_blocked', 'invalid_input', 'invalid_resize',
  'no_provider_session', 'invalid_start', 'browser_not_ready', 'page_limit']);
createInterface({ input: process.stdin, crlfDelay: Infinity }).on('line', line => {
  if (ending) return;
  if (line.length > 2 * 1024 * 1024) return void stop(1);
  let message;
  try { message = JSON.parse(line); } catch {
    emit({ type: 'error', code: 'invalid_input' }); return;
  }
  if (message.type === 'cancel') return void stop();
  if (queued >= 64) { emit({ type: 'error', code: 'input_queue_full' }); return; }
  queued += 1;
  chain = chain.then(async () => {
    if (!ending) await engine.dispatch(message);
  }).catch(error => {
    emit({ type: 'error', code: safeCodes.has(error.message) ?
      error.message : 'browser_runtime_unavailable' });
    if (!engine.context || engine.closed) void stop(1);
  }).finally(() => { queued -= 1; });
}).on('close', () => void stop());
process.on('SIGTERM', () => void stop());
process.on('SIGINT', () => void stop());
process.on('uncaughtException', () => void stop(1));
process.on('unhandledRejection', () => void stop(1));
