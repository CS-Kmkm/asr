#!/usr/bin/env bash
set -eu
export LC_ALL=C.UTF-8
/usr/bin/stty raw -echo
printf 'ASR evaluation receiver\r\n'
exec /usr/bin/timeout 120 /usr/bin/cat > "$1"
