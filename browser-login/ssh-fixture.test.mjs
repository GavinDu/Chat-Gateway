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

test('P-256 fixture keys parse and prove signed challenge, not reused host keys', () => {
  const { utils } = ssh2;
  for (let n = 0; n < 32; n++) {
    const keys = generateFixtureKeyPair();
    const privateKey = utils.parseKey(keys.private);
    const publicKey = utils.parseKey(keys.public);
    assert.ok(!(privateKey instanceof Error), 'private key must parse');
    assert.ok(!(publicKey instanceof Error), 'public key must parse');
    assert.equal(privateKey.type, 'ecdsa-sha2-nistp256');
    assert.equal(publicKey.type, 'ecdsa-sha2-nistp256');
    assert.deepEqual(privateKey.getPublicSSH(), publicKey.getPublicSSH());
    const challenge = Buffer.from('isolated fixture authentication');
    const signature = privateKey.sign(challenge);
    assert.ok(Buffer.isBuffer(signature));
    assert.equal(publicKey.verify(challenge, signature), true);
    assert.equal(publicKey.verify(Buffer.from('different challenge'), signature), false);
  }
});

test('malformed generated fixture key fails with fixed redacted category and removes owned temp', async () => {
  await expectStartupCategory({
    generateFixtureKeyPair() {
      return {private:'not-a-private-key secret',public:'not-a-public-key secret'};
    }
  }, 'startup_unclassified');
});

test('fixture key-generator exception is redacted and cleans up its isolated state', async () => {
  await expectStartupCategory({
    generateFixtureKeyPair() {
      throw new Error('private random key data that must never leak');
    }
  }, 'startup_unclassified');
});
