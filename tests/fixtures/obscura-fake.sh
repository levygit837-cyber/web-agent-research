#!/bin/sh
# Hermetic double for `obscura fetch`.
# Dispatches on $2 (the positional URL); flags after it are ignored.
# Call shape: obscura-fake.sh fetch <url> --dump markdown --quiet
url="$2"
case "$url" in
  *slow*)
    exec sleep 5
    ;;
  *blocked*)
    printf 'access forbidden (bot denied)' >&2
    exit 3
    ;;
  *empty*)
    exit 0
    ;;
  *fail*)
    printf 'boom: something broke' >&2
    exit 2
    ;;
  *robots*)
    printf 'respecting robots.txt for /slow' >&2
    printf '# Title\n\nbody for %s' "$url"
    ;;
  *bignum*)
    printf 'count 14034 items' >&2
    printf '# Title\n\nbody for %s' "$url"
    ;;
  *pad*)
    printf '\n\n# Title\n\nbody\n\n   \n'
    ;;
  *)
    printf '# Title\n\nbody for %s' "$url"
    ;;
esac
