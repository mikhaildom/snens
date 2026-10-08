# Production rollout (core)

This is a staged, operator-run procedure. Preparing these assets does **not** install a binary, touch `~/.local/share/snens`, or switch a service. Do not treat preparation/backup as a completed rollout.

## Hard safety boundary

Production currently has the legacy TUI and its `run-child` process writing JSONL. Do not kill either, switch their session IDs to the server, or point the server at any session that may still be active. The new server's lock only protects its own surface store; it cannot lock out old binaries. The server may discover old JSONL sessions, but discovery is not permission to mutate/resume them. Telegram must start/use a **new** production session after core is ready; coordinate its deployment with the Telegram owner. Do not run the two pollers at once.

Rollout remains paused until the operator confirms all old production TUI/run-child writers and other writers have exited naturally. This session must not try to make that happen. If they cannot quiesce, stop here: install/start/switch is not safe. A service restart is not a substitute for quiescence.

## Backup stages

Use a private, dated backup location with enough free space; do not put it under the live instance. Keep the production instance's secret files private. First make the requested baseline copy now, while writers may still be active, and label it **non-quiescent baseline**. It is a useful recovery point, not a transactionally consistent backup of a live JSONL tail. Do not use it to roll back an active session.

After the operator has confirmed quiescence, make the final sync/backup of the complete instance before changing the service pointer. Verify the copy and retain both stages. At minimum preserve `sessions/` (JSONL canonical), `mail/`, `tickets/`, `soul.md`, `user.md`, `memory.md`, `skills/`, and any runtime files the service needs; preserve metadata/permissions. Do not convert JSONL to SQLite: SQLite is only the surface recovery adapter, not the canonical session log. Never restore an old copy over a newer live session log.

Example commands (run as the service user, after choosing a private destination; replace `BACKUP_ROOT`; choose the timestamp once and reuse it for both stages):

```sh
umask 077
BACKUP_ROOT="$HOME/snens-production-backups"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$BACKUP_ROOT/$STAMP/baseline" "$BACKUP_ROOT/$STAMP/final"
# Baseline: may race a writer; label it honestly.
rsync -a -- "$HOME/.local/share/snens/" "$BACKUP_ROOT/$STAMP/baseline/"
printf '%s\n' 'NON-QUIESCENT BASELINE; active JSONL tails may have changed during copy' > "$BACKUP_ROOT/$STAMP/baseline/NOTE"
# Stop here until an operator independently confirms every old writer exited.
# Then take final quiescent snapshot, before any production service switch:
rsync -a --delete -- "$HOME/.local/share/snens/" "$BACKUP_ROOT/$STAMP/final/"
```

Do not execute these examples as part of asset preparation. `--delete` applies only to the dedicated empty `final/` staging destination above; verify paths before running. Record the backup location and verify expected files/counts and that the copy is readable before proceeding. If the quiescent snapshot fails, do not deploy.

## Stage immutable core binary (after implementation/tests are accepted)

Build from this worktree using the feature/bin that owns the production server. Confirm the exact build invocation with the core implementer; do not accidentally install `snens-next` (the experimental binary). The target installed by the unit must be named `snens`.

Example once the production CLI is integrated (run from `~/projects/snens-next`; adjust only if the implementation's verified target requires it):

```sh
cargo build --release --bin snens --features surface-next
BIN=target/release/snens
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

This pointer swap does not itself restart a process. Preserve the old target/release and its hash for rollback. Do not run a service restart without explicit operator approval and a completed quiescent backup.

## Unit and activation

`deploy/snens.service` is the core-owned user unit. Install it only during the approved rollout, then explicitly reload the user manager and enable/start as a separate, approved operation. The unit uses separated argv (`--listen`, `127.0.0.1:7420`), the production config/data directory, restrictive `UMask=0077`, loopback binding, and a 120-second stop timeout. Keep the `~/.local/bin/snens` symlink stable across future upgrades and point it only at a verified immutable release.

The HTTP/SSE service is production-facing and has access to the user's tools/data. Keep it loopback-only; do not expose the listener directly. The server's own store lock prevents a second compatible server instance, not legacy TUI writers. Do not infer global JSONL locking from it.

## Rollback (manual; no automatic data rewind)

1. Stop the new user service under operator control and verify it is inactive. Do not kill old TUI/run-child processes as a rollback tactic.
2. Atomically repoint `~/.local/bin/snens` to the preserved old immutable release, verify its SHA-256, then start only the intended old service/entrypoint with the preserved config.
3. Preserve production data created by the new server. Never restore the baseline backup over live data. Restore the final backup only with the operator's explicit choice, after preserving the current tree separately and only if data loss is accepted.
4. Coordinate Telegram rollback separately; ensure there is only one poller. Keep the failed release and logs for diagnosis.

This is a manual recovery path, not a tested one-command rollback. Service/process state and data writes after cutover need operator judgment.

## Ownership / gate

Core owns this unit and document. The Telegram agent owns its deployment assets and production smoke; coordinate endpoint/token/session settings directly with that owner. Do not add or modify `scripts/production-smoke*` here. Before activation, require the core implementation and tests to be accepted, the artifact hash recorded, baseline and quiescent backups verified, old writers confirmed exited, and explicit operator authorization. Until all gates pass, production remains unchanged.
