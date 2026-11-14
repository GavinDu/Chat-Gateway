const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
function fixture(hostname = 'chatgpt.com') {
  const commands = [];
  const packets = [];
  let reader;
  function push(packet) {
    if (reader) { const take = reader; reader = null; take(packet); }
    else packets.push(packet);
  }
  const original = function OriginalSocket() {};
  const context = {
    location:{hostname,origin:'https://' + hostname,href:'https://' + hostname + '/'},
    navigator:{userAgent:'fixture'}, WebSocket:original,
    EventTarget, Event, MessageEvent, DOMException, URL, TextEncoder,
    Uint8Array, ArrayBuffer, Blob, btoa, atob, setTimeout, clearTimeout,
    fetch:async (url, options) => {
      assert.equal(url, 'https://' + hostname + '/.well-known/chat-gateway-websocket');
      assert.equal(options.headers['X-Chat-Gateway-WS-Token'], 'fixture-token');
      const command = JSON.parse(Buffer.from(options.headers['X-Chat-Gateway-WS-Command'],'base64').toString());
      commands.push(command);
      let result = {ok:true};
      if (command.op === 'open') result = {ok:true,id:1,protocol:command.protocols[0] || ''};
      if (command.op === 'send') push({type:command.binary?'binary':'text',data:command.data});
      if (command.op === 'close') push({type:'close',code:command.code,reason:command.reason,clean:true});
      if (command.op === 'read') result = {ok:true,packet:packets.length ? packets.shift() : await new Promise(resolve => {reader=resolve;})};
      return {ok:true,json:async()=>result};
    }
  };
  vm.createContext(context);
  vm.runInContext(fs.readFileSync('src/browser/chatgpt_websocket.js','utf8')
    .replace('__BRIDGE_CONFIG__', JSON.stringify({token:'fixture-token',path:'/.well-known/chat-gateway-websocket'})),context);
  return {context,commands,original};
}
test('ChatGPT socket forwards text once, preserves protocol, and closes cleanly', async () => {
  const f=fixture();
  const socket=new f.context.WebSocket('wss://ws.chatgpt.com/socket',['fixture']);
  let messages=0;
  const closed=new Promise((resolve,reject)=>{
    socket.onerror=()=>reject(new Error('unexpected transport error'));
    socket.onopen=()=>{assert.equal(socket.readyState,1);assert.equal(socket.protocol,'fixture');socket.send('fixture text');};
    socket.onmessage=event=>{messages++;assert.equal(event.data,'fixture text');socket.close(1000,'done');};
    socket.onclose=resolve;
  });
  const event=await closed;
  assert.equal(event.code,1000);
  assert.equal(event.wasClean,true);
  assert.equal(messages,1);
  assert.equal(socket.readyState,3);
  assert.equal(f.commands.filter(c=>c.op==='send').length,1);
  assert.equal(Object.hasOwn(f.commands[0],'url'),false);
});
test('binary frames preserve exact bytes', async () => {
  const f=fixture();
  const socket=new f.context.WebSocket('wss://ws.chatgpt.com/socket');
  socket.binaryType='arraybuffer';
  await new Promise((resolve,reject)=>{
    socket.onerror=()=>reject(new Error('transport failed'));
    socket.onopen=()=>socket.send(new Uint8Array([0,1,255]));
    socket.onmessage=event=>{assert.deepEqual([...new Uint8Array(event.data)],[0,1,255]);socket.close();};
    socket.onclose=resolve;
  });
});
test('other provider pages retain their existing transport and make no bridge calls', () => {
  const f=fixture('claude.ai');
  assert.equal(f.context.WebSocket,f.original);
  assert.equal(f.commands.length,0);
});
test('invalid URLs and duplicate protocols fail without opening a socket', () => {
  const f=fixture();
  assert.throws(()=>new f.context.WebSocket('https://ws.chatgpt.com/socket'),{name:'SyntaxError'});
  assert.throws(()=>new f.context.WebSocket('wss://ws.chatgpt.com/socket',['duplicate','duplicate']),{name:'SyntaxError'});
  assert.equal(f.commands.filter(c=>c.op==='open').length,0);
});


test('ChatGPT stream compatibility leaves native Web Streams unchanged', () => {
  const NativeStream = require('node:stream/web').ReadableStream;
  const source = require('node:fs').readFileSync(require('node:path').join(__dirname, '../src/browser/chatgpt_streams.js'), 'utf8');
  const context = {location:{hostname:'chatgpt.com'}, ReadableStream:NativeStream};
  require('node:vm').runInNewContext(source, context);
  assert.equal(context.ReadableStream, NativeStream);
});

test('ChatGPT stream compatibility does not inspect other providers', () => {
  const source = require('node:fs').readFileSync(require('node:path').join(__dirname, '../src/browser/chatgpt_streams.js'), 'utf8');
  const context = {location:{hostname:'example.test'}};
  Object.defineProperty(context, 'ReadableStream', {get(){throw new Error('foreign provider API inspected');}});
  assert.doesNotThrow(() => require('node:vm').runInNewContext(source, context));
});
