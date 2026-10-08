# Production rollout (core)

This is a staged, operator-run procedure. Preparing these assets does **not** install a binary, touch `~/.local/share/snens`, or switch a service. Do not treat preparation/backup as a completed rollout.

## Legacy-session safety boundary

The production legacy TUI and its `run-child` may still be writing JSONL. Never kill them, resume their session IDs through the API TUI, or let Telegram select an existing/active legacy session. The server lock protects only its own surface store, not legacy JSONL. A new API/Telegram session has a distinct JSONL path, but discovery of legacy sessions does not prove they are idle.

A staged core install/start and Telegram start on a **new** session are possible only while every active legacy session ID is excluded by an external per-file guard held until its old writer exits. `scripts/legacy-writer-guard.py` supplies a temporary Linux per-file flock guard; its isolated tests verify compatibility with the Rust/fs2 writer lock. Run it independently of the legacy parent (for example via a transient user service), wait for its `lock-acquired` readiness output, and verify it remains active before starting the API. The guard releases locks only when the recorded legacy PIDs exit or change identity. It does not stop old writers from appending and is not a sandbox; do not infer this protection from the server lock. Without that guard, defer activation until old TUI/run-child writers and other writers exit naturally. Never switch or resume legacy IDs. Do not run two Telegram pollers for the same token.

## Backup stages

Use a private, dated backup location with enough free space; do not put it under the live instance. Keep the production instance's secret files private. Before any production changes, make a baseline copy and label it **non-quiescent baseline** if writers may still be active. It is a useful recovery point, not a transactionally consistent backup of a live JSONL tail. Do not use it to roll back an active session.

If the rollout may touch any legacy session, wait for writer quiescence and make a final sync/backup of the complete instance before changing the service pointer. Verify it and retain it alongside the baseline. If using the staged path with a verified external per-file guard excluding every active legacy ID, the baseline is the recovery snapshot for new, distinct sessions; do not claim or rely on a final quiescent snapshot. Preserve `sessions/` (JSONL canonical), `mail/`, `tickets/`, `soul.md`, `user.md`, `memory.md`, `skills/`, and runtime files; preserve metadata/permissions. Do not convert JSONL to SQLite: it is only the surface recovery adapter, not the canonical session log. Never restore an old copy over a newer live session log.

Example commands (run as the service user, after choosing a private destination; replace `BACKUP_ROOT`; choose the timestamp once and reuse it for both stages):

```sh
umask 077
BACKUP_ROOT="$HOME/snens-production-backups"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$BACKUP_ROOT/$STAMP/baseline" "$BACKUP_ROOT/$STAMP/final"
# Baseline: may race a writer; label it honestly.
rsync -a -- "$HOME/.local/share/snens/" "$BACKUP_ROOT/$STAMP/baseline/"
printf '%s\n' 'NON-QUIESCENT BASELINE; active JSONL tails may have changed during copy' > "$BACKUP_ROOT/$STAMP/baseline/NOTE"
# If any legacy session will be mutated, stop here until its old writer exits.
# Otherwise require the verified external per-file guard described above; do not treat
# a service start or the server lock as that guard. Take a final quiescent snapshot if required:
rsync -a --delete -- "$HOME/.local/share/snens/" "$BACKUP_ROOT/$STAMP/final/"
```

These are examples, not commands to run during documentation preparation. `--delete` applies only to the dedicated empty `final/` staging destination above; verify paths before running. Record the backup location and verify expected files/counts and that the copy is readable before proceeding. If a quiescent snapshot is required but cannot be taken, do not deploy.

## Stage immutable core binary

The production CLI is `snens` (separate from experimental `snens-next`): it provides `serve --config PATH --listen 127.0.0.1:7420`, API `tui` (default endpoint `7420`, token `prodtoken`), and `tui-local` fallback. The production entry point is the default `snens` binary; the optional `surface-next` feature gates only the experimental `snens-next` binary. Build only `snens`, without that feature. Cargo's configured target directory is relocated, so resolve it from cargo metadata rather than assuming `target/` under the checkout.

```sh
cd ~/projects/snens-next
cargo build --release --bin snens
TARGET_DIR=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin).get("target_directory"))')
BIN="$TARGET_DIR/release/snens"
test -x "$BIN"
HASH="$(sha256sum "$BIN" | cut -d' ' -f1)"
RELEASE="$HOME/.local/lib/snens/releases/$HASH"
install -d -m 0755 "$RELEASE"
install -m 0555 "$BIN" "$RELEASE/snens"
test "$(sha256sum "$RELEASE/snens" | cut -d' ' -f1)" = "$HASH"
printf 'release=%s\nsha256=%s\n' "$RELEASE" "$HASH"
```

The exact SHA-256 printed above is the artifact identity to record in the rollout log; do not invent or reuse a hash from another build. Make the release directory read-only after installing, and never overwrite a release path. Do not replace a running binary in place. Before changing the launcher, inspect its current target: preserve the old executable as its own immutable release (copy if it is a regular file, retain its symlink target otherwise), record its SHA-256, and verify the preserved copy. If this cannot be done, stop; rollback is not prepared. After the operator's explicit approval of the verified artifact, atomically repoint the launcher:

```sh
chmod 0555 "$RELEASE"
ln -s "$RELEASE/snens" "$HOME/.local/bin/snens.new"
mv -Tf "$HOME/.local/bin/snens.new" "$HOME/.local/bin/snens"
```

This pointer swap does not itself restart a process. Preserve the old target/release and its hash for rollback. Do not run a service restart without explicit operator approval and the applicable backup: a final quiescent snapshot if shared legacy sessions will be touched, otherwise a verified baseline plus the legacy-session guard described above.

## Unit and activation

`deploy/snens.service` is the core-owned user unit. Install it during the authorized rollout after its safety gates, then explicitly reload the user manager and enable/start in the planned sequence. The unit uses separated argv (`--listen`, `127.0.0.1:7420`), the production config/data directory, restrictive `UMask=0077`, loopback binding, and a 120-second stop timeout. Keep the `~/.local/bin/snens` symlink stable across future upgrades and point it only at a verified immutable release.

The HTTP/SSE service is production-facing and has access to the user's tools/data. Keep it loopback-only; do not expose the listener directly. The server's own store lock prevents a second compatible server instance, not legacy TUI writers. Do not infer global JSONL locking from it.

## Rollback (manual; no automatic data rewind)

1. Stop the new user service under operator control and verify it is inactive. Do not kill old TUI/run-child processes as a rollback tactic.
2. Atomically repoint `~/.local/bin/snens` to the preserved old immutable release, verify its SHA-256, then start only the intended old service/entrypoint with the preserved config.
3. Preserve production data created by the new server. Never restore the baseline backup over live data. Restore a backup only with the operator's explicit choice, after preserving the current tree separately and only if data loss is accepted.
4. Coordinate Telegram rollback separately; ensure there is only one poller. Keep the failed release and logs for diagnosis.

This is a manual recovery path, not a tested one-command rollback. Service/process state and data writes after cutover need operator judgment.

## Ownership / gate

Core owns this unit and document. The Telegram agent owns its deployment assets and production smoke; coordinate endpoint/token/session settings directly with that owner. Do not add or modify `scripts/production-smoke*` here. Before activation, require the core implementation and tests to be accepted, artifact hash recorded, and a verified baseline. If any legacy session could be mutated, also require a verified final quiescent backup and confirmation that old writers exited; alternatively, an independently verified external per-file guard must exclude active legacy IDs until those writers exit. Explicit rollout authorization is already granted for this rollout; do not imply another per-service approval is needed. Do not activate without one of those legacy-session safety conditions.
