# Recipes cover the host workspace. The firmware has its own workspace and recipes later.

# `thumbv7em-none-eabihf` stands in for the ESP32-S3 until the esp toolchain is installed.
nostd_target := "thumbv7em-none-eabihf"

default: dev

# Local development loop: fix, check, build and test
dev: fix check build test

# Auto-fix formatting
fix:
    @echo '{{ style("command") }}fix:{{ NORMAL }}'
    cargo fmt --all

# Check formatting, types and lints, and check that the core and the ui build without std
check:
    @echo '{{ style("command") }}check:{{ NORMAL }}'
    cargo fmt --all --check
    cargo check --workspace --all-targets
    cargo clippy --workspace --all-targets -- --no-deps -Dclippy::pedantic -Dclippy::nursery -Dwarnings
    @just check-nostd

# Check that the core and the ui build for a no_std target.
# The firmware turns on `crux_core/critical-section`, so this check does the same.
check-nostd:
    @echo '{{ style("command") }}check-nostd:{{ NORMAL }}'
    cargo check -p shared -p ui --no-default-features --features crux_core/critical-section --target {{ nostd_target }}

# Build the workspace
build:
    @echo '{{ style("command") }}build:{{ NORMAL }}'
    cargo build --workspace

# Run the tests
test:
    @echo '{{ style("command") }}test:{{ NORMAL }}'
    cargo nextest run --workspace

# Run the desktop simulator, which writes panel.png
sim:
    @echo '{{ style("command") }}sim:{{ NORMAL }}'
    cargo run -p sim

# Report which tools are installed (does not fail)
[script('bash')]
doctor:
    echo '{{ style("command") }}doctor:{{ NORMAL }}'
    check() {
        if command -v "$1" >/dev/null 2>&1; then
            echo "  ✓ $1"
        else
            echo "  {{ style("error") }}✗ $1{{ NORMAL }}: install with: $2"
        fi
    }
    check cargo "https://rustup.rs"
    check cargo-nextest "cargo install cargo-nextest --locked"
    check espup "cargo install espup --locked"
    check espflash "cargo install espflash --locked"
