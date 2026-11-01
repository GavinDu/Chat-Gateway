import { test } from 'node:test';
import assert from 'node:assert/strict';
import { Session } from './navigation-guard.mjs';

test('CDP target RPC correlates responses without forwarding protocol details', async () => {
  const calls = [];
  const parent = { send: async (method, params) => calls.push({ method, params }) };
  const client = new Session(parent, 'fixture-session');
  const first = client.send('Page.enable');
  const second = client.send('Fetch.enable');
  client.receive(JSON.stringify({ id: 2, result: { enabled: true } }));
  client.receive(JSON.stringify({ id: 1, result: {} }));
  assert.deepEqual(await first, {});
  assert.deepEqual(await second, { enabled: true });
  assert(calls.every(c => c.method === 'Target.sendMessageToTarget'));
  assert.equal(client.pending.size, 0);
  client.dispose();
});
test('CDP errors are constant and never forward raw protocol messages', async () => {
  const client = new Session({ send: async () => {} }, 'fixture-session');
  const pending = client.send('Page.enable');
  client.receive(JSON.stringify({ id: 1, error: { message: 'synthetic-private-detail' } }));
  await assert.rejects(pending, { message: 'guard_unavailable' });
  client.dispose();
});
test('target detach rejects pending commands and refuses new controls', async () => {
  const client = new Session({ send: async () => {} }, 'fixture-session');
  const pending = client.send('Page.enable');
  client.dispose();
  await assert.rejects(pending, { message: 'guard_closed' });
  await assert.rejects(client.send('Page.enable'), { message: 'guard_closed' });
  assert.equal(client.pending.size, 0);
});
test('child rejection is handled even if parent transport ACK never arrives', async () => {
  const client = new Session({ send: () => new Promise(() => {}) }, 'fixture-session');
  const pending = client.send('Page.enable');
  await Promise.resolve();
  client.receive(JSON.stringify({ id: 1, error: { message: 'synthetic-private-detail' } }));
  await assert.rejects(pending, { message: 'guard_unavailable' });
  client.dispose();
});
