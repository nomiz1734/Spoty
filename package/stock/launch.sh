#!/bin/sh
# Spoty launcher for the TrimUI stock OS (Apps/Spoty).
#
# Spoty runs in a session of its own ("--serve" below) so the music keeps
# playing after it hands the screen back to the system menu (MENU > "Chay nen").
# The launcher's own run of this script only holds its place while Spoty is
# on screen: `spoty --attach` returns once Spoty leaves the screen or quits.

progdir=$(cd "$(dirname "$0")" && pwd)
cd "$progdir" || exit 1

export SPOTY_DIR="$progdir"
export LD_LIBRARY_PATH="/usr/trimui/lib:/usr/lib:$progdir/lib:${LD_LIBRARY_PATH:-}"
mkdir -p "$progdir/data"
CPU=/sys/devices/system/cpu/cpu0/cpufreq
PORT_FILE=/tmp/spoty.port
SERVE_PID=/tmp/spoty-serve.pid

# SD cards are FAT/exFAT and may not allow executing files: run from /tmp.
# Removing first works even while the old copy is still running.
install_bin() {
    rm -f /tmp/spoty
    cp "$progdir/spoty" /tmp/spoty && chmod +x /tmp/spoty
}

# Stops a player left over from before, and its restart loop first, so the
# loop's clean-up cannot remove the files of the new one.
stop_old() {
    pid=$(cat "$SERVE_PID" 2>/dev/null)
    if [ -n "$pid" ] && grep -q launch.sh "/proc/$pid/cmdline" 2>/dev/null; then
        kill "$pid" 2>/dev/null
    fi
    killall -9 spoty 2>/dev/null
    rm -f "$PORT_FILE" "$SERVE_PID"
}

if [ "${1:-}" = "--serve" ]; then
    # The player, detached from the launcher. Restarts it after an update and
    # rolls back an update that crashes before confirming it works.
    echo $$ > "$SERVE_PID"
    export SPOTY_DETACH=1
    PENDING="$progdir/data/update/pending"
    while true; do
        /tmp/spoty 2> "$progdir/data/launch.log"
        code=$?
        if [ "$code" -eq 42 ]; then
            # An update was installed: start over with the new launcher and binary.
            install_bin
            exec /bin/sh "$progdir/launch.sh" --serve
        fi
        if [ "$code" -ne 0 ] && [ -f "$PENDING" ] && [ -f "$progdir/spoty.old" ]; then
            # The new version crashed before confirming it works: go back to the old one.
            echo "$(date) update $(cat "$PENDING") failed (exit $code), rolled back" >> "$progdir/data/update.log"
            mv -f "$progdir/spoty.old" "$progdir/spoty"
            rm -f "$PENDING"
            install_bin
            continue
        fi
        break
    done
    rm -f /tmp/stay_awake /tmp/spoty "$PORT_FILE" "$SERVE_PID"
    if [ -n "${SPOTY_OLD_GOV:-}" ]; then
        echo "$SPOTY_OLD_GOV" > "$CPU/scaling_governor" 2>/dev/null
    fi
    exit 0
fi

# Spoty already playing in the background: bring it back on screen and wait
# until it leaves again. (Not right after an update from a version without
# background play: /tmp/spoty is then the old binary.)
if [ -z "${SPOTY_RESTARTED:-}" ] && [ -f "$PORT_FILE" ] && [ -x /tmp/spoty ]; then
    /tmp/spoty --attach
    case $? in
        0) exit 0 ;;
        2) stop_old ;; # running but not answering: start afresh
    esac
fi

# Only on the first start, not when re-launched after an update.
if [ -z "${SPOTY_RESTARTED:-}" ]; then
    stop_old
    # Let the kernel scale the CPU with load: fast when scrolling, low while just playing.
    SPOTY_OLD_GOV=$(cat "$CPU/scaling_governor" 2>/dev/null)
    export SPOTY_OLD_GOV
    if grep -q schedutil "$CPU/scaling_available_governors" 2>/dev/null; then
        echo schedutil > "$CPU/scaling_governor" 2>/dev/null
    elif grep -q ondemand "$CPU/scaling_available_governors" 2>/dev/null; then
        echo ondemand > "$CPU/scaling_governor" 2>/dev/null
    fi
    # Ask the system not to auto-sleep while music plays.
    echo 1 > /tmp/stay_awake
fi

install_bin
rm -f "$PORT_FILE"
if command -v setsid >/dev/null 2>&1; then
    setsid /bin/sh "$progdir/launch.sh" --serve </dev/null >/dev/null 2>&1 &
else
    /bin/sh "$progdir/launch.sh" --serve </dev/null >/dev/null 2>&1 &
fi
# Hold the launcher's place until Spoty leaves the screen or quits.
/tmp/spoty --attach --wait
exit 0
