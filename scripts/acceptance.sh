#!/usr/bin/env bash
# Acceptance test: one real turn per harness, in cloud mode
# (`rayline run <harness>`) and in local mode (`rayline run <harness> --local`).
# Usage: [CONFIG=<file>] scripts/acceptance.sh [HARNESS...]
#   (default: every harness; a harness that isn't installed fails)
# Local mode uses ~/.config/rayline/settings.json, or <file> when CONFIG is
# set; either must have a `routing` section. With CONFIG, local mode runs
# `rayline local start --config <file>` and `rayline run <harness> --config
# <file>`; cloud mode always uses the default file. The script writes no
# config. If a local process is already running, or `rayline local status`
# fails, it exits 1 before any turn.
# It starts the local process with `rayline local start` and runs
# `rayline local stop` once on exit, including on Ctrl-C, TERM and HUP. If
# `rayline local start` fails, it stops nothing: a process it didn't start is
# left alone.
# See docs/acceptance-testing.md.
set -uo pipefail

RAYLINE=${RAYLINE:-rayline}
CONFIG=${CONFIG:-}
HARNESSES=(claude codex opencode pi omp hermes openclaw)
PROMPT="Reply with the word ACCEPT immediately followed by the sum of 17 and 25, with no space and nothing else."
ANSWER=ACCEPT42
TIMEOUT=600

usage() {
  echo "usage: [CONFIG=<file>] $0 [HARNESS...]" >&2
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
  command -v "$1" >/dev/null 2>&1
}

pass=0
fail=0

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
    if installed "$harness"; then
      run_turn "$harness" "$mode" "$@"
    else
      echo "FAIL    $harness $mode (not installed)"
      fail=$((fail + 1))
    fi
  done
}

# stop_local: runs `rayline local stop` the first time it is called; a failed
# stop is a FAIL.
stopped=false
stop_local() {
  $stopped && return
  stopped=true
  if ! "$RAYLINE" local stop; then
    echo "FAIL    rayline local stop"
    fail=$((fail + 1))
  fi
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

# `rayline run --config <file>` is in local mode only when <file> has a
# `routing` section, so a CONFIG without one would run no local turn.
local_start=("$RAYLINE" local start)
local_run=(--local)
if [ -n "$CONFIG" ]; then
  perl -MJSON::PP -e '
    my $file = shift;
    open my $fh, "<", $file or die "CONFIG=$file: $!\n";
    my $config = eval { JSON::PP->new->decode(do { local $/; <$fh> }) };
    ref $config eq "HASH" or die "CONFIG=$file: not a JSON object\n";
    exists $config->{routing} or die "CONFIG=$file has no routing section, which local mode needs\n";
  ' "$CONFIG" || exit 2
  local_start+=(--config "$CONFIG")
  local_run=(--config "$CONFIG")
fi

echo "== versions"
"$RAYLINE" --version || exit 1
for harness in "${selected[@]}"; do
  if installed "$harness"; then
    echo "$harness: $("$harness" --version 2>&1 | head -n 1)"
  else
    echo "$harness: not installed"
  fi
done

# The cleanup trap below stops the local process, so the script must not run
# while one it didn't start is running, or when it can't tell.
if ! status_out=$("$RAYLINE" local status 2>&1); then
  echo "$status_out" >&2
  echo "rayline local status failed" >&2
  exit 1
fi
if grep -q '^Local process: running' <<<"$status_out"; then
  echo "a local process is already running; stop it first" >&2
  exit 1
fi

echo "== cloud mode"
run_mode cloud

echo "== local mode"
# Set before `rayline local start`, so a signal during start still stops it.
trap stop_local EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
if "${local_start[@]}"; then
  # After a start, status must exit 0 and report the process running.
  status_out=$("$RAYLINE" local status 2>&1)
  status=$?
  echo "$status_out"
  if [ "$status" -ne 0 ] || ! grep -q '^Local process: running' <<<"$status_out"; then
    echo "FAIL    rayline local status (exit $status)"
    fail=$((fail + 1))
  fi
  run_mode local "${local_run[@]}"
  stop_local
else
  # A refused start (say, a process is already running) must not stop a
  # process this script didn't start.
  trap - EXIT
  stopped=true
  echo "FAIL    rayline local start"
  fail=$((fail + 1))
fi

echo "== pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
