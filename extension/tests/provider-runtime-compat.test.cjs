const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const source = fs.readFileSync('src/browser/pool/metaai_response.js', 'utf8');
function fixture(location = {protocol:'https:', hostname:'www.meta.ai', port:''}) {
  const context = vm.createContext({location, ReadableStream, TextEncoder});
  // Reproduce the pinned Obscura getter's observable contract and failure.
  vm.runInContext(`globalThis.Response = class Response {
    constructor(body) {
      this._bodyBytes = new TextEncoder().encode(body ?? '');
      this._bodyNull = body == null; this._bodyUsed = false; this._bodyStream = null;
    }
    get body() {
      if (this._bodyNull) return null;
      if (this._bodyUsed) throw new TypeError('Body is already consumed');
      if (!this._bodyStream) {
        const bytes = this._bodyBytes;
        const stream = this._bodyStream = new ReadableStream({start(c) {c.enqueue(bytes);c.close();}});
        const getReader = stream.getReader.bind(stream);
        stream.getReader = (...args) => {this._bodyUsed = true; return getReader(...args);};
      }
      return this._bodyStream;
    }
    async text() {
      if (this._bodyUsed) throw new TypeError('Body is already consumed');
      this._bodyUsed = true; return 'fixture';
    }
  };`, context);
  return context;
}
test('Meta AI repeated Response.body returns the same stream without replaying consumed bytes', async () => {
  const context = fixture();
  const broken = new context.Response('fixture');
  broken.body.getReader();
  assert.throws(() => broken.body, /already consumed/);
  vm.runInContext(source, context);
  const response = new context.Response('fixture');
  const stream = response.body;
  const reader = stream.getReader();
  assert.equal(response.body, stream);
  assert.equal(response.body, stream);
  assert.throws(() => response.body.getReader(), /locked/);
  assert.equal(Buffer.from((await reader.read()).value).toString(), 'fixture');
  assert.equal((await reader.read()).done, true);
  assert.equal(response.body, stream);
  await assert.rejects(response.text(), /already consumed/);
  const accessor = Object.getOwnPropertyDescriptor(context.Response.prototype, 'body').get;
  vm.runInContext(source, context);
  assert.equal(Object.getOwnPropertyDescriptor(context.Response.prototype, 'body').get, accessor);
  assert.equal(new context.Response(null).body, null);
});
test('Meta AI shim leaves native Response and unrelated or insecure origins untouched', () => {
  const original = Object.getOwnPropertyDescriptor(Response.prototype, 'body').get;
  vm.runInNewContext(source, {location:{protocol:'https:',hostname:'www.meta.ai',port:''},Response});
  assert.equal(Object.getOwnPropertyDescriptor(Response.prototype, 'body').get, original);
  for (const location of [
    {protocol:'https:',hostname:'chatgpt.com',port:''},
    {protocol:'https:',hostname:'meta.ai.attacker.test',port:''},
    {protocol:'http:',hostname:'www.meta.ai',port:''},
    {protocol:'https:',hostname:'www.meta.ai',port:'8443'}
  ]) {
    const context = fixture(location);
    const before = Object.getOwnPropertyDescriptor(context.Response.prototype, 'body').get;
    vm.runInContext(source, context);
    assert.equal(Object.getOwnPropertyDescriptor(context.Response.prototype, 'body').get, before);
  }
});
