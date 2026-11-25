// OpenCode 2.4 promise-plugin API. No content or credentials are recorded.
export default {
  id: 'local.metaai-request-kind',
  async setup(ctx) {
    const registration = await ctx.session.hook('http.request', event => {
      if (event.model.id !== 'metaai') return;
      const kind = ['primary', 'title', 'compaction', 'generate'].includes(event.kind)
        ? event.kind : 'unspecified';
      event.request.headers.set('X-Chat-Gateway-Request-Kind', kind);
    }, {providerID: 'chat-gateway'});
    return () => registration.dispose();
  }
};
