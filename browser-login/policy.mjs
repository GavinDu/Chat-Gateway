// Reviewed navigation domains. No arbitrary hosts, ports, IPs, or URL credentials.
export const providerDomains = Object.freeze({
  chatgpt: ['chatgpt.com', 'openai.com', 'auth0.com'],
  claude: ['claude.ai', 'anthropic.com'],
  gemini: ['gemini.google.com', 'google.com'],
  grok: ['grok.com', 'x.com', 'twitter.com', 'x.ai'],
  qwen: ['qwen.ai', 'aliyun.com', 'alibabacloud.com'],
  metaai: ['meta.ai', 'meta.com', 'facebook.com'],
  mistral: ['mistral.ai'],
  perplexity: ['perplexity.ai']
});
const idps = ['accounts.google.com', 'appleid.apple.com', 'idmsa.apple.com',
  'login.microsoftonline.com', 'login.live.com', 'github.com'];
// Match session.rs's reviewed cookie import contract, not the broader SSO
// navigation policy. Identity-provider cookies must not enter another vault.
export const vaultDomains = Object.freeze({
  ...providerDomains,
  chatgpt: ['chatgpt.com', 'openai.com'],
  qwen: ['qwen.ai', 'aliyun.com'],
  metaai: ['meta.ai', 'facebook.com', 'fbcdn.net'],
  gemini: ['gemini.google.com', 'google.com', 'googleusercontent.com']
});
export function domainMatches(host, domains) {
  return domains.some(base => host === base || host.endsWith('.' + base));
}
export function allowed(provider, target, extra = []) {
  try {
    const url = new URL(target);
    return Boolean(providerDomains[provider]) && url.protocol === 'https:' &&
      !url.username && !url.password && (!url.port || url.port === '443') &&
      !url.hostname.endsWith('.') &&
      domainMatches(url.hostname, [...providerDomains[provider], ...idps, ...extra]);
  } catch { return false; }
}
// Challenge services are iframe-only. They are never allowed as address-bar
// destinations, and the user must solve any challenge manually.
export function allowedDocument(provider, target, isMainFrame) {
  if (!providerDomains[provider]) return false;
  if (allowed(provider, target)) return true;
  if (isMainFrame) return false;
  try {
    const u = new URL(target);
    if (u.protocol !== 'https:' || u.username || u.password || u.port) return false;
    if (u.hostname === 'challenges.cloudflare.com') return true;
    if (domainMatches(u.hostname, ['hcaptcha.com'])) return true;
    return ['www.google.com', 'www.recaptcha.net'].includes(u.hostname) &&
      u.pathname.startsWith('/recaptcha/');
  } catch { return false; }
}
export function safeAddress(target) {
  try {
    const u = new URL(target);
    return u.protocol === 'https:' ? u.origin + u.pathname : 'about:blank';
  } catch { return 'about:blank'; }
}
export function dimensions(width, height) {
  if (!Number.isInteger(width) || !Number.isInteger(height) ||
      width < 640 || width > 1920 || height < 480 || height > 1080) {
    throw new Error('invalid_resize');
  }
  return { width, height };
}
