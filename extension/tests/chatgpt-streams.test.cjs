const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const script = fs.readFileSync(
  path.join(__dirname, '../../src/browser/pool/chatgpt_streams.js'), 'utf8'
);

// Models the pinned Obscura bootstrap's fallback stream shape: start/enqueue,
// _reads/_queue and getReader exist; underlyingSource.pull is never invoked.
// Do not substitute Node's native WHATWG implementation for this regression.
class PinnedReadableStream {
  constructor(source = {}) {
    this._source = source;
    this._queue = [];
    this._reads = [];
    this._closedWaiters = [];
    this._closeRequested = false;
    this._state = 'readable';
    this._error = null;
    this.locked = false;
    const stream = this;
    this._controller = {
      enqueue(value) {
        if (stream._state !== 'readable' || stream._closeRequested) return;
        const pending = stream._reads.shift();
        if (pending) pending.resolve({value, done: false});
        else stream._queue.push(value);
      },
      close() {
        if (stream._state !== 'readable') return;
        stream._closeRequested = true;
        if (stream._queue.length) return;
        stream._state = 'closed';
        while (stream._reads.length) {
          stream._reads.shift().resolve({value: undefined, done: true});
        }
        while (stream._closedWaiters.length) stream._closedWaiters.shift().resolve();
      },
      error(reason) {
        if (stream._state !== 'readable') return;
        stream._state = 'errored';
        stream._error = reason;
        stream._queue.length = 0;
        while (stream._reads.length) stream._reads.shift().reject(reason);
        while (stream._closedWaiters.length) stream._closedWaiters.shift().reject(reason);
      },
      get desiredSize() {
        return Math.max(0, 1 - stream._queue.length);
      },
    };
    try {
      const started = source.start?.(this._controller);
      if (started && typeof started.then === 'function') {
        started.catch(reason => this._controller.error(reason));
      }
    } catch (reason) {
      this._controller.error(reason);
    }
  }
  getReader() {
    if (this.locked) throw new TypeError('ReadableStream is locked');
    this.locked = true;
    const stream = this;
    return {
      read() {
        if (stream._queue.length) {
          const value = stream._queue.shift();
          if (!stream._queue.length && stream._closeRequested) stream._controller.close();
          return Promise.resolve({value, done: false});
        }
        if (stream._state === 'closed') return Promise.resolve({value: undefined, done: true});
        if (stream._state === 'errored') return Promise.reject(stream._error);
        return new Promise((resolve, reject) => stream._reads.push({resolve, reject}));
      },
      cancel(reason) { return stream.cancel(reason); },
      releaseLock() { stream.locked = false; },
    };
  }
  cancel(reason) {
    if (this._state === 'errored') return Promise.reject(this._error);
    this._queue.length = 0;
    if (this._state === 'closed') return Promise.resolve();
    this._controller.close();
    try { return Promise.resolve(this._source.cancel?.(reason)); }
    catch (error) { return Promise.reject(error); }
  }
}

function load(hostname = 'chatgpt.com', protocol = 'https:', Stream = PinnedReadableStream, port = '') {
  const context = {ReadableStream: Stream, location: {hostname, protocol, port}};
  vm.createContext(context);
  const before = context.ReadableStream;
  vm.runInContext(script, context, {filename: 'chatgpt_streams.js'});
  return {context, before};
}
const tick = () => new Promise(resolve => setImmediate(resolve));
async function settled(promise) {
  return Promise.race([
    promise,
    new Promise((_, reject) => setTimeout(() => reject(new Error('stream read stalled')), 500)),
  ]);
}

test('exact HTTPS ChatGPT origin is required, other providers and non-HTTPS untouched', () => {
  for (const [host, protocol] of [
    ['claude.ai', 'https:'],
    ['chatgpt.com.attacker.invalid', 'https:'],
    ['www.chatgpt.com.attacker.invalid', 'https:'],
    ['chatgpt.com', 'http:'],
  ]) {
    const {context, before} = load(host, protocol);
    assert.strictEqual(context.ReadableStream, before);
  }
  for (const host of ['chatgpt.com', 'www.chatgpt.com']) {
    const {context, before} = load(host);
    assert.notStrictEqual(context.ReadableStream, before);
    assert.equal(context.ReadableStream.__chatGatewayPullCompat, true);
    const installed = context.ReadableStream;
    vm.runInContext(script, context);
    assert.strictEqual(context.ReadableStream, installed, 'preloads must be idempotent');
  }
});

test('ChatGPT compatibility never patches non-default HTTPS ports', () => {
  for (const host of ['chatgpt.com', 'www.chatgpt.com']) {
    for (const port of ['444', '8443']) {
      const {context, before} = load(host, 'https:', PinnedReadableStream, port);
      assert.strictEqual(context.ReadableStream, before,
        'a non-default HTTPS port is not the standard ChatGPT origin');
    }
    for (const port of ['', '443']) {
      const {context, before} = load(host, 'https:', PinnedReadableStream, port);
      assert.notStrictEqual(context.ReadableStream, before,
        'the default HTTPS port remains an allowed ChatGPT origin');
    }
  }
});

test('native standard streams and unknown shim shape are never patched', () => {
  class NativeLookingStream {
    constructor() {}
    getReader() { return {read: async () => ({done: true})}; }
  }
  const {context, before} = load('chatgpt.com', 'https:', NativeLookingStream);
  assert.strictEqual(context.ReadableStream, before);
});

test('pinned lazy pull unblocks waiting reader, then reaches terminal done', async () => {
  const {context} = load();
  let calls = 0;
  const stream = new context.ReadableStream({
    pull(controller) {
      calls++;
      controller.enqueue('current');
      controller.close();
    },
  });
  const reader = stream.getReader();
  assert.deepEqual(await settled(reader.read()), {value: 'current', done: false});
  assert.equal((await settled(reader.read())).done, true);
  assert.equal(calls, 1);
});

test('prefill uses highWaterMark and reading drains queue before next pull', async () => {
  const {context} = load();
  let calls = 0;
  const stream = new context.ReadableStream({
    pull(controller) { controller.enqueue(++calls); },
  }, {highWaterMark: 1});
  await tick();
  assert.equal(calls, 1, 'source should prefill one item without an attached reader');
  const reader = stream.getReader();
  assert.equal((await settled(reader.read())).value, 1);
  await tick();
  assert.equal(calls, 2);
  assert.equal((await settled(reader.read())).value, 2);
  await tick();
  assert.equal(calls, 3);
  await reader.cancel('finished');
  await tick();
  assert.equal(calls, 3, 'cancellation must not keep pulling');
});

test('async start blocks pull; rejection propagates to pending reader', async () => {
  const {context} = load();
  let started;
  let calls = 0;
  const stream = new context.ReadableStream({
    start() { return new Promise(resolve => { started = resolve; }); },
    pull(controller) {
      calls++;
      controller.enqueue('after-start');
      controller.close();
    },
  });
  const reader = stream.getReader();
  const pending = reader.read();
  await tick();
  assert.equal(calls, 0);
  started();
  assert.equal((await settled(pending)).value, 'after-start');
  assert.equal(calls, 1);

  const failedStream = new context.ReadableStream({
    pull() { return Promise.reject(new Error('synthetic-pull-error')); },
  });
  const failedReader = failedStream.getReader();
  await assert.rejects(() => settled(failedReader.read()), /synthetic-pull-error/);
});

test('cancellation drains pending read and does not invoke additional pull', async () => {
  const {context} = load();
  let cancels = 0;
  let pulls = 0;
  const stream = new context.ReadableStream({
    pull() { pulls++; },
    cancel(reason) {
      assert.equal(reason, 'manual-cancel');
      cancels++;
    },
  }, {highWaterMark: 0});
  const reader = stream.getReader();
  const pending = reader.read();
  await tick();
  assert.equal(pulls, 1);
  await reader.cancel('manual-cancel');
  assert.equal((await settled(pending)).done, true);
  await tick();
  assert.equal(cancels, 1);
  assert.equal(pulls, 1);
});

test('reader locking, bounded invalid strategy, and default byte-stream read', async () => {
  const {context} = load();
  const locked = new context.ReadableStream({pull(controller) { controller.close(); }});
  const reader = locked.getReader();
  assert.throws(() => locked.getReader(), /locked/);
  reader.releaseLock();

  assert.throws(() => new context.ReadableStream({}, {highWaterMark: -1}), /highWaterMark/);
  assert.throws(() => new context.ReadableStream({}, {highWaterMark: Infinity}), /highWaterMark/);
  assert.throws(() => new context.ReadableStream({}, {size: 'not callable'}), /stream size/);

  const bytes = new context.ReadableStream({
    type: 'bytes',
    pull(controller) {
      controller.enqueue(Uint8Array.from([4, 2]));
      controller.close();
    },
  });
  const result = await settled(bytes.getReader().read());
  assert.deepEqual(Array.from(result.value), [4, 2]);
});

test('wrapped reader preserves byte-view arguments for the delegated read', async () => {
  // The pinned fallback currently does not implement BYOB. This stand-in
  // verifies the compat wrapper does not destroy a future reader's contract;
  // it is NOT proof of full byte-stream/BYOB conformance.
  class ByteViewStream extends PinnedReadableStream {
    getReader(options) {
      const reader = super.getReader();
      if (options?.mode === 'byob') {
        const originalRead = reader.read.bind(reader);
        reader.read = async view => {
          if (!(view instanceof Uint8Array)) throw new TypeError('BYOB view missing');
          const chunk = await originalRead();
          if (chunk.done) return chunk;
          view.set(chunk.value);
          return {value: view.subarray(0, chunk.value.length), done: false};
        };
      }
      return reader;
    }
  }
  const {context} = load('chatgpt.com', 'https:', ByteViewStream);
  const stream = new context.ReadableStream({
    type: 'bytes',
    pull(controller) {
      controller.enqueue(Uint8Array.from([8, 5]));
      controller.close();
    },
  });
  const reader = stream.getReader({mode: 'byob'});
  const buffer = new Uint8Array(4);
  const first = await settled(reader.read(buffer));
  assert.deepEqual(Array.from(first.value), [8, 5]);
  assert.strictEqual(first.value.buffer, buffer.buffer,
    'the same caller-provided byte-view backing buffer must be delegated');
  assert.equal((await settled(reader.read(new Uint8Array(4)))).done, true);
});

test('rejected queued read retains backpressure until a chunk is consumed', async () => {
  // A specialized reader can reject its view before touching the queue.
  // The compatibility shim must not treat that as a successful dequeue.
  class RejectingReadStream extends PinnedReadableStream {
    getReader(...args) {
      const reader = super.getReader(...args);
      const originalRead = reader.read.bind(reader);
      reader.read = view => view === null
        ? Promise.reject(new TypeError('invalid read view'))
        : originalRead();
      return reader;
    }
  }
  const {context} = load('chatgpt.com', 'https:', RejectingReadStream);
  let controller;
  let pulls = 0;
  const stream = new context.ReadableStream({
    start(value) { controller = value; },
    pull(value) { value.enqueue(++pulls); },
  }, {highWaterMark: 1});
  await tick();
  assert.equal(pulls, 1, 'the first chunk prefills the single-slot queue');
  assert.equal(controller.desiredSize, 0);

  const reader = stream.getReader();
  await assert.rejects(reader.read(null), /invalid read view/);
  await tick();
  assert.equal(pulls, 1, 'rejected read must not trigger a second pull');
  assert.equal(controller.desiredSize, 0, 'rejected read must retain queued credit');
  assert.equal((await settled(reader.read('valid'))).value, 1,
    'valid read still receives the original queued chunk');
  await tick();
  assert.equal(pulls, 2, 'successful dequeue restores one credit');
  assert.equal(controller.desiredSize, 0);
  await reader.cancel('finished');
});

test('50 sequential streams terminate without a retained global registry', async () => {
  const {context} = load();
  for (let i = 0; i < 50; i++) {
    const stream = new context.ReadableStream({
      pull(controller) {
        controller.enqueue(i);
        controller.close();
      },
    });
    assert.equal((await settled(stream.getReader().read())).value, i);
  }
  assert.equal(context.__chatGatewayStreamRegistry, undefined);
});


test('late rejected pull cannot erase final enqueued chunk after close', async () => {
  const {context} = load();
  let calls = 0;
  const stream = new context.ReadableStream({
    pull(controller) {
      calls++;
      controller.enqueue('final');
      controller.close();
      return Promise.reject(new Error('late-rejection-after-close'));
    },
  });
  // Let pull and its rejection settle before attaching a reader. The pinned
  // Obscura fallback remains _closeRequested while one queued chunk exists.
  await tick();
  const reader = stream.getReader();
  assert.deepEqual(await settled(reader.read()), {value:'final',done:false});
  assert.deepEqual(await settled(reader.read()), {value:undefined,done:true});
  assert.equal(calls,1);
});

test('late rejected pull after cancellation cannot change closed state', async () => {
  const {context} = load();
  let rejectPull;
  let sourceController;
  let calls = 0;
  const stream = new context.ReadableStream({
    pull(controller) {
      calls++;
      sourceController = controller;
      return new Promise((_, reject) => {rejectPull = reject;});
    },
  }, {highWaterMark:0});
  const reader = stream.getReader();
  const pendingRead = reader.read();
  await tick();
  assert.equal(calls,1);
  await reader.cancel('owner-cancel');
  assert.equal((await settled(pendingRead)).done,true);
  rejectPull(new Error('late-rejection-after-cancel'));
  await tick();
  assert.equal(sourceController.desiredSize,0,
    'late error must not mark an already-cancelled stream errored');
  assert.equal((await settled(reader.read())).done,true);
  assert.equal(calls,1,'cancelled stream never restarts pulling');
});


test('late async start rejection after close preserves queued terminal response', async () => {
  const {context} = load();
  const stream = new context.ReadableStream({
    start(controller) {
      controller.enqueue('final-async');
      controller.close();
      return Promise.reject(new Error('late-start-async-rejection'));
    },
  });
  await tick();
  const reader = stream.getReader();
  assert.deepEqual(await settled(reader.read()), {value:'final-async',done:false});
  assert.deepEqual(await settled(reader.read()), {value:undefined,done:true});
});

test('synchronous start throw after close cannot erase final queued chunk', async () => {
  const {context} = load();
  const stream = new context.ReadableStream({
    start(controller) {
      controller.enqueue('final-sync');
      controller.close();
      throw new Error('late-start-sync-throw');
    },
  });
  await tick();
  const reader = stream.getReader();
  assert.deepEqual(await settled(reader.read()), {value:'final-sync',done:false});
  assert.deepEqual(await settled(reader.read()), {value:undefined,done:true});
});

test('start rejection before close still errors reader, cancellation blocks late rejection', async () => {
  const {context} = load();
  const failed = new context.ReadableStream({
    start() { return Promise.reject(new Error('real-start-failure')); },
  });
  await assert.rejects(() => settled(failed.getReader().read()), /real-start-failure/);

  let rejectStart;
  let cancellations = 0;
  const cancelled = new context.ReadableStream({
    start() {
      return new Promise((_, reject) => { rejectStart = reject; });
    },
    cancel(reason) {
      assert.equal(reason, 'owner-cancel');
      cancellations++;
    },
  }, {highWaterMark:0});
  const reader = cancelled.getReader();
  const pending = reader.read();
  await reader.cancel('owner-cancel');
  assert.deepEqual(await settled(pending), {value:undefined,done:true});
  rejectStart(new Error('late-after-owner-cancel'));
  await tick();
  assert.deepEqual(await settled(reader.read()), {value:undefined,done:true});
  assert.equal(cancellations, 1);
});
