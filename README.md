# cc-sessions-cross

Cross-platform Claude Code session browser. Fork of [chronologos/cc-sessions](https://github.com/chronologos/cc-sessions) rebuilt with ratatui+crossterm for Windows/Linux/macOS support, plus new features.

## What's different from the original

- **Cross-platform TUI** -- replaced Unix-only `skim` with `ratatui` + `crossterm`
- **Session status** -- mark sessions as Active/Paused/Done (Tab to cycle, persisted to `metadata.json`)
- **Sort modes** -- press `t` to cycle through modified/created/turns/project/status
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
# Copy target/release/cc-sessions-cross(.exe) wherever you like
```

## Usage

```bash
cc-sessions-cross                        # Interactive picker (default)
cc-sessions-cross --fork                 # Fork mode
cc-sessions-cross --project dotfiles     # Filter by project name
cc-sessions-cross --status active        # Filter by status
cc-sessions-cross --debug                # Show session ID prefixes
cc-sessions-cross --list                 # Non-interactive table
cc-sessions-cross --list --count 30      # List 30 sessions
```

### Interactive mode (default)

- **Up/Down** -- navigate sessions
- **Enter** -- resume selected session
- **Tab** -- cycle session status (none -> active -> paused -> done -> none)
- **t** -- cycle sort mode (modified/created/turns/project/status)
- **Ctrl+S** -- full-text transcript search
- **Right** -- drill into fork children
- **Left** -- go back
- **PageUp/PageDown** -- scroll preview
- **Esc** -- clear search/focus, then exit
- **Ctrl+C** -- exit

### List mode (`--list`)

```
CREAT  MOD    ST SOURCE   PROJECT          SUMMARY
---------------------------------------------------------------------
1h     1h     *  local    dotfiles         Shell alias refactoring
2d     3h        local    bike-power       Bike Power App: Build 10
```

### Status indicators

| Symbol | Status |
|--------|--------|
| `*` | Active |
| `~` | Paused |
| `v` | Done |

## How it works

Claude Code stores session data in `~/.claude/projects/`. This tool:

1. Scans `.jsonl` files with valid UUID filenames (parallel via rayon)
2. Extracts metadata in a single pass per file (SIMD prefilter via memchr)
3. Displays in a ratatui TUI with live preview
4. Resumes sessions via `claude -r <session-id>`

Session status is stored in `~/.config/cc-sessions/metadata.json`.

## Credits

Based on [cc-sessions](https://github.com/chronologos/cc-sessions) by chronologos.

## License

MIT
