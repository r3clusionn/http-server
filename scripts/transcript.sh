#!/usr/bin/env bash
# Starts tinyhttp on a scratch directory and records a real curl session against it in
# target/demo/session.txt, for the README screenshot. Needs curl.
set -uo pipefail
export MSYS_NO_PATHCONV=1
cd "$(dirname "$0")/.."
cargo build --release >/dev/null 2>&1
D=target/demo
rm -rf $D/www && mkdir -p $D/www/docs
printf 'hello from a file\n' > $D/www/hello.txt
printf '0123456789' > $D/www/ten.txt
printf '<h1>docs</h1>\n' > $D/www/docs/index.html
target/release/tinyhttp.exe $D/www --demo --port 18080 --quiet >/dev/null &
SERVER=$!
trap 'kill $SERVER 2>/dev/null || true' EXIT
sleep 1
B=http://127.0.0.1:18080
{
  run() { echo "\$ $*"; "$@" 2>&1 | tr -d '\r'; echo; }
  run curl -si $B/hello.txt
  run curl -s -r 2-5 $B/ten.txt
  echo "$ curl -s -o /dev/null -w '%{http_code}' -r 50- $B/ten.txt   (a range past the end)"; curl -s -o /dev/null -w '%{http_code}' -r 50- $B/ten.txt; echo; echo
  echo "\$ curl -s -T - -X POST -H 'Transfer-Encoding: chunked' $B/_/echo"
  printf 'sent in chunks' | curl -s -T - -X POST -H 'Transfer-Encoding: chunked' $B/_/echo; echo; echo
  run curl -si -X DELETE $B/_/echo
  echo "\$ curl -s --path-as-is -o /dev/null -w '%{http_code}\n' $B/../Cargo.toml"
  curl -s --path-as-is -o /dev/null -w '%{http_code}\n' "$B/../Cargo.toml"; echo
  echo "\$ curl -s $B/_/hello/a $B/_/hello/b (over one connection)"
  curl -s $B/_/hello/a $B/_/hello/b
} > $D/session.txt
cat $D/session.txt
