#!/usr/bin/env bash
# Acceptance test: one real turn per installed harness, in cloud mode
# (`rayline run <harness>`) and in local mode (`rayline run <harness> --local`).
# Usage: scripts/acceptance.sh [HARNESS...]   (default: every harness)
# Local mode uses ~/.config/rayline/settings.json, which must have a `routing`
# section. The script writes no config. It starts the local process with
# `rayline local start` and stops it with `rayline local stop` when done.
# See docs/acceptance-testing.md.
set -uo pipefail

RAYLINE=${RAYLINE:-rayline}
HARNESSES=(claude claude-desktop codex opencode pi omp hermes openclaw)
PROMPT="Reply with the word ACCEPT immediately followed by the sum of 17 and 25, with no space and nothing else."
ANSWER=ACCEPT42
TIMEOUT=600
DESKTOP_APP=/Applications/Claude.app

usage() {
  echo "usage: $0 [HARNESS...]" >&2
  echo "harnesses: ${HARNESSES[*]}" >&2
  exit 2
}

# Harness args for one non-interactive turn; the prompt is appended.
turn_args() {
  case $1 in
    claude | pi | omp) echo "-p" ;;
    codex) echo "exec --skip-git-repo-check" ;;
    opencode) echo "run" ;;
    hermes) echo "chat -Q -q" ;;
    openclaw) echo "agent -m" ;;
  esac
}

installed() {
  if [ "$1" = claude-desktop ]; then
    [ -d "$DESKTOP_APP" ]
  else
    command -v "$1" >/dev/null 2>&1
  fi
}

version() {
  if [ "$1" = claude-desktop ]; then
    defaults read "$DESKTOP_APP/Contents/Info" CFBundleShortVersionString 2>&1
  else
    "$1" --version 2>&1 | head -n 1
  fi
}

pass=0
fail=0
skip=0
manual=0

# with_timeout <secs> <cmd...>: runs cmd in its own process group and kills
# the whole group after <secs> or on Ctrl-C (TERM, then KILL after up to 10s),
# so no harness child outlives the turn.
with_timeout() {
  perl -e '
    use POSIX ":sys_wait_h";
    my $secs = shift;
    my $pid = fork // die "fork: $!\n";
    if (!$pid) { setpgrp(0, 0); exec @ARGV or die "exec: $!\n" }
    setpgrp($pid, $pid);
    sub stop_group {
      my ($code, $message) = @_;
      kill "TERM", -$pid;
      for (1 .. 10) { waitpid($pid, WNOHANG); last unless kill 0, -$pid; sleep 1 }
      kill "KILL", -$pid;
      waitpid($pid, 0);
      print STDERR $message if $message;
      exit $code;
    }
    $SIG{INT} = $SIG{TERM} = sub { stop_group(130) };
    $SIG{ALRM} = sub { stop_group(124, "timed out after ${secs}s\n") };
    alarm $secs;
    waitpid($pid, 0);
    exit($? & 127 ? 128 + ($? & 127) : $? >> 8);
  ' "$@"
}

# run_turn <harness> <mode> [rayline run options...]
run_turn() {
  local harness=$1 mode=$2 out status
  shift 2
  # shellcheck disable=SC2046 # turn_args is split into words on purpose
  out=$(with_timeout "$TIMEOUT" \
    "$RAYLINE" run "$harness" "$@" -- $(turn_args "$harness") "$PROMPT" </dev/null 2>&1)
  status=$?
  if [ "$status" -eq 0 ] && grep -q "$ANSWER" <<<"$out"; then
    echo "PASS    $harness $mode"
    pass=$((pass + 1))
  else
    echo "FAIL    $harness $mode (exit $status)"
    tail -n 15 <<<"$out" | sed 's/^/        /'
    fail=$((fail + 1))
  fi
}

# run_mode <mode> [rayline run options...]
run_mode() {
  local mode=$1 harness
  shift
  for harness in "${selected[@]}"; do
    if ! installed "$harness"; then
      echo "SKIP    $harness $mode (not installed)"
      skip=$((skip + 1))
    elif [ "$harness" = claude-desktop ]; then
      echo "MANUAL  claude-desktop $mode: run \`$RAYLINE run claude-desktop${*:+ $*}\` and send one turn in Code (docs/acceptance-testing.md)"
      manual=$((manual + 1))
    else
      run_turn "$harness" "$mode" "$@"
    fi
  done
}

selected=("$@")
[ ${#selected[@]} -gt 0 ] || selected=("${HARNESSES[@]}")
for harness in "${selected[@]}"; do
  known=false
  for name in "${HARNESSES[@]}"; do
    [ "$harness" = "$name" ] && known=true
  done
  $known || { echo "unknown harness: $harness" >&2; usage; }
done

echo "== versions"
"$RAYLINE" --version || exit 1
for harness in "${selected[@]}"; do
  if installed "$harness"; then
    echo "$harness: $(version "$harness")"
  else
    echo "$harness: not installed"
  fi
done

echo "== cloud mode"
run_mode cloud

echo "== local mode"
if "$RAYLINE" local start; then
  trap '"$RAYLINE" local stop; exit 130' INT TERM
  "$RAYLINE" local status
  run_mode local --local
  trap - INT TERM
  if ! "$RAYLINE" local stop; then
    echo "FAIL    rayline local stop"
    fail=$((fail + 1))
  fi
else
  echo "FAIL    rayline local start"
  fail=$((fail + 1))
fi

echo "== pass=$pass fail=$fail skip=$skip manual=$manual"
[ "$fail" -eq 0 ]
