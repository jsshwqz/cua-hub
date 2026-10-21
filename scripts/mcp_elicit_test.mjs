// Elicitation smoke test: a replay with a deny-listed but harmless command
// (echo mkfs test) must ask the client before executing.
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import readline from 'node:readline';

// Self-contained fixture: a deny-listed (high risk) but harmless command.
fs.mkdirSync('.runs/elicit-test', { recursive: true });
fs.writeFileSync(
  '.runs/elicit-test/steps.jsonl',
  [
    '{"index":0,"action":{"kind":"run_command","command":"echo mkfs test","shell":"auto"},"note":null,"at":"2026-10-21T00:00:00Z"}',
    '{"index":1,"action":{"kind":"done","summary":"ok"},"note":null,"at":"2026-10-21T00:00:01Z"}',
    '',
  ].join('\n'),
);

const bin = process.argv[2] ?? './target/debug/cua-hub.exe';
const child = spawn(bin, ['serve'], { stdio: ['pipe', 'pipe', 'inherit'] });
const rl = readline.createInterface({ input: child.stdout });
const pending = new Map();
let mode = 'accept';
let nextId = 1;

rl.on('line', (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg.method === 'elicitation/create') {
    console.log('elicitation:', msg.params.message.slice(0, 90));
    child.stdin.write(
      JSON.stringify({
        jsonrpc: '2.0',
        id: msg.id,
        result: { action: mode, content: { approve: mode === 'accept' } },
      }) + '\n',
    );
    return;
  }
  if (msg.id != null && pending.has(msg.id)) {
    pending.get(msg.id)(msg);
    pending.delete(msg.id);
  }
});

function send(method, params) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    pending.set(id, resolve);
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    setTimeout(() => reject(new Error(`timeout: ${method}`)), 90000);
  });
}

try {
  await send('initialize', {});
  const replay = () =>
    send('tools/call', {
      name: 'replay_run',
      arguments: { dir: '.runs/elicit-test' },
    });

  mode = 'accept';
  const approved = await replay();
  console.log(
    'accept -> isError=%s text=%s',
    approved.result.isError,
    approved.result.content[0].text.slice(0, 140),
  );

  mode = 'decline';
  const declined = await replay();
  console.log(
    'decline -> isError=%s text=%s',
    declined.result.isError,
    declined.result.content[0].text.slice(0, 140),
  );

  const ok =
    approved.result.isError === false && declined.result.isError === true;
  console.log(ok ? 'ELICIT_SMOKE_OK' : 'ELICIT_SMOKE_FAIL');
  process.exitCode = ok ? 0 : 1;
} catch (e) {
  console.error('ELICIT_SMOKE_FAIL', e);
  process.exitCode = 1;
} finally {
  child.kill();
}
