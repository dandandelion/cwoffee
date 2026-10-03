#!/usr/bin/env bash
# Prove the exec sandbox actually isolates, instead of trusting the flags.
#
# The bwrap argument list in crates/engine/src/tools/exec.rs was derived from
# this probe. Re-run it after any change to sandbox_args(), and after any
# bubblewrap / kernel / distro upgrade — user-namespace policy changes silently.
#
#   ./scripts/sandbox-probe.sh
#
# Every check prints PASS/FAIL; exit code is non-zero if any check fails.

set -uo pipefail

WS="$(mktemp -d /tmp/cwoffee_probe_ws.XXXXXX)"
trap 'rm -rf "$WS"' EXIT

fails=0
pass() { echo "  PASS  $1"; }
fail() { echo "  FAIL  $1"; fails=$((fails+1)); }

if ! command -v bwrap >/dev/null 2>&1; then
  echo "bwrap not installed"; exit 2
fi
echo "bubblewrap: $(bwrap --version)"
echo "user namespaces: $(cat /proc/sys/kernel/unprivileged_userns_clone 2>/dev/null || echo '(not exposed)')"
echo

# Exactly the shape sandbox_args() produces, plus the inner coreutils timeout.
run() {
  bwrap \
    --ro-bind / / \
    --dev /dev \
    --proc /proc \
    --tmpfs /tmp \
    --bind "$WS" "$WS" \
    --unshare-user --unshare-ipc --unshare-pid --unshare-net \
    --die-with-parent \
    --setenv HOME "$WS" \
    -- /usr/bin/timeout -k 2 15 /bin/sh -c "$1"
}

echo "1. network is blocked"
out=$(run 'curl -s -m 5 -o /dev/null http://example.com 2>/dev/null && echo REACHABLE || echo BLOCKED' 2>&1)
case "$out" in *BLOCKED*) pass "no network from inside the sandbox";; *) fail "network reachable: $out";; esac

echo "2. host filesystem is read-only"
out=$(run 'touch /cwoffee_escape 2>/dev/null && echo WROTE || echo BLOCKED' 2>&1)
case "$out" in *BLOCKED*) pass "cannot write to /";; *) fail "wrote to /"; rm -f /cwoffee_escape;; esac

echo "3. the real host home is not writable, and HOME is redirected"
real_home="$HOME"
out=$(run "touch $real_home/.cwoffee_home_escape 2>/dev/null && echo WROTE || echo BLOCKED" 2>&1)
case "$out" in
  *BLOCKED*) pass "cannot write to the real host home ($real_home)";;
  *) fail "wrote to the real host home"; rm -f "$real_home/.cwoffee_home_escape";;
esac
inner_home=$(run 'printf %s "$HOME"' 2>/dev/null)
if [ "$inner_home" = "$WS" ]; then
  pass "HOME inside the sandbox is the workspace, so ~ writes are confined"
else
  fail "HOME inside is ${inner_home:-unset}, expected $WS"
fi

echo "4. workspace is writable and lands on the host"
run 'echo hi > '"$WS"'/probe.txt' >/dev/null 2>&1
if [ "$(cat "$WS/probe.txt" 2>/dev/null)" = "hi" ]; then
  pass "agent writes are visible to the user"
else
  fail "workspace bind is not working (check --tmpfs ordering: tmpfs must precede any bind under /tmp)"
fi

echo "5. pid namespace is isolated"
n=$(run 'ps -e 2>/dev/null | wc -l' 2>/dev/null | tr -d '[:space:]')
if [ -n "$n" ] && [ "$n" -lt 50 ]; then
  pass "sees only $n processes (host has $(ps -e | wc -l))"
else
  fail "pid namespace not isolated (saw ${n:-?})"
fi

echo "6. uid is preserved so files stay user-owned"
uid=$(run 'id -u' 2>/dev/null | tr -d '[:space:]')
if [ "$uid" = "$(id -u)" ]; then
  pass "uid $uid (files the agent writes are owned by you, not root)"
else
  fail "uid changed to $uid"
fi

echo "7. deadline is enforced inside the namespace"
start=$(date +%s)
run 'sleep 300' >/dev/null 2>&1
elapsed=$(( $(date +%s) - start ))
if [ "$elapsed" -lt 20 ]; then
  pass "killed after ${elapsed}s (inner coreutils timeout, not just killing bwrap)"
else
  fail "took ${elapsed}s - the sandboxed process outlived its deadline"
fi

echo "8. a process cannot see the host process table via /proc"
out=$(run 'grep -c . /proc/1/cmdline 2>/dev/null; cat /proc/1/comm 2>/dev/null' 2>&1)
if echo "$out" | grep -qiE 'bwrap|timeout|sh'; then
  pass "PID 1 inside is the sandbox's own, not the host init"
else
  fail "unexpected PID 1: $out"
fi

echo
if [ "$fails" -eq 0 ]; then
  echo "sandbox: all checks passed"
else
  echo "sandbox: $fails check(s) FAILED - do not enable exec until fixed"
  exit 1
fi
