// Streamlabs compatibility: a Streamlabs custom widget (its HTML, CSS, JS and fields) runs
// unchanged. Lightair's events become Streamlabs' document events: onLoad (the fields) and
// onEventReceived (chat as IRC-like PRIVMSG, alerts as {type, for, message: [...]}).
(function () {
  var L = window.lightair;
  function fire(name, detail) { document.dispatchEvent(new CustomEvent(name, { detail: detail })); }
  function name(e) { return e.user ? e.user.display_name : ''; }
  function login(e) { return e.user ? (e.user.login || e.user.display_name) : ''; }
  function amountOf(e) {
    if (e.kind && e.kind.viewers != null) return e.kind.viewers;
    if (e.paid) return e.paid.amount;
    var m = /(\d+)\s+months?/.exec(e.headline || '');
    return m ? +m[1] : 1;
  }
  function chat(e) {
    var u = e.user || {};
    return {
      command: 'PRIVMSG', platform: e.platform, owner: false, messageId: e.native_id || String(e.id),
      from: login(e), body: e.text || '', rendered: L.render(e),
      tags: { 'display-name': name(e), color: u.color || '', 'user-id': u.id || '', badges: (u.badges || []).map(function (b) { return b.set + '/' + (b.version || '1'); }).join(','), id: e.native_id || String(e.id) }
    };
  }
  function translate(e) {
    var kind = L.kindOf(e), t = e.kind && e.kind.type;
    if (kind === 'message') return chat(e);
    if (t === 'deletion') return e.kind.native_id ? { command: 'CLEARMSG', messageId: e.kind.native_id } : { command: 'CLEARCHAT', userId: e.kind.user_id };
    var types = { follow: 'follow', sub: 'subscription', paid: 'donation', cheer: 'bits', raid: 'raid' };
    if (!types[kind]) return null;
    var amount = amountOf(e), currency = e.paid ? e.paid.currency : '';
    return {
      type: types[kind], for: e.platform === 'streamlabs' ? 'streamlabs' : e.platform + '_account',
      message: [{ name: name(e), from: name(e), amount: amount, formatted_amount: (currency && currency !== 'BITS' ? currency + ' ' : '') + amount,
        currency: currency, message: e.text || '', months: kind === 'sub' ? amount : undefined, raiders: kind === 'raid' ? amount : undefined,
        isTest: !!e.test, _id: String(e.id) }]
    };
  }
  L.on('load', function () { fire('onLoad', L.fields); });
  L.on('event', function (e) { var t = translate(e); if (t) fire('onEventReceived', t); });
  L.on('fields', function () { location.reload(); });
})();
