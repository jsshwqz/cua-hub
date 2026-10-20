// MCP stdio smoke test: handshake, tools/list, accessibility tree, find, capture.
// Usage: node scripts/mcp_smoke.mjs [path-to-binary]
import { spawn } from 'node:child_process';
import readline from 'node:readline';

const bin = process.argv[2] ?? './target/debug/cua-hub.exe';
const child = spawn(bin, ['serve'], { stdio: ['pipe', 'pipe', 'inherit'] });
const rl = readline.createInterface({ input: child.stdout });
const pending = new Map();

rl.on('line', (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    console.error('non-JSON line:', line);
    return;
  }
  if (msg.id != null && pending.has(msg.id)) {
    pending.get(msg.id)(msg);
    pending.delete(msg.id);
  }
});

let nextId = 1;
function send(method, params) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    pending.set(id, resolve);
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    setTimeout(() => reject(new Error(`timeout: ${method}`)), 60000);
  });
}

try {
  const init = await send('initialize', {
    protocolVersion: '2024-11-05',
    capabilities: {},
    clientInfo: { name: 'smoke', version: '0' },
  });
  console.log('initialize:', init.result.serverInfo);

  const list = await send('tools/list', {});
  console.log('tools:', list.result.tools.map((t) => t.name).join(', '));

  const tree = await send('tools/call', {
    name: 'accessibility_tree',
    arguments: { max_nodes: 20, depth: 1 },
  });
  console.log(
    'tree: isError=%s head=%s',
    tree.result.isError,
    tree.result.content[0].text.slice(0, 100).replace(/\n/g, ' / '),
  );

  const find = await send('tools/call', {
    name: 'find_control',
    arguments: { name: '开始' },
  });
  console.log('find: isError=%s %s', find.result.isError, find.result.content[0].text);

  const cap = await send('tools/call', { name: 'capture_screen', arguments: {} });
  const image = cap.result.content.find((c) => c.type === 'image');
  console.log(
    'capture: isError=%s types=%s image_b64_len=%d',
    cap.result.isError,
    cap.result.content.map((c) => c.type).join('+'),
    image ? image.data.length : 0,
  );

  console.log('MCP_SMOKE_OK');
} catch (e) {
  console.error('MCP_SMOKE_FAIL', e);
  process.exitCode = 1;
} finally {
  child.kill();
}
