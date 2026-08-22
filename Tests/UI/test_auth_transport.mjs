import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../../Public/index.html', import.meta.url), 'utf8');
const start = source.indexOf("  var AUTH_STORAGE_KEY");
const end = source.indexOf("  function receivePayload", start);
assert.ok(start >= 0 && end > start, 'auth/SSE helpers must remain extractable');

function parser() {
  const sessionStorage = { getItem() { return null; } };
  const context = { sessionStorage, globalThis: {} };
  vm.runInNewContext(
    source.slice(start, end) + '\nglobalThis.consumeSseText = consumeSseText;',
    context
  );
  return context.globalThis.consumeSseText;
}

function authApi(initialToken, responseStatus) {
  let stored = initialToken;
  const requests = [];
  const context = {
    sessionStorage: {
      getItem() { return stored; },
      setItem(_key, value) { stored = value; },
      removeItem() { stored = null; }
    },
    authStatus: { textContent: '' },
    authTokenInput: { value: 'visible-secret', focus() {} },
    authPanel: { hidden: true },
    BASE: '/deck/',
    fetch(url, options) {
      requests.push({ url, options });
      return Promise.resolve({ status: responseStatus, ok: responseStatus === 200 });
    },
    setTimeout(callback) { callback(); return 1; },
    clearTimeout() {},
    globalThis: {}
  };
  vm.runInNewContext(
    source.slice(start, end) +
      '\nglobalThis.api = { authorizedFetch, authHeaders, storedToken: function () { return authToken; } };',
    context
  );
  return { ...context.globalThis.api, context, requests, stored: () => stored };
}

test('authenticated transport never puts the token in localStorage, a URL, or retained input DOM', () => {
  assert.match(source, /sessionStorage\.setItem\(AUTH_STORAGE_KEY, replacement\)/);
  assert.match(source, /sessionStorage\.removeItem\(AUTH_STORAGE_KEY\)/);
  assert.doesNotMatch(source, /localStorage\.(?:setItem|getItem)\([^\n]*authToken/i);
  assert.doesNotMatch(source, /[?&](?:token|auth)=/i);
  assert.match(source, /authTokenInput\.value = '';\s*if \(!replacement\) return;/);
  assert.match(source, /headers\.Authorization = 'Bearer ' \+ authToken/);
  assert.match(source, /if \(response\.status === 401\) handleUnauthorized\(\)/);
});

test('local mode retains EventSource while token mode uses fetch streaming and abort', () => {
  assert.match(source, /activeEventSource = new EventSource\(BASE \+ 'events'\)/);
  assert.match(source, /authorizedFetch\('events', \{ signal: controller\.signal \}\)/);
  assert.match(source, /var controller = new AbortController\(\)/);
  assert.match(source, /reconnectDelay = Math\.min\(reconnectDelay \* 2, 30000\)/);
});

test('a 401 clears the active token and reveals the accessible prompt without leaking into the URL', async () => {
  const ui = authApi('0123456789abcdef0123456789abcdef', 401);
  await ui.authorizedFetch('api/snapshot');
  assert.equal(ui.requests[0].url, '/deck/api/snapshot');
  assert.equal(
    ui.requests[0].options.headers.Authorization,
    'Bearer 0123456789abcdef0123456789abcdef'
  );
  assert.equal(ui.storedToken(), '');
  assert.equal(ui.stored(), null);
  assert.equal(ui.context.authTokenInput.value, '');
  assert.equal(ui.context.authPanel.hidden, false);
  assert.doesNotMatch(ui.requests[0].url, /0123456789abcdef/);
});

test('strict SSE parser accepts split exact data frames and preserves the remainder', () => {
  const consume = parser();
  let parsed = consume('', 'data: {"n":1}\n');
  assert.deepEqual(JSON.parse(JSON.stringify(parsed)), {
    remainder: 'data: {"n":1}\n', messages: []
  });
  parsed = consume(parsed.remainder, '\ndata: {"n":2}\n\n');
  assert.deepEqual(JSON.parse(JSON.stringify(parsed)), {
    remainder: '', messages: ['{"n":1}', '{"n":2}']
  });
});

test('strict SSE parser rejects comments, named events, multiline data, CRLF, and invalid JSON', () => {
  const consume = parser();
  for (const invalid of [
    ': ping\n\n',
    'event: state\ndata: {}\n\n',
    'data: {}\ndata: {}\n\n',
    'data: {}\r\n\r\n',
    'data: not-json\n\n',
    'data: true\n\n',
    'data: []\n\n',
    'data: \n\n'
  ]) {
    assert.throws(() => consume('', invalid), /invalid SSE stream|Unexpected token|JSON/);
  }
});

test('strict SSE parser bounds an unterminated stream', () => {
  const consume = parser();
  assert.throws(
    () => consume('', 'x'.repeat(4 * 1024 * 1024 + 1)),
    /invalid SSE stream/
  );
});
