# Recipes cover the host workspace. The firmware has its own workspace and recipes later.

# The no_std check targets the ESP32-S3 with the `esp` toolchain.
# If that toolchain is missing, the check uses `thumbv7em-none-eabihf` instead.
esp_target := "xtensa-esp32s3-none-elf"
fallback_target := "thumbv7em-none-eabihf"

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
# The check needs the `esp` toolchain and `~/export-esp.sh`. Without the toolchain, it uses the fallback target.
[script('bash')]
check-nostd:
    echo '{{ style("command") }}check-nostd:{{ NORMAL }}'
    set -e
    flags="-p shared -p ui --no-default-features --features crux_core/critical-section"
    if rustup toolchain list | grep -q '^esp'; then
        if [ -f "$HOME/export-esp.sh" ]; then
            . "$HOME/export-esp.sh"
        fi
        cargo +esp check $flags --target {{ esp_target }} -Zbuild-std=core,alloc
    else
        echo "note: the esp toolchain is not installed, so this check uses {{ fallback_target }}"
        cargo check $flags --target {{ fallback_target }}
    fi

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
    if rustup toolchain list | grep -q '^esp'; then
        echo "  ✓ esp toolchain"
    else
        echo "  {{ style("error") }}✗ esp toolchain{{ NORMAL }}: install with: espup install"
    fi
    check espflash "cargo install espflash --locked"
