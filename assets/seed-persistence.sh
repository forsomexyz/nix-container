set -e
if [ -e "$1" ] && [ ! -d "$1" ]; then
    echo "persistence location exists in the image but is not a directory: $1" >&2
    exit 1
fi
if [ -d "$1" ] && [ -z "$(ls -A /seed-persistence)" ]; then
    cp -a "$1"/. /seed-persistence/
fi
chown -R "$2" /seed-persistence
got=$(stat -c %u:%g /seed-persistence)
if [ "$got" != "$2" ]; then
    echo "persistence volume stayed owned by $got instead of $2" >&2
    exit 1
fi
