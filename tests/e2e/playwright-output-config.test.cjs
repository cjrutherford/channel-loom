'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const configPath = path.resolve(__dirname, '../../playwright.config.ts');
function resolveConfig(cwd) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ['-e', 'const c = require(process.argv[1]).default; process.stdout.write(JSON.stringify({outputDir:c.outputDir, report:c.reporter[1][1].outputFolder}))', configPath], { cwd });
    let out = '', err = '';
    child.stdout.on('data', chunk => out += chunk);
    child.stderr.on('data', chunk => err += chunk);
    child.on('error', reject);
    child.on('close', code => code === 0 ? resolve(JSON.parse(out)) : reject(new Error(err)));
  });
}
test('config isolates concurrent output and leaves manual evidence untouched', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'awesome-tv-config-test-'));
  try {
    const manual = path.join(root, 'test-results');
    fs.mkdirSync(manual);
    const sentinel = path.join(manual, 'manual-evidence.txt');
    fs.writeFileSync(sentinel, 'preserve this evidence');
    const [a, b] = await Promise.all([resolveConfig(root), resolveConfig(root)]);
    for (const config of [a, b]) {
      assert.match(config.outputDir, /^\/tmp\/awesome-tv-playwright-[0-9a-f-]+\/results$/);
      assert.equal(config.report, path.join(path.dirname(config.outputDir), 'report'));
      assert.equal(fs.existsSync(config.outputDir), false);
    }
    assert.notEqual(a.outputDir, b.outputDir);
    assert.equal(fs.readFileSync(sentinel, 'utf8'), 'preserve this evidence');
    assert.deepEqual(fs.readdirSync(manual), ['manual-evidence.txt']);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
