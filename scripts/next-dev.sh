#!/bin/sh
# Build/install only the experimental target. Never build the production snens bin.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TARGET=/mnt/HC_Volume_106138989/cargo-target/snens-next
export CARGO_TARGET_DIR="$TARGET"
cd "$ROOT"

build() {
    cargo build --features surface-next --bin snens-next
    cargo build -p agent -p audit -p edit -p fetch -p read -p search -p view -p write
    # memory — отдельный репозиторий ~/projects/snens-memory (свой SQLite-плагин),
    # в workspace не входит; симлинк ставится вручную.
    for tool in agent audit edit fetch read search view write; do
        cp "$TARGET/debug/$tool" "tools/$tool/$tool.new"
        mv "tools/$tool/$tool.new" "tools/$tool/$tool"
    done
}
verify() {
    build
    cargo test --workspace --features surface-next
    cargo clippy --workspace --features surface-next -- -D warnings
    python3 scripts/surface-smoke.py
    sh tools/selftest.sh
}
install() {
    mkdir -p "$HOME/.local/bin"
    # Absolute path, independent of the terminal's current directory.
    python3 - "$ROOT" "$TARGET/debug/snens-next" <<'PY'
import pathlib, shlex, sys
root, binary = map(pathlib.Path, sys.argv[1:])
launcher = pathlib.Path.home() / '.local/bin/snens-next'
if launcher.is_symlink():
    launcher.unlink()
launcher.write_text('''#!/bin/sh
set -eu
if [ "$#" -eq 0 ]; then set -- tui; fi
case "$1" in
  serve)
    shift
    exec BINARY serve --config "$HOME/.config/snens-next/config.toml" "$@"
    ;;
  dev)
    shift
    exec SCRIPT "$@"
    ;;
  *) exec BINARY "$@" ;;
esac
'''.replace('BINARY', shlex.quote(str(binary))).replace('SCRIPT', shlex.quote(str(root / 'scripts/next-dev.sh'))))
launcher.chmod(0o755)
PY
    echo 'Installed snens-next launcher (snens-next dev build|verify|install).'
}
case "${1:-build}" in
  build) build ;;
  verify) verify ;;
  install) build; install ;;
  *) echo 'usage: scripts/next-dev.sh build|verify|install' >&2; exit 2 ;;
esac
