import { strict as assert } from 'node:assert';
import { test } from 'node:test';

// Note: these tests require the native module to be built first:
//   cd crates/apprise-napi && npm run build

test('listServices returns array', async () => {
  const { listServices } = await import('../index.js');
  const services = listServices();
  assert.ok(Array.isArray(services));
  assert.ok(services.length > 100, `Expected 100+ services, got ${services.length}`);
});

test('parseUrl returns service info for valid URL', async () => {
  const { parseUrl } = await import('../index.js');
  const info = parseUrl('json://localhost');
  assert.ok(info !== null);
  assert.equal(info.name, 'Custom JSON');
  assert.ok(info.protocols.includes('json'));
});

test('parseUrl returns null for invalid URL', async () => {
  const { parseUrl } = await import('../index.js');
  const info = parseUrl('invalid://');
  assert.equal(info, null);
});

test('Apprise class can add services', async () => {
  const { Apprise } = await import('../index.js');
  const a = new Apprise();
  assert.equal(a.len(), 0);
  assert.ok(a.add('json://localhost'));
  assert.equal(a.len(), 1);
});

test('Apprise class details returns service info', async () => {
  const { Apprise } = await import('../index.js');
  const a = new Apprise();
  a.add('json://localhost');
  const details = a.details();
  assert.equal(details.length, 1);
  assert.equal(details[0].name, 'Custom JSON');
});

test('Apprise class clear removes all services', async () => {
  const { Apprise } = await import('../index.js');
  const a = new Apprise();
  a.add('json://localhost');
  a.add('xml://localhost');
  assert.equal(a.len(), 2);
  a.clear();
  assert.equal(a.len(), 0);
});
