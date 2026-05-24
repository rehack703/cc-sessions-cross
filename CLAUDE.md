# cc-sessions-cross

## Build & Test

```bash
just build    # Build release binary
just test     # Run tests
just install  # Build and install to ~/.local/bin
just lint     # Run clippy
```

## Architecture

```
src/
  main.rs                   # CLI, TUI (ratatui+crossterm), display, session resume
  session.rs                # Session domain model (Session, SessionAgent, SessionSource)
  claude_code.rs            # Claude Code JSONL loading/parsing, search index
  codex.rs                  # Codex JSONL loading/parsing, preview/search
  archive.rs                # Archive/trash/restore file movement
  message_classification.rs # User-message classification rules
  interactive_state.rs      # Pure reducer for interactive state transitions
  metadata.rs               # Session status persistence (Active/Paused/Done)
  remote.rs                 # Remote sync config + SSH/rsync operations
```

**Boundary principle:** If Claude Code changes its storage format, changes should be isolated to `claude_code.rs`. If Codex changes its storage format, changes should be isolated to `codex.rs`.

## Platform handling

- Path separators: `extract_project_name` handles both `/` and `\`
- Directory patterns: `strip_user_prefix` handles macOS (`-Users-`), Windows (`C--Users-`), Linux (`-home-`)
- Session resume: Claude uses `claude -r`; Codex uses `codex resume` / `codex fork`
- Key events: Windows emits Press+Release, filtered to Press-only in event loop
