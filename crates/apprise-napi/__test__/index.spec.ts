import { expect, test } from 'bun:test';

// Note: these tests require the native module to be built first:
//   cd crates/apprise-napi && bun run build

test('listServices returns array', async () => {
  const { listServices } = await import('../index.js');
  const services = listServices();
  expect(Array.isArray(services)).toBe(true);
  expect(services.length, `Expected 100+ services, got ${services.length}`).toBeGreaterThan(100);
});

test('parseUrl returns service info for valid URL', async () => {
  const { parseUrl } = await import('../index.js');
  const info = parseUrl('json://localhost');
  expect(info).not.toBeNull();
  expect(info?.name).toBe('JSON');
  expect(info?.protocols).toContain('json');
});

test('parseUrl returns null for invalid URL', async () => {
  const { parseUrl } = await import('../index.js');
  const info = parseUrl('invalid://');
  expect(info).toBeNull();
});

test('Apprise class can add services', async () => {
  const { Apprise } = await import('../index.js');
  const a = new Apprise();
  expect(a.len()).toBe(0);
  expect(a.add('json://localhost')).toBe(true);
  expect(a.len()).toBe(1);
});

test('Apprise class details returns service info', async () => {
  const { Apprise } = await import('../index.js');
  const a = new Apprise();
  a.add('json://localhost');
  const details = a.details();
  expect(details.length).toBe(1);
  expect(details[0].name).toBe('JSON');
});

test('Apprise class clear removes all services', async () => {
  const { Apprise } = await import('../index.js');
  const a = new Apprise();
  a.add('json://localhost');
  a.add('xml://localhost');
  expect(a.len()).toBe(2);
  a.clear();
  expect(a.len()).toBe(0);
});
