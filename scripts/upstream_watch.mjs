// Compare the tracked upstreams against integrations/upstreams.json and print
// a markdown report. Exit code is always 0; the workflow greps for "| update |".
import fs from 'node:fs';

const cfg = JSON.parse(fs.readFileSync('integrations/upstreams.json', 'utf8'));
const token = process.env.GITHUB_TOKEN || process.env.GH_TOKEN || '';
const ghHeaders = token ? { Authorization: `Bearer ${token}` } : {};

async function latestTag(repo) {
  try {
    let r = await fetch(`https://api.github.com/repos/${repo}/releases/latest`, {
      headers: { ...ghHeaders, Accept: 'application/vnd.github+json' },
    });
    if (r.ok) {
      const j = await r.json();
      if (j.tag_name) return j.tag_name;
    }
    r = await fetch(`https://api.github.com/repos/${repo}/tags?per_page=1`, {
      headers: { ...ghHeaders, Accept: 'application/vnd.github+json' },
    });
    if (r.ok) {
      const j = await r.json();
      return j?.[0]?.name ?? null;
    }
  } catch (e) {}
  return null;
}

async function latestCrate(name) {
  try {
    const r = await fetch(`https://crates.io/api/v1/crates/${name}`, {
      headers: { 'User-Agent': 'cua-hub-upstream-watch' },
    });
    if (!r.ok) return null;
    const j = await r.json();
    return j?.crate?.max_stable_version ?? j?.crate?.newest_version ?? null;
  } catch (e) {
    return null;
  }
}

function versionNumbers(v) {
  if (!v) return [];
  const m = String(v).match(/\d+(?:\.\d+)+/);
  const raw = m ? m[0] : String(v).replace(/^v/, '');
  return raw.split('.').map((x) => parseInt(x, 10)).filter(Number.isFinite);
}

function isNewer(baseline, latest) {
  if (!baseline || !latest) return false;
  const a = versionNumbers(baseline);
  const b = versionNumbers(latest);
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const x = a[i] ?? 0;
    const y = b[i] ?? 0;
    if (x !== y) return y > x;
  }
  return false;
}

const rows = [];
for (const item of cfg.github) {
  const latest = await latestTag(item.repo);
  rows.push({
    kind: 'github',
    name: item.repo,
    baseline: item.tag,
    latest,
    status: !latest ? 'unknown' : isNewer(item.tag, latest) ? 'update' : 'ok',
    affects: item.affects,
  });
}
for (const item of cfg.crates) {
  const latest = await latestCrate(item.name);
  rows.push({
    kind: 'crate',
    name: item.name,
    baseline: item.version,
    latest,
    status: !latest ? 'unknown' : isNewer(item.version, latest) ? 'update' : 'ok',
    affects: item.affects,
  });
}

const updates = rows.filter((r) => r.status === 'update');
console.log(`# Upstream watch — ${new Date().toISOString().slice(0, 10)}`);
console.log(`\nTracked: ${rows.length} · updates: ${updates.length}\n`);
console.log('| type | name | baseline | latest | status | affects |');
console.log('|------|------|----------|--------|--------|---------|');
for (const r of rows) {
  console.log(
    `| ${r.kind} | ${r.name} | ${r.baseline} | ${r.latest ?? '?'} | ${r.status} | ${r.affects} |`,
  );
}
if (updates.length) {
  console.log('\n## Adopt');
  console.log('1. Read the upstream release notes for the rows marked `update`.');
  console.log('2. Change only the adapter or dependency, not the core interfaces.');
  console.log('3. Run `node scripts/mcp_smoke.mjs`, `node scripts/mcp_elicit_test.mjs`,');
  console.log('   `cargo run -p cua-engine-browser --example browser_smoke`, then CI.');
  console.log('4. Update `integrations/upstreams.json` and CHANGELOG after merging.');
}
