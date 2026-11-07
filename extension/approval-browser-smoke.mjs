// Real Chromium extension + real gateway approval. Fresh profile, no provider credentials.
import {chromium} from 'playwright';
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {mkdtemp,cp,readFile,writeFile,rm} from 'node:fs/promises';
import {resolve,dirname,join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {randomBytes} from 'node:crypto';
import net from 'node:net';
const root=dirname(fileURLToPath(import.meta.url));
const temp=await mkdtemp('/tmp/cg-extension-approval-');
const binary=resolve(process.argv[2]);
const server=net.createServer();await new Promise(r=>server.listen(0,'127.0.0.1',r));
const port=server.address().port;await new Promise(r=>server.close(r));
const base='http://127.0.0.1:'+port;
let context,gateway;
try {
 await writeFile(join(temp,'config.json'),JSON.stringify({providers:[]}));
 await cp(root,join(temp,'extension'),{recursive:true});
 const manifest=JSON.parse(await readFile(join(temp,'extension/manifest.json'),'utf8'));
 // Host permission consent is pregranted for this isolated fixture. Production JS is unchanged.
 manifest.host_permissions=['http://127.0.0.1/*'];
 await writeFile(join(temp,'extension/manifest.json'),JSON.stringify(manifest));
 gateway=spawn(binary,['--config',join(temp,'config.json')],{cwd:temp,stdio:'ignore',env:{
  PATH:process.env.PATH,HOME:temp,PORT:String(port),TOKIO_WORKER_THREADS:'2',
  CHAT_GATEWAY_SESSION_STORE:'vault',CHAT_GATEWAY_VAULT_KEY:randomBytes(32).toString('hex'),
  CHAT_GATEWAY_MEMORY_PATH:join(temp,'memory.sqlite3'),CHAT_GATEWAY_EMBEDDED_BROWSER:'disabled',
  ...(process.env.LD_LIBRARY_PATH?{LD_LIBRARY_PATH:process.env.LD_LIBRARY_PATH}:{}),
 }});
 let ready=false;
 for(let i=0;i<200;i++){
  if(gateway.exitCode!==null) throw new Error('Gateway exited during extension acceptance');
  try {ready=(await fetch(base+'/health')).ok;}catch{}
  if(ready)break;await new Promise(r=>setTimeout(r,100));
 }
 assert.ok(ready,'gateway health');
 context=await chromium.launchPersistentContext(join(temp,'profile'),{
  headless:true,chromiumSandbox:true,
  ...(process.env.CHAT_GATEWAY_CHROMIUM_PATH?{executablePath:process.env.CHAT_GATEWAY_CHROMIUM_PATH}:{}),
  ignoreDefaultArgs:['--disable-extensions'],args:['--disable-extensions-except='+join(temp,'extension'),'--load-extension='+join(temp,'extension')],
 });
 const worker=context.serviceWorkers()[0]||await context.waitForEvent('serviceworker');
 const id=new URL(worker.url()).host;
 const popup=await context.newPage();await popup.goto('chrome-extension://'+id+'/popup.html');
 assert.equal(await popup.locator('#code').count(),0);
 await popup.locator('#gateway').fill(base);
 const approvalTab=context.waitForEvent('page');
 await popup.locator('#pair').click();
 const approval=await approvalTab;await approval.waitForLoadState('domcontentloaded');
 await popup.close();
 const [response]=await Promise.all([approval.waitForNavigation(),approval.getByRole('button',{name:'Approve connection'}).click()]);
 assert.equal(response.status(),200,'native Chromium form submission must retain a valid Origin');
 await approval.getByText('Connection approved.',{exact:false}).waitFor();
 const reopened=await context.newPage();await reopened.goto('chrome-extension://'+id+'/popup.html');
 await reopened.locator('#linked:not([hidden])').waitFor({timeout:20000});
 await reopened.locator('#status li').first().waitFor();
 assert.equal(await reopened.locator('#status li').count(),8);
 console.log('PASS real Chromium extension URL-only approval, popup closure and provider dashboard');
} finally {
 await context?.close();
 if(gateway&&gateway.exitCode===null){
  const stopped=new Promise(r=>gateway.once('exit',r));gateway.kill('SIGTERM');
  const timer=setTimeout(()=>gateway.kill('SIGKILL'),30000);await stopped;clearTimeout(timer);
 }
 await rm(temp,{recursive:true,force:true});
}
