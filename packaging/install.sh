#!/bin/sh
# Install + start the hearth OpenRC service, retiring the old ambient-st-bridge one.
# Run as root, AFTER the folder has been renamed to .../hearth:
#   sudo sh /datar/workspace/claude_code_experiments/hearth/packaging/install.sh
set -eu

PROJECT="/datar/workspace/claude_code_experiments/hearth"
SRC="$PROJECT/packaging/hearth.openrc"
DEST="/etc/init.d/hearth"
BIN="$PROJECT/target/release/hearth"
LOG="$PROJECT/hearth.log"

if [ "$(id -u)" -ne 0 ]; then
  echo "ERROR: run as root, e.g.  sudo sh $0" >&2
  exit 1
fi
[ -f "$SRC" ] || { echo "ERROR: service script missing: $SRC (did you rename the folder to hearth?)" >&2; exit 1; }
[ -x "$BIN" ] || { echo "ERROR: release binary missing (run 'cargo build --release'): $BIN" >&2; exit 1; }

# Retire the old ambient-st-bridge service if it's still around.
if [ -f /etc/init.d/ambient-st-bridge ]; then
  echo ">> retiring old ambient-st-bridge service"
  rc-service ambient-st-bridge stop 2>/dev/null || true
  rc-update del ambient-st-bridge default 2>/dev/null || true
  rm -f /etc/init.d/ambient-st-bridge
fi

echo ">> installing $DEST"
install -m 0755 "$SRC" "$DEST"
echo ">> enabling at boot"
rc-update add hearth default 2>/dev/null || true
echo ">> starting"
if rc-service hearth status >/dev/null 2>&1; then
  rc-service hearth restart || echo "WARN: restart error (see status/log below)"
else
  rc-service hearth start || echo "WARN: start error (see status/log below)"
fi

echo
echo ">> status:"; rc-service hearth status || true
echo
echo ">> first poll (waiting a few seconds)..."; sleep 6
[ -f "$LOG" ] && tail -n 15 "$LOG" || echo "(no log yet — try: tail -f $LOG)"
echo
echo ">> Done. Healthy = 'smartthings publish sent=10' every ~60s.   Watch: tail -f $LOG"
