#!/bin/bash
# Run this in the Moonlight/Konsole session to capture environment info
# Output goes to /tmp/moonlight-env.txt

OUT=/tmp/moonlight-env.txt

{
    echo "=== Moonlight Session Diagnostics ==="
    echo "Generated: $(date)"
    echo "Hostname: $(hostname)"
    echo "TTY: $(tty)"
    echo "Shell: $SHELL"
    echo ""

    echo "=== Display Environment ==="
    env | grep -E '(DISPLAY|WAYLAND|XDG|DBUS|KDE|QT)' | sort
    echo ""

    echo "=== Wayland Sockets ==="
    ls -la /run/user/$(id -u)/wayland* 2>/dev/null || echo "No wayland sockets found"
    echo ""

    echo "=== DBus Session ==="
    echo "DBUS_SESSION_BUS_ADDRESS: $DBUS_SESSION_BUS_ADDRESS"
    echo ""

    echo "=== Full Environment ==="
    env | sort

} > "$OUT"

echo "✅ Diagnostics written to $OUT"
echo "Run 'cat $OUT' from your other terminal to see results"
