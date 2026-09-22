#!/bin/sh
# Собрать тулы в release и положить бинарники туда, откуда их берёт реестр:
# tools/<имя>/<имя>. Путь к target берём из самого cargo — он может быть перенесён.
set -eu
cd "$(dirname "$0")/.."

for dir in tools/*/; do
  name=$(basename "$dir")
  [ -f "$dir/Cargo.toml" ] || continue
  bin=$(cargo build --release -p "$name" --message-format=json 2>/dev/null \
        | sed -n 's/.*"executable":"\([^"]\{1,\}\)".*/\1/p' | head -1)
  if [ -z "$bin" ]; then
    echo "не нашёл бинарник для $name" >&2
    exit 1
  fi
  ln -sf "$bin" "$dir$name"
  echo "$name → $bin"
done