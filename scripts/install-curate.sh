#!/bin/bash
# Installs the curate run (deck curate) as a launchd agent (Mondays at 9:00) and loads
# it. Run it again if deck or claude moves to another directory, or the plist changes.
# Removal: --uninstall.
set -euo pipefail

LABEL="io.github.jarilehtinen.deck-curate"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$REPO/scripts/$LABEL.plist"
DEST="$HOME/Library/LaunchAgents/$LABEL.plist"
DOMAIN="gui/$(id -u)"

# Remove the old version if it is loaded.
launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$DEST"
    echo "Removed: $DEST"
    exit 0
fi

# launchd's PATH is narrow, so the job's PATH is built from the directories where the
# commands are found now, and written into the plist.
JOB_PATH=""
for cmd in deck claude; do
    found="$(command -v "$cmd" || true)"
    case "$found" in
        /*) ;;
        *)
            echo "error: $cmd not found in PATH. Install it (see README) and run this again." >&2
            exit 1
            ;;
    esac
    dir="$(dirname "$found")"
    case ":$JOB_PATH:" in
        *":$dir:"*) ;;
        *) JOB_PATH="${JOB_PATH:+$JOB_PATH:}$dir" ;;
    esac
done
for dir in /usr/bin /bin /usr/sbin /sbin; do
    case ":$JOB_PATH:" in
        *":$dir:"*) ;;
        *) JOB_PATH="$JOB_PATH:$dir" ;;
    esac
done

# A value for the plist: first the XML characters (&, <, >) become entities, then \, &
# and the delimiter | are made literal for sed's replacement string.
escape() {
    printf '%s' "$1" \
        | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' \
        | sed -e 's/[\\&|]/\\&/g'
}

mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
sed -e "s|@DECK@|$(escape "$(command -v deck)")|g" \
    -e "s|@HOME@|$(escape "$HOME")|g" \
    -e "s|@PATH@|$(escape "$JOB_PATH")|g" \
    "$SRC" > "$DEST"
plutil -lint "$DEST" > /dev/null
launchctl bootstrap "$DOMAIN" "$DEST"

echo "Installed: $DEST"
echo "PATH for the job: $JOB_PATH"
if launchctl print "$DOMAIN/$LABEL" > /dev/null 2>&1; then
    echo "Loaded: $DOMAIN/$LABEL"
else
    echo "warning: launchd does not list $LABEL" >&2
fi
