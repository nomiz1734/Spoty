#!/bin/sh
# Spoty launcher for the TrimUI stock OS (Apps/Spoty).

progdir=$(cd "$(dirname "$0")" && pwd)
cd "$progdir" || exit 1

export SPOTY_DIR="$progdir"
export LD_LIBRARY_PATH="/usr/trimui/lib:/usr/lib:$progdir/lib:${LD_LIBRARY_PATH:-}"
mkdir -p "$progdir/data"
CPU=/sys/devices/system/cpu/cpu0/cpufreq

# Only on the first start, not when re-launched after an update.
if [ -z "${SPOTY_RESTARTED:-}" ]; then
    killall spoty 2>/dev/null
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

PENDING="$progdir/data/update/pending"
while true; do
    # SD cards are FAT/exFAT and may not allow executing files: run from /tmp.
    cp -f "$progdir/spoty" /tmp/spoty && chmod +x /tmp/spoty
    /tmp/spoty 2> "$progdir/data/launch.log"
    code=$?
    if [ "$code" -eq 42 ]; then
        # An update was installed: start over with the new launcher and binary.
        export SPOTY_RESTARTED=1
        exec /bin/sh "$progdir/launch.sh"
    fi
    if [ "$code" -ne 0 ] && [ -f "$PENDING" ] && [ -f "$progdir/spoty.old" ]; then
        # The new version crashed before confirming it works: go back to the old one.
        echo "$(date) update $(cat "$PENDING") failed (exit $code), rolled back" >> "$progdir/data/update.log"
        mv -f "$progdir/spoty.old" "$progdir/spoty"
        rm -f "$PENDING"
        continue
    fi
    break
done

rm -f /tmp/stay_awake /tmp/spoty
if [ -n "${SPOTY_OLD_GOV:-}" ]; then
    echo "$SPOTY_OLD_GOV" > "$CPU/scaling_governor" 2>/dev/null
fi
