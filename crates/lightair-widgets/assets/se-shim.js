// StreamElements compatibility: a StreamElements custom widget (its HTML, CSS, JS and fields)
// runs unchanged. Lightair's events become StreamElements' onWidgetLoad, onEventReceived (with
// its listener names and payloads) and onSessionUpdate; SE_API is backed by Lightair's storage.
(function () {
  var L = window.lightair;
  function fire(name, detail) { window.dispatchEvent(new CustomEvent(name, { detail: detail })); }
  function name(e) { return e.user ? e.user.display_name : ''; }
  function login(e) { return e.user ? (e.user.login || e.user.display_name) : ''; }
  function amountOf(e) {
    if (e.kind && e.kind.viewers != null) return e.kind.viewers;
    if (e.paid) return e.paid.amount;
    var m = /(\d+)\s+months?/.exec(e.headline || '');
    return m ? +m[1] : 1;
  }
  function emotes(e) {
    var at = 0, out = [];
    (e.fragments || []).forEach(function (f) {
      var t = f.type === 'emote' ? f.code : f.type === 'mention' ? f.name : f.type === 'link' ? f.url : f.text;
      if (f.type === 'emote') {
        out.push({ type: f.provider, name: f.code, id: f.id, gif: /\.gif/.test(f.image), urls: { 1: f.image, 2: f.image, 4: f.image }, start: at, end: at + t.length - 1 });
      }
      at += t.length;
    });
    return out;
  }
  function badges(e) {
    return ((e.user && e.user.badges) || []).map(function (b) {
      return { type: b.set, version: b.version || '1', url: b.image || '', description: b.set };
    });
  }
  function message(e) {
    return {
      listener: 'message',
      event: {
        service: e.platform,
        renderedText: L.render(e),
        data: {
          time: e.ts_ms, tags: {}, nick: login(e), userId: e.user ? e.user.id : '', displayName: name(e),
          displayColor: (e.user && e.user.color) || '', badges: badges(e), channel: e.channel, text: e.text,
          isAction: false, emotes: emotes(e), msgId: e.native_id || String(e.id)
        }
      }
    };
  }
  function translate(e) {
    var kind = L.kindOf(e);
    if (kind === 'message') return message(e);
    var t = e.kind && e.kind.type;
    if (t === 'deletion') {
      return e.kind.native_id ? { listener: 'delete-message', event: { msgId: e.kind.native_id } } : { listener: 'delete-messages', event: { userId: e.kind.user_id } };
    }
    var base = { name: login(e), displayName: name(e), amount: amountOf(e), message: e.text || '', type: kind, provider: e.platform, isTest: !!e.test };
    switch (kind) {
      case 'follow': return { listener: 'follower-latest', event: base };
      case 'sub': base.tier = '1000'; base.gifted = /gift/i.test(e.headline || ''); return { listener: 'subscriber-latest', event: base };
      case 'paid': base.currency = e.paid ? e.paid.currency : ''; return { listener: 'tip-latest', event: base };
      case 'cheer': return { listener: 'cheer-latest', event: base };
      case 'raid': return { listener: 'raid-latest', event: base };
    }
    return null;
  }
  function session(s) {
    var latest = s.latest || {}, counts = s.counts || {}, totals = s.totals || {};
    function last(k) { var v = latest[k]; return v ? { name: v.name, amount: v.amount, message: v.text || '' } : { name: '', amount: 0, message: '' }; }
    var money = 0;
    Object.keys(totals).forEach(function (c) { if (c !== 'BITS') money += totals[c]; });
    var top = s.top ? { name: s.top.name, amount: s.top.amount } : { name: '', amount: 0 };
    return {
      'follower-latest': last('follow'), 'follower-session': { count: counts.follow || 0 }, 'follower-total': { count: counts.follow || 0 },
      'subscriber-latest': last('sub'), 'subscriber-session': { count: counts.sub || 0 }, 'subscriber-total': { count: counts.sub || 0 },
      'tip-latest': last('paid'), 'tip-session': { amount: money }, 'tip-total': { amount: money }, 'tip-session-top-donation': top,
      'cheer-latest': last('cheer'), 'cheer-session': { amount: totals.BITS || 0 }, 'cheer-total': { amount: totals.BITS || 0 },
      'raid-latest': last('raid')
    };
  }
  window.SE_API = {
    store: {
      get: function (k) { return Promise.resolve(L.storage['se:' + k] == null ? null : L.storage['se:' + k]); },
      set: function (k, v) { L.store('se:' + k, v); }
    },
    counters: { get: function (c) { return Promise.resolve({ counter: c, value: L.storage['counter:' + c] || 0 }); } },
    resumeQueue: function () { L.done(); },
    sanitize: function (o) { return Promise.resolve({ result: { message: (o && o.message) || '' }, skip: false }); },
    cheerFilter: function (m) { return Promise.resolve(String(m || '').replace(/\b[A-Za-z]+\d+\b/g, '').replace(/\s+/g, ' ').trim()); },
    setField: function () {},
    getOverlayStatus: function () { return Promise.resolve({ isEditorMode: false, muted: false }); }
  };
  L.on('load', function () {
    fire('onWidgetLoad', {
      channel: { username: '', apiToken: '', id: '', providerId: '', avatar: '' },
      currency: { code: 'USD', name: 'U.S. Dollar', symbol: '$' },
      fieldData: L.fields,
      recents: (L.recents || []).map(translate).filter(Boolean).filter(function (x) { return x.listener !== 'message'; }).map(function (x) { return x.event; }),
      session: { data: session(L.session), settings: { autoReset: false } },
      overlay: { isEditorMode: false, muted: false }
    });
  });
  L.on('event', function (e) { var t = translate(e); if (t) fire('onEventReceived', t); });
  L.on('session', function (s) { fire('onSessionUpdate', { session: session(s) }); });
  L.on('button', function (n) { fire('onEventReceived', { listener: 'widget-button', event: { field: n, value: '' } }); });
  L.on('fields', function () { location.reload(); });
})();
