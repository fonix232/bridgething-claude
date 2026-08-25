// Only what desktop-app/hooks/scripts/{install,uninstall}-hooks.js need — the
// rest of this used to configure the JS daemon itself, which is now the Rust
// app in ../../src-tauri/.

import os from 'node:os';
import path from 'node:path';

export const PORT = Number(process.env.CLAUDE_THING_PORT) || 8790;

export const CLAUDE_DIR = process.env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), '.claude');
export const CLAUDE_SETTINGS = path.join(CLAUDE_DIR, 'settings.json');

// Must exceed PERMISSION_HOLD_MS in desktop-app/src-tauri/src/daemon/config.rs
// (currently ~595s), or Claude Code gives up on the hook before the app does.
export const HOOK_TIMEOUT_S = 600;

