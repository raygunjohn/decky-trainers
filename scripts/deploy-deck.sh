#!/usr/bin/env bash
set -euo pipefail

ROOT="${ZED_WORKTREE_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$ROOT"

MODE="deploy"
case "${1:-}" in
    "")
        ;;
    --restart-only)
        MODE="restart"
        ;;
    --status)
        MODE="status"
        ;;
    -h|--help)
        cat <<'USAGE'
Usage:
  scripts/deploy-deck.sh
  scripts/deploy-deck.sh --restart-only
  scripts/deploy-deck.sh --status

Reads Deck connection settings from .vscode/settings.json by default.
Environment overrides:
  DECK_IP DECK_PORT DECK_USER DECK_PASS DECK_KEY DECK_DIR PLUGIN_NAME
USAGE
        exit 0
        ;;
    *)
        echo "Unknown argument: $1" >&2
        exit 1
        ;;
esac

require_command() {
    local command_name="$1"
    if ! command -v "$command_name" >/dev/null 2>&1; then
        echo "$command_name is required." >&2
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

if (!fs.existsSync(file)) {
  process.stdout.write(fallback);
  process.exit(0);
}

let raw = fs.readFileSync(file, "utf8").replace(/^\uFEFF/, "");
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

default_plugin_name() {
    node <<'NODE'
try {
  const plugin = require("./plugin.json");
  process.stdout.write(String(plugin.name || ""));
} catch {
  process.stdout.write("");
}
NODE
}

setting_or_env() {
    local env_name="$1"
    local key="$2"
    local fallback="${3:-}"
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
        # SSH restores terminal echo before a later remote sudo prompt. Consume
        # that input line while logging is disabled so the password cannot be
        # copied into build/deployment output.
        expect {
            -re "\r?\n" {
                log_user 1
            }
            eof {
                log_user 1
            }
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

PLUGIN_NAME="$(setting_or_env PLUGIN_NAME pluginname "$(default_plugin_name)")"
DECK_IP="$(setting_or_env DECK_IP deckip steamdeck.local)"
DECK_PORT="$(setting_or_env DECK_PORT deckport 22)"
DECK_USER="$(setting_or_env DECK_USER deckuser deck)"
DECK_PASS="$(setting_or_env DECK_PASS deckpass "")"
DECK_KEY_RAW="$(setting_or_env DECK_KEY deckkey "")"
DECK_DIR="$(setting_or_env DECK_DIR deckdir /home/deck)"

if [[ -z "$PLUGIN_NAME" ]]; then
    echo "Plugin name is missing. Set pluginname in .vscode/settings.json or PLUGIN_NAME." >&2
    exit 1
fi

if [[ -z "$DECK_IP" || -z "$DECK_USER" || -z "$DECK_DIR" ]]; then
    echo "Deck connection settings are incomplete." >&2
    exit 1
fi

DECK_KEY_RAW="$(expand_env_placeholders "$DECK_KEY_RAW")"
SSH_KEY_ARGS=()
if [[ -n "$DECK_KEY_RAW" ]]; then
    key_path="$DECK_KEY_RAW"
    if [[ "$key_path" == -i\ * ]]; then
        key_path="${key_path#-i }"
    fi

    if [[ -f "$key_path" ]]; then
        SSH_KEY_ARGS=(-i "$key_path")
    else
        echo "Skipping missing SSH key: $key_path" >&2
    fi
fi

if [[ -n "$DECK_PASS" ]] && ! command -v expect >/dev/null 2>&1; then
    echo "expect was not found, so SSH/SCP will prompt interactively." >&2
fi

PLUGIN_DIR="${DECK_DIR%/}/homebrew/plugins"
REMOTE_ZIP="$PLUGIN_DIR/$PLUGIN_NAME.zip"
REMOTE_PLUGIN_DIR="$PLUGIN_DIR/$PLUGIN_NAME"
ZIP_PATH="$ROOT/out/$PLUGIN_NAME.zip"
HOST="$DECK_USER@$DECK_IP"

if [[ -z "$DECK_PASS" && "$MODE" != "status" && -t 0 ]]; then
    read -rsp "Steam Deck SSH/sudo password for $HOST: " DECK_PASS
    printf '\n'
fi

SSH_ARGS=(-o StrictHostKeyChecking=accept-new -p "$DECK_PORT")
SCP_ARGS=(-o StrictHostKeyChecking=accept-new -P "$DECK_PORT")
if [[ ${#SSH_KEY_ARGS[@]} -gt 0 ]]; then
    SSH_ARGS+=("${SSH_KEY_ARGS[@]}")
    SCP_ARGS+=("${SSH_KEY_ARGS[@]}")
fi

run_remote() {
    local remote_command="$1"
    run_auth_command ssh "${SSH_ARGS[@]}" "$HOST" "$remote_command"
}

copy_zip() {
    run_auth_command scp "${SCP_ARGS[@]}" "$ZIP_PATH" "$HOST:$REMOTE_ZIP"
}

q_plugins_dir="$(sh_quote "$PLUGIN_DIR")"
q_remote_zip="$(sh_quote "$REMOTE_ZIP")"
q_remote_plugin_dir="$(sh_quote "$REMOTE_PLUGIN_DIR")"
q_owner="$(sh_quote "$DECK_USER:$DECK_USER")"

if [[ "$MODE" == "status" ]]; then
    run_remote "echo 'Plugin zip:'; ls -l $q_remote_zip 2>/dev/null || true; echo 'Plugin files:'; find $q_remote_plugin_dir -maxdepth 2 -type f 2>/dev/null | sort || true; echo 'Decky loader:'; systemctl is-active plugin_loader || true"
    exit 0
fi

if [[ "$MODE" == "restart" ]]; then
    run_remote "sudo -S systemctl restart plugin_loader && systemctl is-active plugin_loader"
    exit 0
fi

if [[ ! -f "$ZIP_PATH" ]]; then
    echo "Missing $ZIP_PATH. Run scripts/build-deck.sh first." >&2
    exit 1
fi

prepare_script="mkdir -p $q_plugins_dir && chown $q_owner $q_plugins_dir && rm -rf $q_remote_plugin_dir && mkdir -p $q_remote_plugin_dir && chown -R $q_owner $q_remote_plugin_dir"
extract_script="cd $q_plugins_dir && bsdtar -xpf $q_remote_zip -C $q_remote_plugin_dir --strip-components=1 && chmod -R u+rwX,go+rX $q_remote_plugin_dir"

# Every file the zip carries must exist afterwards. bsdtar reports a partial
# extraction on stderr and the `&&` chain does abort on it, but a check that
# names the missing paths turns "something went wrong" into "py_modules/ is not
# there", which is the difference between a minute and an hour.
verify_script="cd $q_remote_plugin_dir && missing=0 && for f in \$(bsdtar -tf $q_remote_zip | sed 's|^[^/]*/||' | grep -v '/\$'); do [ -e \"\$f\" ] || { echo \"  MISSING: \$f\"; missing=\$((missing+1)); }; done; [ \"\$missing\" -eq 0 ] || { echo \"\$missing file(s) from the zip did not land - the plugin will not load\" >&2; exit 1; }; echo \"verified \$(bsdtar -tf $q_remote_zip | grep -vc '/\$') file(s)\""

# plugin_loader has to be down for the whole prepare+extract, not just
# restarted afterwards.
#
# It watches the plugins directory. Recreating one under a live loader makes it
# re-scan and take ownership, and that chown races the extraction: the files
# already written stay, the rest fail with "Can't create ... Permission denied".
# Observed dropping py_modules/ entirely, which left the plugin dead with
# `ModuleNotFoundError: No module named 'generate'` - a failure that looks like
# a code bug and is nothing of the sort.
LOADER_STOPPED=0

stop_loader() {
    echo "Stopping plugin_loader (it races the extract otherwise)"
    run_remote "sudo -S systemctl stop plugin_loader"
    LOADER_STOPPED=1
}

start_loader() {
    [[ "$LOADER_STOPPED" == "1" ]] || return 0
    LOADER_STOPPED=0
    echo "Starting plugin_loader"
    run_remote "sudo -S systemctl start plugin_loader && systemctl is-active plugin_loader"
}

# Bring it back even if a step below fails, so a broken deploy does not also
# leave the Deck with no plugin loader at all.
on_exit() {
    local code=$?
    if [[ "$LOADER_STOPPED" == "1" ]]; then
        echo "Deploy did not finish - restarting plugin_loader anyway" >&2
        start_loader || echo "Could not restart plugin_loader; do it by hand" >&2
    fi
    return "$code"
}
trap on_exit EXIT

stop_loader

echo "Preparing $HOST:$REMOTE_PLUGIN_DIR"
run_remote "sudo -S sh -c $(sh_quote "$prepare_script")"

echo "Copying $ZIP_PATH to $HOST:$REMOTE_ZIP"
copy_zip

echo "Extracting plugin"
run_remote "$extract_script"

echo "Verifying every file landed"
run_remote "sh -c $(sh_quote "$verify_script")"

start_loader

echo "Deployment complete."
