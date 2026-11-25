default: check

fmt:
    cargo fmt

lint:
    cargo clippy --all-targets -- -D warnings

test:
    cargo test

build:
    cargo build --release

# Replace ~/.cargo/bin/conn with the release build. `install` unlinks the old file first, so a
# running `conn serve` keeps its binary; restart it to run the new one (and get a new sign-in URL).
install: build
    install -m 755 target/release/conn ~/.cargo/bin/conn

# Empties target/ but keeps it a btrfs subvolume, so fs snapshots skip build output.
# `cargo clean` would delete the subvolume, and the next build would recreate a plain directory.
clean:
    #!/usr/bin/env bash
    set -euo pipefail
    # A btrfs subvolume root always has inode 256.
    if [[ -e target && "$(stat -c %i target)" == 256 ]]; then
        find target -mindepth 1 -delete
    else
        rm -rf target
        btrfs subvolume create target
    fi

# What "ready" means: formatted, lint-clean, tests green (tests/no_leaks.rs among them).
check:
    cargo fmt --check
    just lint
    just test
