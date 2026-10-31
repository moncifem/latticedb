#!/usr/bin/env bash
# Dev stack for a maintainer agent and its subagents: a real latticed with SCRAM over TLS, published on
# 127.0.0.1 only, with one credential per account. dev/compose.yaml holds the containers and
# docs/alpha.md#dev-stack explains the workflow.
#
#   dev/lattice-dev.sh up
#   dev/lattice-dev.sh account add NAME [--read T1,T2] [--write T1,T2] [--ddl T1,T2]
#   dev/lattice-dev.sh account remove NAME
#   dev/lattice-dev.sh psql NAME [psql arguments]
#   dev/lattice-dev.sh status | logs [docker compose logs options] | down
#   dev/lattice-dev.sh reset [--yes]
#
# Environment, all optional:
#   LATTICE_DEV_HOME      owner-only state dir: CA, server key and certificate, account env files.
#                         Default $HOME/.lattice-dev. Never inside the repository.
#   LATTICE_DEV_PROJECT   compose project name and volume prefix. Default lattice-dev.
#   LATTICE_DEV_PORT      host port on 127.0.0.1. Default 5432.
#   LATTICE_DEV_IMAGE     image to run. Default the published 0.1.0-alpha.1 image, pinned by digest.
#   LATTICE_DEV_GATEWAY_PORT  host port on 127.0.0.1 for the sync gateway and POST /mcp, over TLS with
#                         the dev CA (https://127.0.0.1:PORT/mcp). The setting is remembered in the state
#                         dir, so later commands keep it. 0 turns it off and forgets the port. Default:
#                         off. The image must serve the gateway over TLS; the script checks that before
#                         it restarts latticed with the gateway.
#   LATTICE_DEV_DRAIN_SECS  seconds an account add or remove waits for open connections to latticed to
#                         close before it restarts the server. Default 15, the compose stop grace.
#
# Secrets: a generated password is printed once, by `account add`, and written only to that account's
# env file (mode 0600). Passwords never go on a command line: the server reads the account files and
# psql reads PGPASSWORD from the environment of the process this script execs.
set -euo pipefail

die() {
  echo "lattice-dev: $*" >&2
  exit 1
}

usage() {
  sed -n '2,/^set -euo pipefail/{/^set -euo/!p;}' "$0" | sed 's/^# \{0,1\}//'
}

DEV_DIR=$(cd "$(dirname "$0")" && pwd)
COMPOSE_FILE=$DEV_DIR/compose.yaml
HOME_DIR=${LATTICE_DEV_HOME:-$HOME/.lattice-dev}
PROJECT=${LATTICE_DEV_PROJECT:-lattice-dev}
PORT=${LATTICE_DEV_PORT:-5432}
DRAIN_SECS=${LATTICE_DEV_DRAIN_SECS:-15}
GATEWAY_PORT=${LATTICE_DEV_GATEWAY_PORT:-}
# The port latticed binds inside its container when the gateway is on. Only the published port varies.
GATEWAY_CONTAINER_PORT=7070
GATEWAY_OVERRIDE=""
GATEWAY_STATE=""
GATEWAY_OFF=""
GATEWAY_REMEMBERED=""
partial_file=""
LOCK_DIR=""

# Removes what this run created and still owns: a temp file that holds a password, and the lock.
on_exit() {
  if [ -n "$partial_file" ]; then rm -f -- "$partial_file"; fi
  release_lock
}
trap on_exit EXIT

# Prints the physical path of LATTICE_DEV_HOME: symlinks and . or .. are resolved, so that the checks
# below compare what the paths really are. A directory that does not exist yet is resolved through
# its parent, which must exist. Returns 1 when the path cannot be resolved.
resolve_home_dir() {
  local path=$1 base parent
  if [ -d "$path" ]; then
    (cd -P -- "$path" && pwd -P)
    return
  fi
  base=$(basename -- "$path")
  parent=$(dirname -- "$path")
  case "$base" in
    "" | . | ..) return 1 ;;
  esac
  parent=$(cd -P -- "$parent" 2>/dev/null && pwd -P) || return 1
  printf '%s/%s\n' "$parent" "$base"
}

check_settings() {
  case "$HOME_DIR" in
    /*) ;;
    *) die "LATTICE_DEV_HOME must be an absolute path" ;;
  esac
  if ! HOME_DIR=$(resolve_home_dir "$HOME_DIR"); then
    die "LATTICE_DEV_HOME cannot be resolved: its parent directory must exist"
  fi
  # The state dir is chmod'ed and, on reset, has files deleted in it. It must never be the home
  # directory, a parent of it, or the root, whatever spelling (trailing slash, ., .., symlink) was given.
  local home_real
  home_real=$(cd -P -- "$HOME" && pwd -P) || die "HOME is not a directory"
  case "$home_real/" in
    "$HOME_DIR/"*) die "LATTICE_DEV_HOME must not be $HOME or one of its parent directories" ;;
  esac
  [ "$HOME_DIR" != / ] || die "LATTICE_DEV_HOME must not be the root directory"
  [[ $PROJECT =~ ^[a-z0-9][a-z0-9_-]*$ ]] || die "LATTICE_DEV_PROJECT must be lowercase letters, digits, - or _"
  if ! [[ $PORT =~ ^[0-9]{1,5}$ ]] || [ "$PORT" -lt 1 ] || [ "$PORT" -gt 65535 ]; then
    die "LATTICE_DEV_PORT must be a port number"
  fi
  if ! [[ $DRAIN_SECS =~ ^[0-9]{1,3}$ ]] || [ "$DRAIN_SECS" -gt 600 ]; then
    die "LATTICE_DEV_DRAIN_SECS must be a number of seconds from 0 to 600"
  fi
  # The gateway setting is remembered in the state dir, so a command without LATTICE_DEV_GATEWAY_PORT
  # restarts latticed with the gateway still on. LATTICE_DEV_GATEWAY_PORT=0 turns it off.
  if [ -n "${LATTICE_DEV_GATEWAY_PORT:-}" ]; then
    GATEWAY_PORT=$LATTICE_DEV_GATEWAY_PORT
  elif [ -f "$HOME_DIR/gateway.port" ]; then
    GATEWAY_PORT=$(cat -- "$HOME_DIR/gateway.port")
    GATEWAY_REMEMBERED=1
  fi
  if [ "$GATEWAY_PORT" = 0 ]; then
    GATEWAY_PORT=""
    GATEWAY_OFF=1
  fi
  if [ -n "$GATEWAY_PORT" ]; then
    if ! [[ $GATEWAY_PORT =~ ^[0-9]{1,5}$ ]] || [ "$GATEWAY_PORT" -lt 1 ] || [ "$GATEWAY_PORT" -gt 65535 ]; then
      die "LATTICE_DEV_GATEWAY_PORT must be a port number (0 turns the gateway off)"
    fi
    [ "$GATEWAY_PORT" != "$PORT" ] || die "LATTICE_DEV_GATEWAY_PORT must differ from LATTICE_DEV_PORT"
    GATEWAY_OVERRIDE=$HOME_DIR/compose.gateway.yaml
    GATEWAY_STATE=$HOME_DIR/gateway.port
  fi
  export LATTICE_DEV_HOME=$HOME_DIR LATTICE_DEV_PROJECT=$PROJECT LATTICE_DEV_PORT=$PORT
  if [ -n "${LATTICE_DEV_IMAGE:-}" ]; then export LATTICE_DEV_IMAGE; fi
}

need_tools() {
  command -v docker >/dev/null 2>&1 || die "docker is not installed"
  docker compose version >/dev/null 2>&1 || die "docker compose (v2) is not available"
  docker info >/dev/null 2>&1 || die "the docker daemon is not running"
  command -v openssl >/dev/null 2>&1 || die "openssl is not installed"
}

# Every compose call names the project, the file and the tools profile. The tools services are only
# started by `docker compose run`. With the gateway on, the override file written by ensure_state_dir
# goes after the base file, so latticed runs with the gateway in every restart.
compose() {
  local override=()
  if [ -n "$GATEWAY_OVERRIDE" ] && [ -f "$GATEWAY_OVERRIDE" ]; then
    override=(-f "$GATEWAY_OVERRIDE")
  fi
  docker compose --profile tools -p "$PROJECT" -f "$COMPOSE_FILE" ${override[@]+"${override[@]}"} "$@"
}

# With LATTICE_DEV_GATEWAY_PORT set, latticed also serves the sync gateway and POST /mcp on its port
# 7070, with the TLS files it already has and the bearer tokens of the config volume. Compose cannot
# add list items on a condition, so this override replaces latticed's command and ports. Its command
# repeats the list in compose.yaml, and the Host the gateway accepts is the published port on
# 127.0.0.1: the bound address is 0.0.0.0, so the default Host values do not match it.
write_gateway_override() {
  [ -n "$GATEWAY_OVERRIDE" ] || return 0
  local tmp=$GATEWAY_OVERRIDE.tmp
  umask 077
  cat > "$tmp" <<YAML
# Written by dev/lattice-dev.sh for LATTICE_DEV_GATEWAY_PORT=$GATEWAY_PORT. It is rewritten on every
# run that sets the variable, and reset removes it. Its command list repeats dev/compose.yaml.
services:
  latticed:
    command:
      - --data-dir
      - /var/lib/lattice
      - --auth
      - scram-tls
      - --listen
      - 0.0.0.0:5432
      - --tls-cert
      - /etc/lattice-tls/server-cert.pem
      - --tls-key
      - /etc/lattice-tls/server-key.pem
      - --scram-verifiers
      - /etc/lattice/scram-verifiers
      - --scram-decoy-secret
      - /etc/lattice/scram-decoy-secret
      - --account-policies
      - /etc/lattice/account-policies
      - --stdin-control
      - "off"
      - --gateway-listen
      - "0.0.0.0:$GATEWAY_CONTAINER_PORT"
      - --gateway-tokens
      - /etc/lattice/bearer-tokens
      - --gateway-allowed-host
      - "127.0.0.1:$GATEWAY_PORT"
    ports:
      - target: 5432
        published: "$PORT"
        host_ip: 127.0.0.1
      - target: $GATEWAY_CONTAINER_PORT
        published: "$GATEWAY_PORT"
        host_ip: 127.0.0.1
YAML
  mv -- "$tmp" "$GATEWAY_OVERRIDE"
}

# The image must serve the gateway over TLS. This runs before latticed restarts with the gateway: a
# start that fails would otherwise crash-loop under restart: unless-stopped with the stack half-started.
require_gateway_image() {
  local help rc=0
  help=$(compose run --rm --no-deps -T --entrypoint /usr/local/bin/latticed lattice-cli --help 2>/dev/null) || rc=$?
  [ "$rc" -eq 0 ] || die "cannot run latticed --help in the image (exit $rc): set LATTICE_DEV_IMAGE to a working image, or LATTICE_DEV_GATEWAY_PORT=0"
  if ! grep -qF "serve the gateway over TLS" <<<"$help" || ! grep -qF -- "--gateway-allowed-host" <<<"$help"; then
    die "the image's latticed has no TLS gateway, so LATTICE_DEV_GATEWAY_PORT cannot be used with it: set LATTICE_DEV_IMAGE to an image built from main at or after 4b297405, or LATTICE_DEV_GATEWAY_PORT=0"
  fi
}

ensure_state_dir() {
  umask 077
  if [ ! -d "$HOME_DIR" ]; then
    mkdir -m 0700 "$HOME_DIR" || die "cannot create $HOME_DIR (its parent must exist)"
  fi
  mkdir -p "$HOME_DIR/tls" "$HOME_DIR/accounts"
  chmod 0700 "$HOME_DIR" "$HOME_DIR/tls" "$HOME_DIR/accounts"
  if [ -n "$GATEWAY_PORT" ]; then
    require_gateway_image
    if [ "$GATEWAY_REMEMBERED" = 1 ]; then
      echo "lattice-dev: the gateway stays on at 127.0.0.1:$GATEWAY_PORT, remembered in $GATEWAY_STATE; LATTICE_DEV_GATEWAY_PORT=0 turns it off" >&2
    fi
    printf '%s\n' "$GATEWAY_PORT" > "$GATEWAY_STATE"
    write_gateway_override
  elif [ -n "$GATEWAY_OFF" ]; then
    rm -f -- "$HOME_DIR/gateway.port" "$HOME_DIR/compose.gateway.yaml"
  fi
}

# Creates the CA and the server certificate on first use, and renews the server certificate when it
# expires within a day. Sets CERTS_CHANGED=1 when either file was written.
CERTS_CHANGED=0
ensure_certificates() {
  local ca_key=$HOME_DIR/ca-key.pem ca=$HOME_DIR/ca.pem tls=$HOME_DIR/tls
  if [ ! -f "$ca_key" ] || [ ! -f "$ca" ]; then
    rm -f "$ca_key" "$ca" "$HOME_DIR/ca.srl"
    openssl req -x509 -newkey rsa:2048 -nodes -keyout "$ca_key" -out "$ca" -days 3650 \
      -subj "/CN=lattice dev CA" -addext "basicConstraints=critical,CA:TRUE" \
      -addext "keyUsage=critical,keyCertSign,cRLSign" 2>/dev/null \
      || die "openssl could not create the CA"
    chmod 0600 "$ca_key"
    rm -f "$tls/server-cert.pem"
    CERTS_CHANGED=1
  fi
  if [ ! -f "$tls/server-key.pem" ] || [ ! -f "$tls/server-cert.pem" ] \
    || ! openssl x509 -checkend 86400 -noout -in "$tls/server-cert.pem" >/dev/null 2>&1; then
    local ext=$tls/server.ext csr=$tls/server.csr
    printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' > "$ext"
    openssl req -newkey rsa:2048 -nodes -keyout "$tls/server-key.pem" -out "$csr" -subj "/CN=localhost" 2>/dev/null \
      || die "openssl could not create the server key"
    openssl x509 -req -in "$csr" -CA "$ca" -CAkey "$ca_key" -CAserial "$HOME_DIR/ca.srl" -CAcreateserial \
      -days 365 -extfile "$ext" -out "$tls/server-cert.pem" 2>/dev/null \
      || die "openssl could not sign the server certificate"
    chmod 0600 "$tls/server-key.pem"
    rm -f "$csr" "$ext"
    CERTS_CHANGED=1
  fi
}

# Copies the server key and certificate into the tls volume, owned by the server's uid.
install_tls() {
  compose run --rm --no-deps -T tls-install >/dev/null || die "could not copy the TLS files into the volume"
}

# The credential files exist once `lattice account init` has run; the verifier file once the first
# account is added. latticed refuses to start without accounts, so `up` starts it only then.
config_has() {
  compose run --rm --no-deps -T --entrypoint test lattice-cli -f "/etc/lattice/$1" >/dev/null 2>&1
}

is_running() {
  [ -n "$(compose ps --status running -q latticed 2>/dev/null)" ]
}

# One dev-stack command at a time. The lock is a directory in the state dir, so it works on macOS and
# Linux alike, and it holds the pid of its owner. Commands that restart or delete the server's files
# take it: account add and remove, up, and reset. A lock whose pid is gone is stale and is taken over.
# A live holder is waited for up to 180 seconds, longer than its worst legitimate hold.
take_lock() {
  [ -d "$HOME_DIR" ] || return 0
  local tries=0 holder=""
  until mkdir -m 0700 "$HOME_DIR/.lock" 2>/dev/null; do
    holder=$(cat "$HOME_DIR/.lock/pid" 2>/dev/null || true)
    if [ -n "$holder" ] && ! kill -0 "$holder" 2>/dev/null; then
      # Rename first: if another run already moved it, the rename fails and the loop tries again.
      mv "$HOME_DIR/.lock" "$HOME_DIR/.lock.stale.$$" 2>/dev/null || true
      rm -rf -- "$HOME_DIR/.lock.stale.$$"
      continue
    fi
    tries=$((tries + 1))
    [ "$tries" -le 180 ] || die "another lattice-dev command (pid ${holder:-unknown}) still holds $HOME_DIR/.lock after 180 seconds"
    sleep 1
  done
  printf '%s\n' "$$" > "$HOME_DIR/.lock/pid"
  LOCK_DIR=$HOME_DIR/.lock
}

release_lock() {
  if [ -n "$LOCK_DIR" ]; then
    rm -f -- "$LOCK_DIR/pid"
    rmdir -- "$LOCK_DIR" 2>/dev/null || true
    LOCK_DIR=""
  fi
}

# Prints how many TCP connections latticed holds to its port 5432, read from /proc inside the container
# (the image has bash but no other tool). Local port 5432 is 1538 in hex; state 01 is ESTABLISHED.
open_connections() {
  local n
  # shellcheck disable=SC2016 # the $ expansions are for the container's bash, not this shell
  n=$(compose exec -T latticed bash -c '
    n=0
    { read -r _
      while read -r _ laddr _ st _; do
        if [ "${laddr##*:}" = 1538 ] && [ "$st" = 01 ]; then n=$((n + 1)); fi
      done
    } < /proc/net/tcp
    echo "$n"') || die "cannot count the connections latticed holds"
  [[ $n =~ ^[0-9]+$ ]] || die "cannot count the connections latticed holds"
  printf '%s\n' "$n"
}

# A restart cuts every connection, and an open transaction with it. Before any change to the server's
# files, wait up to DRAIN_SECS for the connections to close. If they stay open, refuse: nothing has
# been written yet, so the caller retries once the sessions are closed.
wait_for_drain() {
  is_running || return 0
  local waited=0 n
  while :; do
    n=$(open_connections)
    [ "$n" = 0 ] && return 0
    if [ "$waited" -ge "$DRAIN_SECS" ]; then
      die "latticed holds $n open connection(s) to its port 5432: close them, or raise LATTICE_DEV_DRAIN_SECS, then retry. Nothing was changed."
    fi
    sleep 1
    waited=$((waited + 1))
  done
}

# Starts latticed, or restarts it when it runs with files that changed, and waits until it is healthy.
start_latticed() {
  if [ "$1" = restart ] && is_running; then
    compose restart latticed >/dev/null || die "could not restart latticed"
  fi
  compose up -d --wait --wait-timeout 120 latticed >/dev/null || die "latticed did not become healthy: see lattice-dev.sh logs"
}

# A name becomes a file name and an argument of lattice and psql, so it must start with a letter or a
# digit (a leading - would read as an option) and hold nothing else.
check_name() {
  [[ ${1:-} =~ ^[A-Za-z0-9][A-Za-z0-9_-]{0,62}$ ]] \
    || die "an account name is 1 to 63 letters, digits, _ or -, and starts with a letter or digit"
}

cmd_up() {
  ensure_state_dir
  take_lock
  ensure_certificates
  install_tls
  if ! config_has scram-decoy-secret; then
    compose run --rm --no-deps -T lattice-cli account init /etc/lattice >/dev/null \
      || die "lattice account init failed"
  fi
  if config_has scram-verifiers; then
    if [ "$CERTS_CHANGED" = 1 ]; then start_latticed restart; else start_latticed keep; fi
    echo "latticed is up on 127.0.0.1:$PORT (project $PROJECT, state $HOME_DIR)"
  else
    echo "stack prepared on 127.0.0.1:$PORT (project $PROJECT, state $HOME_DIR)"
    echo "latticed starts with the first account: lattice-dev.sh account add NAME [--read T] [--write T] [--ddl T]"
  fi
  echo "CA for libpq sslrootcert: $HOME_DIR/ca.pem"
}

cmd_account_add() {
  local name=${1:-}
  check_name "$name"
  shift
  ensure_state_dir
  [ -f "$HOME_DIR/tls/server-cert.pem" ] || die "run lattice-dev.sh up first"
  take_lock
  local envfile=$HOME_DIR/accounts/$name.env
  [ ! -e "$envfile" ] || die "account $name already exists (its env file is $envfile)"
  wait_for_drain

  # The tool prints the generated password once on stdout. Only that line is kept; the rest of its
  # output is not shown. Stderr passes through: the tool never puts a password there.
  local out pw
  out=$(compose run --rm --no-deps -T lattice-cli account add /etc/lattice "$name" "$@") \
    || die "lattice account add failed"
  pw=$(printf '%s\n' "$out" | sed -n 's/^password (shown once, not stored): //p')
  [ -n "$pw" ] || die "lattice account add printed no password"

  umask 077
  # The temp file holds the password until it is renamed, so the env file appears whole or not at all.
  # on_exit removes the temp file if anything fails before the rename.
  partial_file=$(mktemp "$HOME_DIR/accounts/.$name.XXXXXX")
  printf 'PGHOST=%q\nPGPORT=%q\nPGDATABASE=%q\nPGUSER=%q\nPGPASSWORD=%q\nPGSSLMODE=%q\nPGSSLROOTCERT=%q\n' \
    127.0.0.1 "$PORT" lattice "$name" "$pw" verify-full "$HOME_DIR/ca.pem" > "$partial_file"
  mv -- "$partial_file" "$envfile"
  partial_file=""

  # latticed reads the verifier file at start only, so a running server restarts to see the account.
  start_latticed restart
  echo "account $name added on the server; its libpq settings are in $envfile"
  echo "password (shown once, saved in that file): $pw"
}

# Removes the login, the grants and the bearer tokens of NAME, then its env file. latticed reads the
# verifier file at start only, so it restarts to drop the login. Removing the last account deletes the
# verifier file, which latticed cannot start without, so the server is stopped instead.
cmd_account_remove() {
  local name=${1:-}
  check_name "$name"
  [ $# -eq 1 ] || die "usage: lattice-dev.sh account remove NAME"
  ensure_state_dir
  take_lock
  wait_for_drain

  local out
  out=$(compose run --rm --no-deps -T lattice-cli account remove /etc/lattice "$name") \
    || die "lattice account remove failed (no account $name, or the credential files are missing)"
  rm -f -- "$HOME_DIR/accounts/$name.env"
  printf '%s\n' "$out"
  # Only a running server is restarted or stopped: a stack that is down stays down, and reads the
  # change when it next starts.
  if config_has scram-verifiers; then
    if is_running; then
      start_latticed restart
      echo "latticed restarted without $name"
    else
      echo "latticed is not running; it reads the change when it starts"
    fi
  else
    if is_running; then
      compose stop latticed >/dev/null || die "could not stop latticed"
    fi
    echo "no accounts left: latticed is stopped (the next account add starts it)"
  fi
}

cmd_psql() {
  local name=${1:-}
  check_name "$name"
  shift
  local envfile=$HOME_DIR/accounts/$name.env
  [ -f "$envfile" ] || die "no account $name: add it with lattice-dev.sh account add $name"
  command -v psql >/dev/null 2>&1 || die "psql is not installed (brew install libpq)"
  set -a
  # shellcheck source=/dev/null
  . "$envfile"
  set +a
  exec psql "$@"
}

cmd_status() {
  compose ps || true
  local names=() f
  for f in "$HOME_DIR"/accounts/*.env; do
    [ -e "$f" ] || break
    names+=("$(basename "$f" .env)")
  done
  echo "accounts: ${names[*]:-none}"
  echo "state dir: $HOME_DIR"
}

cmd_logs() {
  if [ $# -eq 0 ]; then
    set -- --tail 100
  fi
  compose logs --no-color "$@" latticed
}

cmd_down() {
  compose down --remove-orphans
}

cmd_reset() {
  if [ "${1:-}" != --yes ]; then
    cat >&2 <<EOF
reset deletes every row and every credential of project $PROJECT:
  the containers and the volumes (data, config, tls)
  $HOME_DIR (CA, server key and certificate, account env files)
EOF
    printf 'type yes to continue: ' >&2
    local answer=""
    read -r answer || true
    [ "$answer" = yes ] || die "not reset"
  fi
  take_lock
  compose down --volumes --remove-orphans
  # A volume that compose did not own (created by hand) keeps its project label only if compose made it.
  docker volume ls -q --filter "label=com.docker.compose.project=$PROJECT" | while IFS= read -r v; do
    docker volume rm "$v" >/dev/null
  done
  rm -f "$HOME_DIR/ca-key.pem" "$HOME_DIR/ca.pem" "$HOME_DIR/ca.srl" "$HOME_DIR/gateway.port" "$HOME_DIR/compose.gateway.yaml" "$HOME_DIR/compose.gateway.yaml.tmp"
  rm -rf "$HOME_DIR/tls" "$HOME_DIR/accounts"
  release_lock
  rmdir "$HOME_DIR" 2>/dev/null || true
  echo "reset project $PROJECT"
}

main() {
  local cmd=${1:-}
  case "$cmd" in
    "" | -h | --help | help)
      usage
      return 0
      ;;
  esac
  shift || true
  check_settings
  need_tools
  case "$cmd" in
    up) cmd_up ;;
    down) cmd_down ;;
    status) cmd_status ;;
    reset) cmd_reset "$@" ;;
    logs) cmd_logs "$@" ;;
    account)
      local sub=${1:-}
      shift || true
      case "$sub" in
        add) cmd_account_add "$@" ;;
        remove) cmd_account_remove "$@" ;;
        *) die "usage: lattice-dev.sh account add NAME [--read T] [--write T] [--ddl T] | account remove NAME" ;;
      esac
      ;;
    psql) cmd_psql "$@" ;;
    *) die "unknown subcommand '$cmd' (see lattice-dev.sh --help)" ;;
  esac
}

main "$@"
