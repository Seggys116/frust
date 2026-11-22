#!/bin/bash
# Stop the background services of a GitHub-hosted runner that spend CPU or disk while a benchmark runs,
# and write what was done, and what was still running afterwards, to a file the benchmark driver records
# in its results. Every step is best effort: a refused command is noted and the run goes on, because the
# measurement is paired and a noisier runner only widens the intervals; it never changes a ratio.
# Usage: quiet-runner.sh <note-file>
# Needs passwordless sudo, which the hosted runner images have. Works with bash 3.2 (macOS).
set -uo pipefail
if [[ $# -ne 1 ]]; then
    echo "usage: quiet-runner.sh <note-file>" >&2
    exit 2
fi
note=$1
: > "$note"

say() {
    printf '%s\n' "$*" | tee -a "$note"
}

# try <description> <command...>: run a privileged or optional command, record whether it worked.
try() {
    local what=$1 output
    shift
    if output=$("$@" 2>&1); then
        say "ok: $what"
    else
        say "refused: $what: $(printf '%s' "$output" | tr '\n' ' ' | cut -c1-160)"
    fi
}

case $(uname -s) in
    Darwin)
        # Spotlight indexes every new file on the volume, and the benchmark writes gigabytes of them. The
        # work directory is also named *.noindex by the workflow, which Spotlight skips on its own.
        try "spotlight indexing off" sudo mdutil -a -i off
        say "spotlight: $(mdutil -s / 2>&1 | tr '\n' ' ')"
        # Gatekeeper assesses every new executable (build scripts, proc macros, test binaries), and the
        # security daemons then scan it. The assessment can be switched off; the XProtect scan cannot.
        try "gatekeeper assessments off (master-disable)" sudo spctl --master-disable
        try "gatekeeper assessments off (global-disable)" sudo spctl --global-disable
        say "gatekeeper: $(spctl --status 2>&1 | tr '\n' ' ')"
        try "time machine off" sudo tmutil disable
        try "automatic software update checks off" sudo softwareupdate --schedule off
        try "sleep and disk sleep off" sudo pmset -a sleep 0 disksleep 0 displaysleep 0
        try "crash reporter off" defaults write com.apple.CrashReporter DialogType none
        ;;
    Linux)
        for unit in apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service motd-news.timer \
            man-db.timer fstrim.timer update-notifier-download.timer; do
            if systemctl cat "$unit" > /dev/null 2>&1; then
                try "stop $unit" sudo systemctl stop "$unit"
            fi
        done
        say "frequency governor: $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo 'not exposed')"
        ;;
esac

# Let the settling work of the last steps (installing the toolchains writes gigabytes) finish.
sync
say "busiest processes now:"
if [[ $(uname -s) == Darwin ]]; then
    ps -Ao pcpu=,comm= -r | head -n 8 | tee -a "$note"
else
    ps -eo pcpu=,comm= --sort=-pcpu | head -n 8 | tee -a "$note"
fi
exit 0
