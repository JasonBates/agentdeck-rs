// No browser dependency: exercise the pure capability helpers embedded in the
// single-file UI. Browser/SSE integration remains covered by a future bridge preview.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../../Public/index.html', import.meta.url), 'utf8');
const start = source.indexOf('  var SETUP_DISMISS_PREFIX');
const end = source.indexOf('  function renderSetupHints', start);
assert.ok(start >= 0 && end > start, 'setup hint helpers must remain extractable');
const plain = value => JSON.parse(JSON.stringify(value));

function storage(initial = {}) {
  const values = new Map(Object.entries(initial));
  return {
    get length() { return values.size; },
    key(index) { return [...values.keys()][index] ?? null; },
    getItem(key) { return values.get(key) ?? null; },
    setItem(key, value) { values.set(key, String(value)); },
    removeItem(key) { values.delete(key); },
    values
  };
}

function api(initialStorage) {
  const localStorage = storage(initialStorage);
  const context = {
    URL, localStorage,
    location: { href: 'https://deck.example/deck/' },
    globalThis: {}
  };
  vm.runInNewContext(
    "var BASE = '/deck/';\n" + source.slice(start, end) +
      '\nglobalThis.api = { collectSetupHints, safeDocsPath, resetSetupHints, dismissedSetupHint };',
    context
  );
  return { ...context.globalThis.api, localStorage };
}

function escapeHTML(value) {
  const escStart = source.indexOf('  function esc(s) {');
  const escEnd = source.indexOf('\n  function phaseHTML', escStart);
  return new Function(source.slice(escStart, escEnd) + '\nreturn esc;')()(value);
}

function restoreBaselineCap(panel) {
  const restoreStart = source.indexOf('  function restoreBaselineCapacityPanel() {');
  const restoreEnd = source.indexOf('\n  function renderFooterBaseline', restoreStart);
  return new Function('cap', source.slice(restoreStart, restoreEnd) +
    '\nreturn restoreBaselineCapacityPanel;')(panel)();
}

const hint = (message, backend = undefined) => ({
  state: 'missing', ...(backend ? { backend } : {}),
  setupHint: { message, actionLabel: 'Learn more', docsPath: 'docs/setup.html' }
});

test('baseline capability absence has no setup entries and retains the data footer', () => {
  const ui = api();
  assert.deepEqual(plain(ui.collectSetupHints(undefined, () => false)), { hints: [], missing: false });
  assert.match(source, /if \(!d\.capabilities \|\| typeof d\.capabilities !== 'object'\) \{\s*renderSetupHints\(null\);\s*featureErrors\.textContent = '';\s*renderFooterBaseline\(d\)/);
  const cap = { hidden: true, className: 'quota warn' };
  restoreBaselineCap(cap);
  assert.deepEqual(cap, { hidden: false, className: 'quota' });
});

test('startup and runtime prose uses bounded footer and setup cells', () => {
  assert.match(source, /\.quota\.footer-message\s*\{[^}]*overflow:\s*hidden;[^}]*overflow-wrap:\s*anywhere;/s);
  assert.match(source, /\.feature-errors\s*\{[^}]*max-height:\s*48px;[^}]*overflow:\s*hidden;[^}]*overflow-wrap:\s*anywhere;/s);
  assert.match(source, /\.sys \.block\s*\{[^}]*overflow:\s*hidden;/s);
  assert.match(source, /\.setup-hint-message\s*\{[^}]*overflow-wrap:\s*anywhere;/s);
  assert.match(source, /\.setup-hints\s*\{[^}]*max-height:\s*72px;[^}]*overflow-y:\s*auto;/s);
  assert.match(source, /\.quota\.footer-message\s*\{\s*width:\s*min\(210px, 45vw\);\s*flex-basis:\s*210px;/s);
  assert.match(source, /\.setup-hints\s*\{\s*max-height:\s*46px;/s);
  assert.equal((source.match(/cap\.className = 'quota footer-message warn'/g) || []).length, 2);
});

test('missing hints dedupe by backend and show at most two', () => {
  const ui = api();
  const result = ui.collectSetupHints({
    headings: hint('Install Ollama', 'ollama'),
    localModelTelemetry: hint('Install Ollama again', 'ollama'),
    capacity: hint('Install CodexBar', 'codexbar'),
    hostTelemetry: hint('Install another optional tool', 'other')
  }, () => false);
  assert.deepEqual(plain(result.hints.map(entry => entry.key)), ['ollama', 'codexbar']);
});

test('dismissal reset removes only AgentDeck setup keys', () => {
  const ui = api({
    'agentdeck.setupHint.dismissed.v1.ollama': '1',
    'agentdeck.setupHint.dismissed.v1.codexbar': '1',
    'agentdeck.slots': '{}',
    'another.app.key': 'keep'
  });
  assert.equal(ui.dismissedSetupHint('ollama'), true);
  ui.resetSetupHints();
  assert.equal(ui.localStorage.getItem('agentdeck.setupHint.dismissed.v1.ollama'), null);
  assert.equal(ui.localStorage.getItem('agentdeck.slots'), '{}');
  assert.equal(ui.localStorage.getItem('another.app.key'), 'keep');
});

test('disabled, unsupported, and error states produce no install hint or Recommended setup error', () => {
  const ui = api();
  const result = ui.collectSetupHints({
    headings: { state: 'disabled' },
    capacity: { state: 'unsupported' },
    hostTelemetry: { state: 'error', reason: 'sampler_failed', setupHint: { message: 'do not show' } }
  }, () => false);
  assert.equal(result.hints.length, 0);
  assert.equal(result.missing, false);
  assert.doesNotMatch(source.slice(source.indexOf('  function renderSetupHints'), source.indexOf('  function renderFeatureErrors')), /unavailable/);
  assert.match(source, /function renderFeatureErrors\(capabilities\)/);
  assert.match(source, /if \(hostStatus && hostStatus\.state === 'error'\) systemErrors\.push/);
});

test('docs routes remain inside AgentDeck base path', () => {
  const ui = api();
  assert.equal(ui.safeDocsPath('docs/setup.html'), 'https://deck.example/deck/docs/setup.html');
  for (const unsafe of [
    'https://example.com', '//example.com', '/docs/setup.html', '../other',
    'docs/../../other', 'docs/%2e%2e/%2e%2e/other', '%2F%2Fevil.example',
    'docs/%5c..%5cother', 'javascript:alert(1)'
  ]) {
    assert.equal(ui.safeDocsPath(unsafe), null, unsafe);
  }
});

test('hint text and attribute values use the UI HTML escaper', () => {
  assert.equal(escapeHTML('"<&>'), '&quot;&lt;&amp;&gt;');
  assert.match(source, /data-copy-command="' \+\s*esc\(hint\.command\)/);
  assert.match(source, /data-dismiss-key="' \+\s*esc\(entry\.key\)/);
});

test('a dismissed missing provider keeps only the reset setup row; restoring availability removes it', () => {
  const ui = api({ 'agentdeck.setupHint.dismissed.v1.ollama': '1' });
  const missing = ui.collectSetupHints({ headings: hint('Install Ollama', 'ollama') }, ui.dismissedSetupHint);
  assert.deepEqual(plain(missing), { hints: [], missing: true });
  const available = ui.collectSetupHints({ headings: { state: 'available' } }, ui.dismissedSetupHint);
  assert.deepEqual(plain(available), { hints: [], missing: false });
});
