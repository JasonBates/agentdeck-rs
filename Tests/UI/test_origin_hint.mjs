// The bridge refuses /api and /events for any Host it has not been configured with.
// Behind a proxy the page still loads, so the UI must turn that 403 into the exact
// configuration lines rather than an indefinite "bridge offline".
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../../Public/index.html', import.meta.url), 'utf8');
const start = source.indexOf('  function originRejectedConfig');
const end = source.indexOf('  function scheduleAuthenticatedReconnect', start);
assert.ok(start >= 0 && end > start, 'origin hint helpers must remain extractable');

function api(location, responses) {
  const context = {
    location,
    originPanel: { hidden: true },
    originHelp: { textContent: '' },
    originConfig: { textContent: '' },
    cleared: 0,
    hidden: 0,
    clearActiveConnection() { context.cleared += 1; },
    hideAuthPrompt() { context.hidden += 1; },
    globalThis: {}
  };
  vm.runInNewContext(
    source.slice(start, end) +
      '\nglobalThis.api = { originRejectedConfig, isOriginRejected, showOriginRejected };',
    context
  );
  return { ...context.globalThis.api, context };
}

const response = (status, body) => ({
  status,
  clone() { return { json: () => body === undefined ? Promise.reject(new Error('no body')) : Promise.resolve(body) }; }
});

test('a plain https address maps to server.public_host', () => {
  const ui = api({ protocol: 'https:', hostname: 'studio.tail1234.ts.net', port: '',
    origin: 'https://studio.tail1234.ts.net' });
  assert.equal(ui.originRejectedConfig(), '[server]\npublic_host = "studio.tail1234.ts.net"');
});

test('an address with a port or without TLS maps to an exact allowed origin', () => {
  const withPort = api({ protocol: 'https:', hostname: 'studio.tail1234.ts.net', port: '9797',
    origin: 'https://studio.tail1234.ts.net:9797' });
  assert.equal(withPort.originRejectedConfig(),
    '[security]\nallowed_origins = ["https://studio.tail1234.ts.net:9797"]');
  const plainHttp = api({ protocol: 'http:', hostname: '100.101.102.103', port: '9798',
    origin: 'http://100.101.102.103:9798' });
  assert.equal(plainHttp.originRejectedConfig(),
    '[security]\nallowed_origins = ["http://100.101.102.103:9798"]');
});

test('only a 403 whose body says origin_rejected counts', async () => {
  const ui = api({ protocol: 'https:', hostname: 'h', port: '', origin: 'https://h' });
  assert.equal(await ui.isOriginRejected(response(403, { ok: false, error: 'origin_rejected' })), true);
  assert.equal(await ui.isOriginRejected(response(403, { ok: false, error: 'forbidden' })), false);
  assert.equal(await ui.isOriginRejected(response(403)), false);
  assert.equal(await ui.isOriginRejected(response(401, { error: 'origin_rejected' })), false);
});

test('showing the hint stops reconnecting and prints the address and config', () => {
  const ui = api({ protocol: 'https:', hostname: 'studio.tail1234.ts.net', port: '',
    origin: 'https://studio.tail1234.ts.net' });
  ui.showOriginRejected();
  assert.equal(ui.context.cleared, 1);
  assert.equal(ui.context.hidden, 1);
  assert.equal(ui.context.originPanel.hidden, false);
  assert.match(ui.context.originHelp.textContent, /https:\/\/studio\.tail1234\.ts\.net/);
  assert.equal(ui.context.originConfig.textContent, '[server]\npublic_host = "studio.tail1234.ts.net"');
});

test('both transports route a 403 through the origin check', () => {
  assert.match(source, /if \(response\.status === 403\) \{\s*return isOriginRejected\(response\)\.then\(function \(rejected\) \{\s*if \(rejected\) \{ showOriginRejected\(\); return null; \}\s*throw new Error\('stream unavailable'\);/);
  assert.match(source, /if \(response\.status === 403\) \{\s*return isOriginRejected\(response\)\.then\(function \(rejected\) \{\s*if \(rejected\) showOriginRejected\(\);\s*return null;/);
  assert.match(source, /originReload\.addEventListener\('click', function \(\) \{ location\.reload\(\); \}\)/);
});
