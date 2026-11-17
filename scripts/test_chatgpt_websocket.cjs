const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
function fixture(hostname = 'chatgpt.com', options = {}, provider = 'chatgpt') {
  const commands = [];
  const packets = [];
  let reader;
  function push(packet) {
    if (reader) { const take = reader; reader = null; take(packet); }
    else packets.push(packet);
  }
  const original = function OriginalSocket() {};
  const context = {
    location:{hostname,protocol:'https:',port:'',origin:'https://' + hostname,href:'https://' + hostname + '/'},
    navigator:{userAgent:'fixture'}, WebSocket:original,
    EventTarget, Event, MessageEvent, DOMException, URL, TextEncoder,
    Uint8Array, ArrayBuffer, Blob, btoa, atob, setTimeout, clearTimeout,
    fetch:async (url, requestOptions) => {
      assert.equal(url, 'https://' + hostname + '/.well-known/chat-gateway-websocket');
      assert.equal(requestOptions.headers['X-Chat-Gateway-WS-Token'], 'fixture-token');
      const command = JSON.parse(Buffer.from(requestOptions.headers['X-Chat-Gateway-WS-Command'],'base64').toString());
      commands.push(command);
      let result = {ok:true};
      if (command.op === 'open') {
        if (options.delayOpenMs) await new Promise(resolve => setTimeout(resolve, options.delayOpenMs));
        result = {ok:true,id:1,protocol:command.protocols[0] || ''};
      }
      if (command.op === 'send') push({type:command.binary?'binary':'text',data:command.data});
      if (command.op === 'close' && !options.ignoreClose) push({type:'close',code:command.code,reason:command.reason,clean:true});
      if (command.op === 'read') result = {ok:true,packet:packets.length ? packets.shift() : await new Promise(resolve => {reader=resolve;})};
      return {ok:true,json:async()=>result};
    }
  };
  vm.createContext(context);
  let script = fs.readFileSync('src/browser/chatgpt_websocket.js','utf8')
    .replace('__BRIDGE_CONFIG__', JSON.stringify({token:'fixture-token',path:'/.well-known/chat-gateway-websocket'}));
  if (options.closeTimeoutMs) {
    script = script.replace('const CLOSE_TIMEOUT_MS = 10000;',
      `const CLOSE_TIMEOUT_MS = ${options.closeTimeoutMs};`);
  }
  script = script.replace("['chatgpt.com', 'www.chatgpt.com']", provider === 'metaai' ? "['meta.ai', 'www.meta.ai']" : "['chatgpt.com', 'www.chatgpt.com']");
  vm.runInContext(script, context);
  return {context,commands,original,push};
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
  const source = require('node:fs').readFileSync(require('node:path').join(__dirname, '../src/browser/pool/chatgpt_streams.js'), 'utf8');
  const context = {location:{hostname:'chatgpt.com'}, ReadableStream:NativeStream};
  require('node:vm').runInNewContext(source, context);
  assert.equal(context.ReadableStream, NativeStream);
});

test('ChatGPT stream compatibility does not inspect other providers', () => {
  const source = require('node:fs').readFileSync(require('node:path').join(__dirname, '../src/browser/pool/chatgpt_streams.js'), 'utf8');
  const context = {location:{hostname:'example.test'}};
  Object.defineProperty(context, 'ReadableStream', {get(){throw new Error('foreign provider API inspected');}});
  assert.doesNotThrow(() => require('node:vm').runInNewContext(source, context));
});

test('non-HTTPS and alternate port never install the ChatGPT socket override', () => {
  for (const [protocol, port] of [['http:', ''], ['https:', '8443']]) {
    const f = fixture('chatgpt.com');
    assert.notEqual(f.context.WebSocket, f.original);
    // Re-evaluate with a fresh realm because the constructor is installed only once.
    const vm = require('node:vm');
    const code = fs.readFileSync('src/browser/chatgpt_websocket.js','utf8')
      .replace('__BRIDGE_CONFIG__', JSON.stringify({token:'fixture', path:'/.well-known/chat-gateway-websocket'}));
    const context = {
      location: {hostname:'chatgpt.com', protocol, port, origin:protocol + '//chatgpt.com'},
      WebSocket:f.original,
    };
    vm.createContext(context);
    vm.runInContext(code, context);
    assert.equal(context.WebSocket, f.original);
  }
});

test('noncooperating close deadline releases socket and ignores a late message', async () => {
  const f = fixture('chatgpt.com', {ignoreClose:true,closeTimeoutMs:5});
  const socket = new f.context.WebSocket('wss://ws.chatgpt.com/socket');
  let messages = 0, errors = 0, closes = 0;
  const finished = new Promise((resolve,reject) => {
    socket.onerror = () => { errors++; };
    socket.onmessage = () => { messages++; };
    socket.onopen = () => socket.close();
    socket.onclose = event => {
      closes++;
      try { assert.equal(event.wasClean,false); resolve(); } catch (e) { reject(e); }
    };
  });
  await finished;
  assert.equal(socket.readyState, 3);
  f.push({type:'text',data:'late private frame'});
  await new Promise(resolve => setTimeout(resolve,15));
  assert.equal(messages,0);
  assert.equal(errors,1);
  assert.equal(closes,1);
  assert.equal(f.commands.filter(c=>c.op==='release').length,1);
});

test('close during delayed connect cannot resurrect a socket after deadline', async () => {
  const f = fixture('chatgpt.com', {delayOpenMs:40,closeTimeoutMs:5});
  const socket = new f.context.WebSocket('wss://ws.chatgpt.com/socket');
  let opens=0, closes=0;
  socket.onopen = () => { opens++; };
  const closed = new Promise(resolve => { socket.onclose=()=>{closes++;resolve();}; });
  socket.close();
  await closed;
  await new Promise(resolve=>setTimeout(resolve,55));
  assert.equal(socket.readyState,3);
  assert.equal(opens,0);
  assert.equal(closes,1);
  assert.equal(f.commands.filter(c=>c.op==='release').length,1);
});

test('Meta AI page bridge forwards a real binary conversation frame and preserves origin isolation', async () => {
  const f = fixture('www.meta.ai', {}, 'metaai');
  const socket = new f.context.WebSocket('wss://gateway.meta.ai/ws/clippy');
  socket.binaryType = 'arraybuffer';
  await new Promise((resolve, reject) => {
    socket.onerror = () => reject(new Error('unexpected Meta AI transport failure'));
    socket.onopen = () => socket.send(new Uint8Array([8, 1, 255]));
    socket.onmessage = event => {
      assert.deepEqual([...new Uint8Array(event.data)], [8, 1, 255]);
      socket.close(1000, 'fixture');
    };
    socket.onclose = resolve;
  });
  assert.equal(f.commands.filter(command => command.op === 'send').length, 1);
  assert.equal(f.commands.find(command => command.op === 'open').url, 'wss://gateway.meta.ai/ws/clippy');
  const foreign = fixture('chatgpt.com', {}, 'metaai');
  assert.equal(foreign.context.WebSocket, foreign.original);
  assert.equal(foreign.commands.length, 0);
});

test('Meta AI close while connect is pending releases exactly once before the deadline', async () => {
  const f = fixture('www.meta.ai', {delayOpenMs:5,closeTimeoutMs:1000}, 'metaai');
  const socket = new f.context.WebSocket('wss://gateway.meta.ai/ws/clippy');
  let opens = 0;
  socket.onopen = () => {opens++;};
  const closed = new Promise(resolve => {socket.onclose = resolve;});
  socket.close();
  await closed;
  assert.equal(opens, 0);
  assert.equal(socket.readyState, 3);
  assert.equal(f.commands.filter(c => c.op === 'release').length, 1);
});
