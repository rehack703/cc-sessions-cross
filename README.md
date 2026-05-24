# cc-sessions

Cross-platform Claude Code and Codex session browser. Fork of [chronologos/cc-sessions](https://github.com/chronologos/cc-sessions) rebuilt with ratatui+crossterm for Windows/Linux/macOS support, plus new features.

## What's different from the original

- **Cross-platform TUI** -- replaced Unix-only `skim` with `ratatui` + `crossterm`
- **Codex support** -- browse and resume sessions from `~/.codex/sessions`
- **Session status** -- mark sessions as Active/Paused/Done (Tab to cycle, persisted to `metadata.json`)
- **Archive/trash flow** -- move finished sessions out of the live picker and restore them later
- **Sort modes** -- press `t` to cycle through modified/created/turns/project/status
- **`--agent` filter** -- show Claude, Codex, or both
- **`--status` filter** -- filter sessions by status from CLI
- **Windows path handling** -- supports `C:\` drive paths and `C--Users-` directory patterns
- **Linux path handling** -- supports `-home-user-` directory patterns

## Installation

### Build from source

Requires Rust 1.88+ (edition 2024) and [just](https://github.com/casey/just).

```bash
just install  # Build and install to ~/.local/bin
```

### Manual build

```bash
cargo build --release
# Copy target/release/cc-sessions(.exe) wherever you like
```

## Usage

```bash
cc-sessions                              # Interactive picker (default)
cc-sessions --fork                       # Fork mode
cc-sessions --agent codex                # Codex sessions only
cc-sessions --agent all                  # Claude + Codex sessions
cc-sessions --project dotfiles           # Filter by project name
cc-sessions --status active              # Filter by status
cc-sessions --include-done               # Include done sessions in live view
cc-sessions --archive                    # Browse archived sessions
cc-sessions --trash                      # Browse trashed sessions
cc-sessions --debug                      # Show session ID prefixes and extra columns
cc-sessions --list                       # Non-interactive table
cc-sessions --list --count 30            # List 30 sessions
```

### Interactive mode (default)

- **Up/Down** -- navigate sessions
- **Enter** -- resume selected session (`claude -r` or `codex resume`)
- **Tab** -- cycle session status (none -> active -> paused -> done -> none)
- **d** -- mark selected live session done and hide it from the default live view
- **a** -- move selected live session to archive
- **x** -- move selected live session to trash
- **r** -- restore selected archived/trashed session to its original path
- **t** -- cycle sort mode (modified/created/turns/project/status)
- **Ctrl+S** -- full-text transcript search
- **Right** -- drill into fork children
- **Left** -- go back
- **PageUp/PageDown** -- scroll preview
- **Esc** -- clear search/focus, then exit
- **Ctrl+C** -- exit

### List mode (`--list`)

```
CREAT  MOD    ST AGENT  SOURCE   PROJECT          SUMMARY
----------------------------------------------------------------------------
1h     1h     *  claude local    dotfiles         Shell alias refactoring
2d     3h        codex  local    bike-power       Bike Power App: Build 10
```

### Status indicators

| Symbol | Status |
|--------|--------|
| `*` | Active |
| `~` | Paused |
| `v` | Done |

## How it works

Claude Code stores session data in `~/.claude/projects/`. Codex stores session data in `~/.codex/sessions/`. This tool:

1. Scans Claude and Codex `.jsonl` files
2. Extracts metadata in a single pass per file
3. Displays in a ratatui TUI with live preview
4. Resumes sessions via `claude -r <session-id>` or `codex resume <session-id>`

Archive and trash entries are moved under `~/.local/share/cc-sessions/` with a sidecar file that records the original path for restore.

Session status is stored in `~/.config/cc-sessions/metadata.json`.

## Credits

Based on [cc-sessions](https://github.com/chronologos/cc-sessions) by chronologos.

## License

MIT
