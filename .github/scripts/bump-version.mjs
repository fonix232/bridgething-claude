#!/usr/bin/env node
// Single source of truth for "the project version" is the root VERSION file.
// Bumps the patch component and propagates it into every file that needs to
// carry it, so bridgething-app and desktop-app always release in lockstep.
//
//   node .github/scripts/bump-version.mjs           # bump patch, write everywhere
//   node .github/scripts/bump-version.mjs --dry-run # print the new version only

import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const VERSION_FILE = path.join(ROOT, 'VERSION');
const dryRun = process.argv.includes('--dry-run');

const current = readFileSync(VERSION_FILE, 'utf8').trim();
const parts = current.split('.').map(Number);
if (parts.length !== 3 || parts.some(Number.isNaN)) {
  throw new Error(`VERSION file has an unexpected shape: "${current}"`);
}
parts[2] += 1;
const next = parts.join('.');

if (dryRun) {
  console.log(next);
  process.exit(0);
}

function bumpJson(relPath) {
  const file = path.join(ROOT, relPath);
  const json = JSON.parse(readFileSync(file, 'utf8'));
  json.version = next;
  writeFileSync(file, JSON.stringify(json, null, 2) + '\n');
}

function bumpCargoToml(relPath) {
  const file = path.join(ROOT, relPath);
  const text = readFileSync(file, 'utf8');
  // Only the package's own `version = "…"` line, at the start of a line (not
  // a dependency's inline `{ version = "…" }` table entry).
  const updated = text.replace(/^version = "[^"]+"/m, `version = "${next}"`);
  writeFileSync(file, updated);
}

bumpJson('bridgething-app/package.json');
bumpJson('bridgething-app/public/manifest.json');
bumpJson('desktop-app/package.json');
bumpJson('desktop-app/hooks/package.json');
bumpJson('desktop-app/src-tauri/tauri.conf.json');
bumpCargoToml('desktop-app/src-tauri/Cargo.toml');
writeFileSync(VERSION_FILE, next + '\n');

console.log(next);
