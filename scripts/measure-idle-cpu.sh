#!/usr/bin/env bash
#
# Measure a GPUI binary's *idle* CPU, in percent of one core.
#
# Why this exists
# ---------------
# `ps -o pcpu` reports CPU time over the process's whole **lifetime**, so an app
# that spends 20 seconds compiling shaders, loading assets and laying out a few
# hundred rows looks like it is burning a core forever. That number misled us
# once already: it read 91% for an app that was mostly still starting up.
#
# This samples `utime + stime` from `/proc/<pid>/stat` across a window, **after**
# a settle period, so it measures what the process is doing *now*. That is the
# number worth acting on.
#
# Usage:
#   scripts/measure-idle-cpu.sh <binary> [settle_secs] [window_secs]
#
# Example:
#   scripts/measure-idle-cpu.sh target/debug/btop-gpui     # 76% — suspicious
#   scripts/measure-idle-cpu.sh target/release/btop-gpui   # the honest compare
#
# The window must be long enough to span several 2 s ticks of the collector, or
# the sample lands entirely inside a quiet gap and reads ~0.

set -euo pipefail

BIN=${1:?usage: measure-idle-cpu.sh <binary> [settle_secs] [window_secs]}
SETTLE=${2:-20}
WINDOW=${3:-10}

if [ ! -x "$BIN" ]; then
    echo "not executable: $BIN" >&2
    exit 1
fi

# Resolve to an absolute path before matching on it. The app is launched by
# absolute path, so its argv is `^<abs path>$` exactly — and this script's own
# argv contains that same path as an argument. An unanchored `pkill -f "$BIN"`
# therefore matches and kills *this script*. The anchors are load-bearing.
BIN=$(realpath "$BIN")

# Clear any previous instance so the pid we sample is the one we started.
pkill -f "^${BIN}$" 2>/dev/null || true
sleep 1

setsid "$BIN" >/dev/null 2>&1 &

PID=""
for _ in $(seq 1 60); do
    PID=$(pgrep -f "^${BIN}$" | head -1 || true)
    [ -n "$PID" ] && break
    sleep 0.1
done

if [ -z "$PID" ]; then
    echo "app did not start: $BIN" >&2
    exit 1
fi

echo "pid $PID · binary $BIN"
echo "settling ${SETTLE}s (shader compilation and asset loading live here)…"
sleep "$SETTLE"

HZ=$(getconf CLK_TCK)

# Fields 14 and 15 of /proc/<pid>/stat are utime and stime, in clock ticks.
# Read on every call rather than cached: a clean exit mid-window means the
# second read fails, and that should surface as an error, not as 0% CPU.
ticks() { awk '{print $14 + $15}' "/proc/$1/stat"; }

T0=$(ticks "$PID")
sleep "$WINDOW"
T1=$(ticks "$PID")

CPU=$(awk -v a="$T0" -v b="$T1" -v hz="$HZ" -v w="$WINDOW" \
      'BEGIN { printf "%.1f", (b - a) * 100 / (hz * w) }')
RSS=$(awk '/VmRSS/ {print $2 " " $3}' "/proc/$PID/status" 2>/dev/null || echo "n/a")

echo
echo "idle CPU: ${CPU}% of one core   (averaged over ${WINDOW}s)"
echo "RSS:      ${RSS}"

pkill -f "^${BIN}$" 2>/dev/null || true
