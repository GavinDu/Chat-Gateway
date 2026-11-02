// Public platform settings only. Never inherit gateway Secrets into fixtures.
import { existsSync } from 'node:fs';
const nixLibrary = '/nix/store/2ga5nd1m56n5cx2wh8vbf6nrdhqk2f0q-gcc-15.3.0-lib/lib';
export function rustRuntimeEnv() {
  return existsSync(nixLibrary) ? {
    GLIBC_TUNABLES: 'glibc.rtld.optional_static_tls=1048576',
    LD_LIBRARY_PATH: nixLibrary,
  } : {};
}
export function chromiumExecutable() {
  return process.env.CHAT_GATEWAY_CHROMIUM_PATH ||
    (existsSync('/repl/tools/bin/chromium') ? '/repl/tools/bin/chromium' : undefined);
}
