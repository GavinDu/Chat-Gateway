// Test-only SSH server. Actual OpenSSH client, signed ephemeral public-key
// authentication, pinned host key, production helper CLI and remote stdio code.
// Never reads existing SSH config, credentials, provider state or host keys.
import ssh2 from 'ssh2';
import { spawn } from 'node:child_process';
import { mkdtemp, writeFile, readFile, rm } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
const { Server, utils } = ssh2;
const root = dirname(fileURLToPath(import.meta.url));

const STARTUP_CATEGORIES = new Set([
  'server_bind_failed',
  'helper_spawn_failed',
  'helper_exited_before_ready',
  'helper_ready_timeout',
  'cleanup_failed',
  'startup_unclassified'
]);
function startupError(category) {
  const error = new Error('Isolated SSH fixture startup unavailable.');
  error.fixtureCategory = STARTUP_CATEGORIES.has(category) ? category : 'startup_unclassified';
  return error;
}

// The optional hooks inject only fixture process/socket failures in offline
// tests. The real browser/SSH acceptance always calls this with no hooks.
export async function startOwnerTunnel(gatewayPort, testHooks = {}) {
  const temp = await mkdtemp('/dev/shm/cg-ssh-fixture-');
  testHooks.onTemporaryDirectory?.(temp);
  const host = utils.generateKeyPairSync('ed25519');
  const owner = utils.generateKeyPairSync('ed25519');
  const allowed = utils.parseKey(owner.public);
  const clients = new Set(), children = new Set();
  let authenticated = 0, rejected = 0, helper;
  const source = await readFile(join(root, 'owner_tunnel.py'), 'utf8');
  const remote = source.match(/REMOTE = """([\s\S]+?)"""/)[1].replaceAll('8099', String(gatewayPort));
  const expected = "python3 -c '" + remote.replaceAll("'", "'\"'\"'") + "'";
  const server = new Server({hostKeys:[host.private]}, client => {
    clients.add(client);
    client.on('error', () => {});
    client.on('close', () => clients.delete(client));
    client.on('authentication', ctx => {
      if (ctx.method === 'none') { ctx.reject(['publickey']); return; }
      if (ctx.username !== 'fixture' || ctx.method !== 'publickey' ||
          ctx.key.algo !== allowed.type || !ctx.key.data.equals(allowed.getPublicSSH()) ||
          (ctx.signature && allowed.verify(ctx.blob, ctx.signature, ctx.hashAlgo) !== true)) {
        rejected++; ctx.reject(); return;
      }
      if (ctx.signature) authenticated++;
      ctx.accept();
    });
    client.on('ready', () => client.on('session', accept => {
      const session = accept();
      session.on('exec', (acceptExec, rejectExec, info) => {
        if (info.command !== expected) { rejectExec(); return; }
        const channel = acceptExec();
        const child = spawn('python3', ['-u', '-c', remote], {
          env:{PATH:process.env.PATH,HOME:temp}, stdio:['pipe','pipe','ignore']
        });
        children.add(child);
        channel.on('error', () => {});
        child.stdin.on('error', () => {});
        channel.pipe(child.stdin); child.stdout.pipe(channel, {end:false});
        channel.on('close', () => { if(child.exitCode === null) child.kill('SIGTERM'); });
        child.on('exit', code => {
          children.delete(child);
          if (!channel.destroyed) { channel.exit(code ?? 1); channel.end(); }
        });
      });
    }));
  });
  const stop = async () => {
    if (helper && helper.exitCode === null) {
      // A failed spawn might emit 'error' without 'exit'. Never hang cleanup.
      await new Promise(resolve => {
        const timer = setTimeout(resolve, 1_000);
        const done = () => { clearTimeout(timer); resolve(); };
        helper.once('exit', done);
        helper.once('error', done);
        try { helper.kill('SIGTERM'); } catch { done(); }
      });
    }
    for (const client of clients) client.destroy();
    for (const child of children) child.kill('SIGTERM');
    await new Promise(resolve => server.close(resolve));
    await rm(temp,{recursive:true,force:true});
  };
  try {
    await new Promise((resolve,reject) => {
      const bindFailed = () => reject(startupError('server_bind_failed'));
      server.once('error', bindFailed);
      try {
        if (testHooks.listenServer) testHooks.listenServer(server, resolve, bindFailed);
        else server.listen(0,'127.0.0.1',resolve);
      } catch { bindFailed(); }
    });
    await writeFile(join(temp,'identity'),owner.private,{mode:0o600});
    await writeFile(join(temp,'known_hosts'),'cg-fixture '+host.public.trim()+'\n',{mode:0o600});
    await writeFile(join(temp,'config'),[
      'Host fixture', '  HostName 127.0.0.1', '  User fixture',
      '  IdentityFile '+join(temp,'identity'), '  IdentitiesOnly yes',
      '  UserKnownHostsFile '+join(temp,'known_hosts'),
      '  GlobalKnownHostsFile /dev/null', '  HostKeyAlias cg-fixture',
      '  PasswordAuthentication no', '  KbdInteractiveAuthentication no',
      '  IdentityAgent none', ''
    ].join('\n'),{mode:0o600});
    try {
      helper = (testHooks.spawnHelper || spawn)('python3',['-B',join(root,'owner_tunnel.py'),
        '--ssh-target','fixture','--ssh-port',String(server.address().port),
        '--ssh-config',join(temp,'config'),'--gateway-port',String(gatewayPort),
        '--local-port','0'], {env:{PATH:process.env.PATH,HOME:temp},
        stdio:['ignore','pipe','ignore']});
    } catch { throw startupError('helper_spawn_failed'); }
    const port = await new Promise((resolve,reject) => {
      let settled = false;
      const finish = (value, category) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        if (category) reject(startupError(category));
        else resolve(value);
      };
      const timer = setTimeout(() => finish(null, 'helper_ready_timeout'),
        testHooks.startupTimeoutMs ?? 15_000);
      let text='';
      helper.stdout.on('data',data=>{
        text += data.toString();
        const match=text.match(/Owner-only local UI: http:\/\/127\.0\.0\.1:(\d+)\/login/);
        if(match) finish(Number(match[1]), null);
      });
      helper.once('error',()=>finish(null, 'helper_spawn_failed'));
      helper.once('exit',()=>finish(null, 'helper_exited_before_ready'));
    });
    return {port,stop,get authenticated(){return authenticated;},get rejected(){return rejected;}};
  } catch(error) {
    // No path, remote command, key material or raw child stderr can escape
    // through this fixture's diagnostic surface.
    const category = error?.fixtureCategory || 'startup_unclassified';
    try { await stop(); }
    catch { throw startupError('cleanup_failed'); }
    throw startupError(category);
  }
}
