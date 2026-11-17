import test from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { PassThrough } from 'node:stream';
import { stat } from 'node:fs/promises';
import ssh2 from 'ssh2';
import { startOwnerTunnel, generateFixtureKeyPair } from './ssh-fixture.mjs';

// No real owner credentials, existing SSH config, or provider browser state:
// every test creates its own ephemeral fixture server and injects only the
// child process / bind failure before the tunnel is ever used.
function fakeHelper({exitImmediately = false} = {}) {
  const child = new EventEmitter();
  child.stdout = new PassThrough();
  child.exitCode = null;
  child.kill = () => {
    child.exitCode = 143;
    child.emit('exit', 143);
    return true;
  };
  if (exitImmediately) queueMicrotask(() => {
    child.exitCode = 1;
    child.emit('exit', 1);
  });
  return child;
}

async function expectStartupCategory(hooks, category) {
  let ownedTemp = null;
  await assert.rejects(
    startOwnerTunnel(8099, {
      ...hooks,
      onTemporaryDirectory: path => { ownedTemp = path; }
    }),
    error => error.fixtureCategory === category &&
      error.message === 'Isolated SSH fixture startup unavailable.'
  );
  assert.ok(ownedTemp, 'the fixture must create an isolated temporary directory');
  await assert.rejects(stat(ownedTemp), {code:'ENOENT'});
}

test('SSH fixture classifies bind failure and removes isolated keys', async () => {
  await expectStartupCategory({
    listenServer(_server, _resolve, reject) { reject(new Error('secret bind details')); }
  }, 'server_bind_failed');
});

test('SSH fixture classifies child exit before ready and cleans up', async () => {
  await expectStartupCategory({
    spawnHelper() { return fakeHelper({exitImmediately:true}); }
  }, 'helper_exited_before_ready');
});

test('SSH fixture bounds silent helper readiness and terminates child', async () => {
  let killed = false;
  await expectStartupCategory({
    startupTimeoutMs:10,
    spawnHelper() {
      const child = fakeHelper();
      const originalKill = child.kill;
      child.kill = signal => { killed = true; return originalKill(signal); };
      return child;
    }
  }, 'helper_ready_timeout');
  assert.equal(killed, true, 'a timed-out child must be stopped');
});

test('SSH fixture classifies synchronous helper spawn failure without raw stderr', async () => {
  await expectStartupCategory({
    spawnHelper() { throw new Error('private external path and token'); }
  }, 'helper_spawn_failed');
});

test('fixture keys retain the P-256 format and authenticate with matching signatures', () => {
  const keys = generateFixtureKeyPair();
  const privateKey = ssh2.utils.parseKey(keys.private);
  const publicKey = ssh2.utils.parseKey(keys.public);
  assert.ok(!(privateKey instanceof Error), 'fixture private key must parse');
  assert.ok(!(publicKey instanceof Error), 'fixture public key must parse');
  assert.equal(privateKey.type, 'ecdsa-sha2-nistp256');
  assert.equal(publicKey.type, 'ecdsa-sha2-nistp256');
  assert.deepEqual(privateKey.getPublicSSH(), publicKey.getPublicSSH());
  const message = Buffer.from('isolated fixture authentication');
  const signature = privateKey.sign(message);
  assert.ok(Buffer.isBuffer(signature));
  assert.equal(publicKey.verify(message, signature), true);
  assert.equal(publicKey.verify(Buffer.from('altered challenge'), signature), false);
});
