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

test('Apprise class loads configuration asynchronously', async () => {
  const { Apprise } = await import('../index.js');
  const { mkdtempSync, writeFileSync } = await import('node:fs');
  const { join } = await import('node:path');
  const { tmpdir } = await import('node:os');
  const dir = mkdtempSync(join(tmpdir(), 'apprise-napi-'));
  const path = join(dir, 'apprise.yml');
  writeFileSync(path, 'urls:\n  - json://localhost:\n      tag: alpha\n  - xml://localhost\n');
  const a = new Apprise();
  expect(await a.addConfig(path)).toBe(2);
  expect(a.len()).toBe(2);
});

test('Apprise.notify only notifies the services the tags select', async () => {
  const { Apprise } = await import('../index.js');
  const hits: string[] = [];
  const server = Bun.serve({
    port: 0,
    async fetch(req) {
      if (req.method === 'POST') {
        hits.push(new URL(req.url).pathname);
      }
      return new Response('ok');
    },
  });
  try {
    const a = new Apprise();
    await a.addConfig(
      await (async () => {
        const { mkdtempSync, writeFileSync } = await import('node:fs');
        const { join } = await import('node:path');
        const { tmpdir } = await import('node:os');
        const path = join(mkdtempSync(join(tmpdir(), 'apprise-napi-')), 'apprise.conf');
        writeFileSync(path, `alpha=json://127.0.0.1:${server.port}/alpha\nbeta=json://127.0.0.1:${server.port}/beta\n`);
        return path;
      })(),
    );
    const result = await a.notify('hello', { tags: ['beta'] });
    expect(result.status).toBe('success');
    expect(result.total).toBe(1);
    expect(hits).toEqual(['/beta']);
  } finally {
    server.stop(true);
  }
});

test('notify rejects an unknown notification type', async () => {
  const { Apprise } = await import('../index.js');
  const a = new Apprise();
  a.add('json://localhost');
  await expect(a.notify('hello', { notificationType: 'bogus' })).rejects.toThrow();
});

test('sync methods do not wait for a notification in progress', async () => {
  const { Apprise } = await import('../index.js');
  const server = Bun.serve({
    port: 0,
    async fetch() {
      await Bun.sleep(1500);
      return new Response('ok');
    },
  });
  try {
    const a = new Apprise();
    a.add(`json://127.0.0.1:${server.port}/slow`);
    const pending = a.notify('hello');
    await Bun.sleep(100);
    const started = performance.now();
    expect(a.add('json://localhost/other')).toBe(true);
    expect(a.len()).toBe(2);
    expect(performance.now() - started).toBeLessThan(500);
    const result = await pending;
    // The notification already under way kept the services it started with
    expect(result.total).toBe(1);
  } finally {
    server.stop(true);
  }
});
