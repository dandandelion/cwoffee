#!/usr/bin/env bash
# What the exec sandbox can actually reach over the network, in each mode.
#
#   ./scripts/egress-probe.sh            # proxy + off + open, real hosts
#   EGRESS_PROBE_OFFLINE=1 ./scripts/egress-probe.sh
#
# The offline form checks the parts that need no internet (the proxy's refusal of an
# unlisted host, the byte accounting, the environment variables). The full form
# installs two tiny npm packages inside the sandbox, which is the thing people
# actually want: `npm ci` working without the sandbox becoming a general route out.
#
# Everything runs through the engine's own exec tool, with the same ExecCfg and the
# same bwrap argument list the agent uses, so a line here is a line the model gets.
set -uo pipefail
cd "$(dirname "$0")/.."

WS=$(mktemp -d /tmp/cwoffee_egress_probe.XXXX)
trap 'rm -rf "$WS"' EXIT
BIN=./target/debug/examples/exec_probe
fails=0

say() { printf '%s\n' "$*"; }
check() { # name, expect_substring, actual
  if printf '%s' "$3" | grep -qF -- "$2"; then
    say "  ok   $1"
  else
    say "  FAIL $1: expected to contain [$2]"
    printf '%s\n' "$3" | sed 's/^/         /'
    fails=$((fails + 1))
  fi
}

if [ ! -x "$BIN" ]; then
  say "building exec_probe..."
  ./scripts/cargo-guard.sh --max-mb 2400 "${CARGO:-cargo}" build -p cwoffee-engine --example exec_probe >/dev/null || {
    say "FAIL could not build exec_probe"; exit 1;
  }
fi

say "== proxy mode: an unlisted host is refused by name, where the model reads it =="
out=$($BIN proxy "$WS" "curl -sS -m 10 https://example.com/ 2>&1 | head -2" 2>&1)
check "refusal names the host" "egress blocked: example.com" "$out"
check "refusal names the config line" 'allow = [...] under [sandbox]' "$out"
check "the command itself was not silently opened" "CONNECT tunnel failed" "$out"

if [ "${EGRESS_PROBE_OFFLINE:-0}" = "1" ]; then
  say "== offline form: stopping here =="
  [ $fails -eq 0 ] && say "egress-probe ok (offline form)" || say "egress-probe FAILED ($fails)"
  exit $fails
fi

say ""
say "== proxy mode: an allowlisted registry works, end to end =="
mkdir -p "$WS/pkg"
printf '{\n "name":"probe","version":"1.0.0",\n "dependencies":{"ms":"2.1.3","left-pad":"1.0.0"}\n}\n' > "$WS/pkg/package.json"
out=$($BIN proxy "$WS" "cd pkg && npm install --no-audit --no-fund --loglevel=warn 2>&1 | grep -E 'added|up to date|npm error' ; node -e 'console.log(require(\"ms\")(\"2 seconds\"))'" 2>&1)
check "npm install ran inside the sandbox" "added 2 packages" "$out"
check "the installed package resolves" "2000" "$out"

out=$($BIN proxy "$WS" "curl -sS -m 15 -o /dev/null -w 'registry %{http_code}\n' https://registry.npmjs.org/express" 2>&1)
check "registry over TLS through the tunnel" "registry 200" "$out"

say ""
say "== off mode: no route at all =="
out=$($BIN off "$WS" "curl -sS -m 10 -o /dev/null https://registry.npmjs.org/express 2>&1 | head -2" 2>&1)
check "off mode cannot reach the registry" "Could not connect to server" "$out"
check "and the tool says why" "no network access" "$out"

say ""
say "== open mode: the machine's own network =="
out=$($BIN open "$WS" "curl -sS -m 10 -o /dev/null -w 'open %{http_code}\n' https://example.com" 2>&1)
check "open mode reaches an unlisted host" "open 200" "$out"

say ""
[ $fails -eq 0 ] && say "egress-probe ok (proxy admits registries, refuses the rest by name, off has no route, open is open)" \
                 || say "egress-probe FAILED ($fails)"
exit $fails
