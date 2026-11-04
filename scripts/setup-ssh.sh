#!/bin/sh
# Install this machine's public key on a build or vantage host, once.
#
# After this, everything else uses key authentication: no password is needed,
# typed, or stored. The password is read from the SSH_PASS environment variable
# and written nowhere — not to a file, not to ~/.ssh/config, and not into shell
# history if the command is prefixed with a space.
#
#   SSH_PASS='...' scripts/setup-ssh.sh void
#
# expect drives ssh-copy-id because it prompts interactively and has no
# non-interactive flag. sshpass is not used: Homebrew refuses to package it,
# and expect is in the base install on FreeBSD and a one-line add elsewhere.

set -eu

self=$(basename -- "$0")

if [ $# -lt 1 ]; then
    printf 'usage: SSH_PASS=... %s <host-alias-from-ssh-config>\n' "$self" >&2
    exit 2
fi
host=$1

if [ -z "${SSH_PASS:-}" ]; then
    printf '%s: set SSH_PASS in the environment.\n' "$self" >&2
    printf 'Passing a password as an argument would put it in ps(1) output and\n' >&2
    printf 'in shell history, where it outlives the command.\n' >&2
    exit 2
fi

key="$HOME/.ssh/id_ed25519.pub"
if [ ! -f "$key" ]; then
    printf '%s: no public key at %s\n' "$self" "$key" >&2
    printf 'create one with:  ssh-keygen -t ed25519\n' >&2
    exit 1
fi

if ssh -o BatchMode=yes -o ConnectTimeout=5 "$host" true 2>/dev/null; then
    printf '%s already accepts key authentication; nothing to do.\n' "$host"
    exit 0
fi

if ! command -v expect >/dev/null 2>&1; then
    printf '%s: expect is not installed.\n' "$self" >&2
    printf '  FreeBSD: pkg install expect\n' >&2
    printf '  Void:    xbps-install -S expect\n' >&2
    printf '  Debian:  apt install expect\n' >&2
    printf '  macOS:   brew install expect\n' >&2
    printf 'Or run it by hand:  ssh-copy-id %s\n' "$host" >&2
    exit 1
fi

printf 'installing %s on %s ...\n' "$key" "$host"

# The password reaches expect through the environment, never as an argument.
#
# shellcheck disable=SC2016  # single quotes are required: $env(SSH_PASS) is
# expect syntax, and letting sh expand it would put the password on expect's
# command line, where ps(1) would show it. That is the one thing this script
# exists to prevent.
expect -c '
    set timeout 60
    set host [lindex $argv 0]
    spawn ssh-copy-id -o StrictHostKeyChecking=accept-new \
        -i $env(HOME)/.ssh/id_ed25519.pub $host
    expect {
        -re {(?i)password:}    { send "$env(SSH_PASS)\r"; exp_continue }
        -re {(?i)passphrase}   { send "\r";               exp_continue }
        -re {(?i)are you sure} { send "yes\r";            exp_continue }
        eof
    }
    catch wait result
    exit [lindex $result 3]
' "$host"

if ssh -o BatchMode=yes -o ConnectTimeout=5 "$host" 'uname -a' 2>/dev/null; then
    printf 'key authentication works on %s.\n' "$host"
else
    printf '%s: the key did not take effect on %s.\n' "$self" "$host" >&2
    printf 'Check that the account can write ~/.ssh/authorized_keys, and that\n' >&2
    printf 'the server allows publickey authentication.\n' >&2
    exit 1
fi
