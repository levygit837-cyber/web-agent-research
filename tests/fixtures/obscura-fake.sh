#!/bin/sh
# Hermetic double for `obscura fetch`.
# Dispatches on $2 (the positional URL); flags after it are ignored.
# Call shape: obscura-fake.sh fetch <url> --dump markdown --quiet
url="$2"
case "$url" in
  *slow*)
    exec sleep 5
    ;;
  *errflood*)
    # Endless stderr: the 5 MiB cap must kill it long before the timeout.
    exec yes 'stderr noise line' >&2
    ;;
  *flood*)
    # Endless stdout: same, for the markdown stream.
    exec yes 'markdown flood line'
    ;;
  *blocked*)
    printf 'access forbidden (bot denied)' >&2
    exit 3
    ;;
  *interstitial*)
    printf '# stackoverflow.com\n## Performing security verification\nThis website uses a security service to protect against malicious bots.\n## Verification successful. Waiting for stackoverflow.com to respond\nRay ID: `a45f0daa58bc0675`\nPerformance and Security by [Cloudflare](https://www.cloudflare.com)\n'
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
  *noisy*)
    printf '# Title\n\n![inline chart](data:image/png;base64,AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA)\n\n[reference](https://example.org/ref?utm_source=newsletter&id=3)\n'
    ;;
  *)
    printf '# Title\n\nbody for %s' "$url"
    ;;
esac
