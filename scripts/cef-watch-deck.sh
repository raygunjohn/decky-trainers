#!/usr/bin/env bash
# Install (or manage) the Steam CEF console watcher on the Deck.
#
# A steamwebhelper segfault takes the whole Steam UI down and the JS console
# with it, which is what made the 2026-08-10 crash undiagnosable. This runs
# `tools/cef-watch.py` as a systemd *user* service so the next one is recorded
# whether or not anyone is connected.
#
# Connection settings come from .vscode/settings.json or the same environment
# overrides deploy-deck.sh uses. Run without sudo.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

MODE="install"
LOG_LINES=80

case "${1:-}" in
    "") ;;
    --status) MODE="status" ;;
    --logs) MODE="logs"; LOG_LINES="${2:-80}" ;;
    --follow) MODE="follow" ;;
    --restart) MODE="restart" ;;
    --stop) MODE="stop" ;;
    --uninstall) MODE="uninstall" ;;
    -h|--help)
        cat <<'USAGE'
Usage:
  scripts/cef-watch-deck.sh              install and start the watcher
  scripts/cef-watch-deck.sh --status     is it running, and since when
  scripts/cef-watch-deck.sh --logs [N]   last N lines of the captured console
  scripts/cef-watch-deck.sh --follow     tail the capture live
  scripts/cef-watch-deck.sh --restart    restart the service
  scripts/cef-watch-deck.sh --stop       stop it (stays installed)
  scripts/cef-watch-deck.sh --uninstall  stop, disable and remove it

Reads Deck connection settings from .vscode/settings.json by default.
Environment overrides:
  DECK_IP DECK_PORT DECK_USER DECK_PASS DECK_KEY DECK_DIR
USAGE
        exit 0
        ;;
    *)
        echo "Unknown argument: $1" >&2
        exit 1
        ;;
esac

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "$1 is required." >&2
        exit 1
    fi
}

read_setting() {
    local key="$1"
    local fallback="${2:-}"
    node - "$key" "$fallback" <<'NODE'
const fs = require("fs");
const key = process.argv[2];
const fallback = process.argv[3] ?? "";
const file = ".vscode/settings.json";
if (!fs.existsSync(file)) { process.stdout.write(fallback); process.exit(0); }
let raw = fs.readFileSync(file, "utf8").replace(/^﻿/, "");
raw = raw.replace(/\/\*[\s\S]*?\*\//g, "");
raw = raw.replace(/(^|[^:])\/\/.*$/gm, "$1");
try {
  const settings = JSON.parse(raw);
  process.stdout.write(String(settings[key] ?? fallback));
} catch (error) {
  console.error(`Could not parse ${file}: ${error.message}`);
  process.exit(1);
}
NODE
}

setting_or_env() {
    local env_name="$1" key="$2" fallback="${3:-}"
    local value="${!env_name:-}"
    if [[ -z "$value" ]]; then
        value="$(read_setting "$key" "$fallback")"
    fi
    printf '%s' "$value"
}

expand_env_placeholders() {
    local value="$1"
    value="${value//\$\{env:HOME\}/$HOME}"
    value="${value/#\~/$HOME}"
    printf '%s' "$value"
}

sh_quote() {
    printf "'"
    printf '%s' "$1" | sed "s/'/'\\\\''/g"
    printf "'"
}

run_auth_command() {
    local -a command=("$@")
    if [[ -n "$DECK_PASS" ]] && command -v expect >/dev/null 2>&1; then
    DECK_EXPECT_PASS="$DECK_PASS" expect -f - "${command[@]}" <<'EXPECT'
set timeout -1
set pass $env(DECK_EXPECT_PASS)
spawn {*}$argv
expect {
    -re "(?i)are you sure you want to continue connecting" {
        send "yes\r"
        exp_continue
    }
    -re "(?i)password.*:" {
        log_user 0
        send -- "$pass\r"
        expect {
            -re "\r?\n" { log_user 1 }
            eof { log_user 1 }
        }
        exp_continue
    }
    eof
}
catch wait result
exit [lindex $result 3]
EXPECT
    else
        "${command[@]}"
    fi
}

require_command node
require_command ssh
require_command scp

DECK_IP="$(setting_or_env DECK_IP deckip steamdeck.local)"
DECK_PORT="$(setting_or_env DECK_PORT deckport 22)"
DECK_USER="$(setting_or_env DECK_USER deckuser deck)"
DECK_PASS="$(setting_or_env DECK_PASS deckpass "")"
DECK_KEY_RAW="$(setting_or_env DECK_KEY deckkey "")"
DECK_DIR="$(setting_or_env DECK_DIR deckdir /home/deck)"

if [[ -z "$DECK_IP" || -z "$DECK_USER" || -z "$DECK_DIR" ]]; then
    echo "Deck connection settings are incomplete." >&2
    exit 1
fi

DECK_KEY_RAW="$(expand_env_placeholders "$DECK_KEY_RAW")"
SSH_KEY_ARGS=()
if [[ -n "$DECK_KEY_RAW" ]]; then
    key_path="$DECK_KEY_RAW"
    [[ "$key_path" == -i\ * ]] && key_path="${key_path#-i }"
    if [[ -f "$key_path" ]]; then
        SSH_KEY_ARGS=(-i "$key_path")
    else
        echo "Skipping missing SSH key: $key_path" >&2
    fi
fi

if [[ -z "$DECK_PASS" && -t 0 ]]; then
    read -rsp "Steam Deck SSH password for $DECK_USER@$DECK_IP: " DECK_PASS
    printf '\n'
fi

SSH_ARGS=(-o StrictHostKeyChecking=accept-new -p "$DECK_PORT")
SCP_ARGS=(-o StrictHostKeyChecking=accept-new -P "$DECK_PORT")
if [[ ${#SSH_KEY_ARGS[@]} -gt 0 ]]; then
    SSH_ARGS+=("${SSH_KEY_ARGS[@]}")
    SCP_ARGS+=("${SSH_KEY_ARGS[@]}")
fi

HOST="$DECK_USER@$DECK_IP"
REMOTE_HOME="${DECK_DIR%/}"
REMOTE_DIR="$REMOTE_HOME/cef-watch"
REMOTE_SCRIPT="$REMOTE_DIR/cef-watch.py"
REMOTE_LOG="$REMOTE_DIR/console.log"
UNIT_PATH="$REMOTE_HOME/.config/systemd/user/cef-watch.service"

run_remote() {
    run_auth_command ssh "${SSH_ARGS[@]}" "$HOST" "$1"
}

# `systemctl --user` over SSH has no session bus unless this is set, and the
# failure ("Failed to connect to bus") looks like the unit is broken rather
# than like the shell is missing an environment variable.
USERCTL="export XDG_RUNTIME_DIR=/run/user/\$(id -u); systemctl --user"

q_dir="$(sh_quote "$REMOTE_DIR")"
q_script="$(sh_quote "$REMOTE_SCRIPT")"
q_log="$(sh_quote "$REMOTE_LOG")"
q_unit="$(sh_quote "$UNIT_PATH")"

case "$MODE" in
status)
    run_remote "$USERCTL status cef-watch.service --no-pager -n 5 || true; \
        echo '--- log ---'; ls -l $q_log 2>/dev/null || echo 'no log yet'"
    exit 0
    ;;
logs)
    run_remote "tail -n $LOG_LINES $q_log 2>/dev/null || echo 'no log at $REMOTE_LOG'"
    exit 0
    ;;
follow)
    echo "Tailing $REMOTE_LOG — Ctrl-C to stop."
    run_remote "tail -f $q_log"
    exit 0
    ;;
restart)
    run_remote "$USERCTL restart cef-watch.service && $USERCTL is-active cef-watch.service"
    exit 0
    ;;
stop)
    run_remote "$USERCTL stop cef-watch.service && echo stopped"
    exit 0
    ;;
uninstall)
    run_remote "$USERCTL disable --now cef-watch.service 2>/dev/null; \
        rm -f $q_unit; $USERCTL daemon-reload; \
        echo 'removed the unit; $REMOTE_DIR (script and logs) left in place'"
    exit 0
    ;;
esac

# ---------------------------------------------------------------- install

LOCAL_SCRIPT="$ROOT/tools/cef-watch.py"
if [[ ! -f "$LOCAL_SCRIPT" ]]; then
    echo "Missing $LOCAL_SCRIPT" >&2
    exit 1
fi

echo "Preparing $HOST:$REMOTE_DIR"
run_remote "mkdir -p $q_dir $(sh_quote "$REMOTE_HOME/.config/systemd/user")"

echo "Copying cef-watch.py"
run_auth_command scp "${SCP_ARGS[@]}" "$LOCAL_SCRIPT" "$HOST:$REMOTE_SCRIPT"

UNIT_TMP="$(mktemp)"
trap 'rm -f "$UNIT_TMP"' EXIT
cat >"$UNIT_TMP" <<UNIT
[Unit]
Description=Steam CEF console watcher (crash evidence for Decky plugins)
After=graphical-session.target

[Service]
Type=simple
ExecStart=/usr/bin/python3 $REMOTE_SCRIPT --log $REMOTE_LOG
# The whole point is to be running when the renderer dies, including if the
# watcher itself is the casualty of a Steam restart.
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
UNIT

echo "Installing the user service"
run_auth_command scp "${SCP_ARGS[@]}" "$UNIT_TMP" "$HOST:$UNIT_PATH"

run_remote "$USERCTL daemon-reload && $USERCTL enable --now cef-watch.service && \
    sleep 2 && $USERCTL is-active cef-watch.service && \
    echo '--- first lines ---' && (head -n 12 $q_log 2>/dev/null || echo 'log not written yet')"

cat <<DONE

Watching. The capture is at $REMOTE_LOG (rotated, 4 MB x 3).

  scripts/cef-watch-deck.sh --logs 100   what it has recorded
  scripts/cef-watch-deck.sh --follow     watch it live

After a crash, grep for CRASH — a dropped browser connection is logged as
"CEF CONNECTION LOST", with the memory reading at the moment it went.
DONE
