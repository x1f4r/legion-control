// Synchronous read-only probes run in a disposable process during status, so
// native SQLite or OS inspection cannot block the snapshot's event loop.

const [kind, serialized] = process.argv.slice(2);
let value;
if (kind === 'boot-identity') {
  const { bootIdentity } = await import('./operations.mjs');
  value = bootIdentity();
} else if (kind === 'sqlite-busy') {
  const { checkT3Sqlite } = await import('./probes/t3-sqlite.mjs');
  const { service, liveness } = JSON.parse(serialized);
  value = checkT3Sqlite(service.busy, { liveness, serviceName: service.name });
} else {
  throw new Error('unknown read-only status probe');
}
process.stdout.write(JSON.stringify(value));
