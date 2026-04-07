# cc-sessions-cross justfile

# Default recipe - show available commands
default:
    @just --list

# Build release binary
build:
    cargo build --release

# Run tests
test:
    cargo test

# Build and install to ~/.local/bin
install: build
    @if [ "$(uname)" = "Darwin" ]; then \
        cp target/release/cc-sessions ~/.local/bin/cc-sessions && \
        codesign -s - -f ~/.local/bin/cc-sessions; \
    elif [ "$(uname)" = "Linux" ]; then \
        cp target/release/cc-sessions ~/.local/bin/cc-sessions; \
    else \
        cp target/release/cc-sessions.exe ~/.local/bin/cc-sessions.exe; \
    fi

# Run with arguments (e.g., just run -- --list)
run *ARGS:
    cargo run --release -- {{ARGS}}

# Check code without building
check:
    cargo check

# Format code
fmt:
    cargo fmt

# Lint with clippy
lint:
    cargo clippy -- -D warnings

# Clean build artifacts
clean:
    cargo clean
