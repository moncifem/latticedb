#!/usr/bin/env bash
# End-to-end test of dev/lattice-dev.sh against the local Docker daemon: the published image, compose,
# the throwaway CA, SCRAM over TLS and libpq. It runs in its own compose project and its own state dir,
# both picked here, so a stack the owner runs is never touched. It deletes what it created on exit.
#
#   tests/dev/dev-stack.sh
#
# Steps, in the order the script runs them:
#   1.  up with no accounts (no server yet); the state dir and key files have owner-only modes.
#   1b. invalid account names and a state dir that is home or one of its parents are refused.
#   2.  add alice (read, write and ddl on notes) and bob (read on notes); a duplicate name is refused.
#   2b. the published port is bound to 127.0.0.1 only.
#   3.  alice creates the table and inserts two rows; bob reads them and is refused a write.
#   4.  a wrong password is refused.
#   5.  status names the accounts and shows no password.
#   6.  down, then up on the same volumes; the rows are read back.
#   6a. remove alice: her token is revoked, her env file goes, an old copy of it no longer logs in,
#       bob still reads, and the name can be added again.
#   6b. add while the server runs: refused while alice's session is open (nothing changes); after the
#       session writes a row and ends, the add restarts latticed, the row survives and carol logs in.
#   6c. remove the last accounts; removing the last one stops latticed.
#   6d. the gateway, opt-in: needs LATTICE_DEV_IMAGE, an image whose latticed serves the gateway over
#       TLS. Without one, or with one that predates it, the step prints a skip and its reason. With one:
#       three accounts; POST /mcp on 127.0.0.1 over TLS refuses no token and a wrong token and plain
#       http; a writer and a reader share a key; a principal without grants is refused; the published
#       port is 127.0.0.1 only; a removed account's token stops working; the setting is remembered
#       across a command that does not name it, and LATTICE_DEV_GATEWAY_PORT=0 turns it off.
#   7.  reset without a yes is refused.
#   7b. a failed rename leaves no temp file holding a password.
#   8.  reset, answered yes at the prompt, removes every container, volume and file the test made.
#
# Needs docker with compose v2, openssl and psql. Passwords are never printed by this script: it reads
# them from the account env files and checks the counts of their appearances in the command output.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
dev=$root/dev/lattice-dev.sh

run_id=$(openssl rand -hex 4)
export LATTICE_DEV_PROJECT=lattice-dev-test-$run_id
export LATTICE_DEV_HOME
LATTICE_DEV_HOME=$(mktemp -d "${TMPDIR:-/tmp}/lattice-dev-test.XXXXXX")
work=$(mktemp -d "${TMPDIR:-/tmp}/lattice-dev-work.XXXXXX")

port=${LATTICE_DEV_TEST_PORT:-25500}
if [ -z "${LATTICE_DEV_TEST_PORT:-}" ]; then
  # The first port from 25500 that nothing listens on.
  while (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; do
    port=$((port + 1))
  done
fi
export LATTICE_DEV_PORT=$port

# Every step before 6d runs on the pinned default image, whatever the caller set. LATTICE_DEV_IMAGE
# names the image for the gateway step only, and the gateway is off unless step 6d turns it on.
gateway_image=${LATTICE_DEV_IMAGE:-}
unset LATTICE_DEV_IMAGE LATTICE_DEV_GATEWAY_PORT

case "$LATTICE_DEV_PROJECT" in
  lattice-dev-test-*) ;;
  *) echo "refusing to run with project $LATTICE_DEV_PROJECT" >&2; exit 1 ;;
esac

cleanup() {
  status=$?
  kill "${watchdog:-}" 2>/dev/null || true
  "$dev" reset --yes >/dev/null 2>&1 || true
  # Anything reset could not remove: containers and volumes that carry this project's label.
  docker ps -aq --filter "label=com.docker.compose.project=$LATTICE_DEV_PROJECT" | while IFS= read -r c; do
    docker rm -f "$c" >/dev/null 2>&1 || true
  done
  docker volume ls -q --filter "label=com.docker.compose.project=$LATTICE_DEV_PROJECT" | while IFS= read -r v; do
    docker volume rm "$v" >/dev/null 2>&1 || true
  done
  rm -rf "$LATTICE_DEV_HOME" "$work"
  exit "$status"
}
trap cleanup EXIT
# Whole-run watchdog: a hang anywhere fails the run after 20 minutes instead of the job timeout.
# TERM runs the EXIT trap, so cleanup still removes the project and the state dir.
trap 'exit 124' TERM
(sleep 1200 && kill -TERM $$) >/dev/null 2>&1 &
watchdog=$!

fail() {
  echo "FAILED  $*" >&2
  exit 1
}

ok() {
  echo "ok      $*"
}

# expect_ok NAME COMMAND...: stdout and stderr go to $work/NAME.{out,err}; on failure the stderr tail
# is shown. Command output is never shown in full, because `account add` prints a password.
expect_ok() {
  local name=$1
  shift
  if ! "$@" >"$work/$name.out" 2>"$work/$name.err"; then
    tail -n 20 "$work/$name.err" >&2
    fail "$name exited non-zero"
  fi
}

# expect_refused NAME COMMAND...: the command must exit non-zero.
expect_refused() {
  local name=$1
  shift
  if "$@" >"$work/$name.out" 2>"$work/$name.err"; then
    fail "$name succeeded but was expected to be refused"
  fi
}

mode_of() {
  if stat -c %a "$1" >/dev/null 2>&1; then stat -c %a "$1"; else stat -f %Lp "$1"; fi
}

password_of() {
  (
    # shellcheck source=/dev/null
    . "$1"
    printf '%s' "$PGPASSWORD"
  )
}

# Runs a lattice-cli command in the tools container of this test's project. The dev script has no
# subcommand for tokens, so the test calls the tool directly. Its output can hold a token: callers
# check it with grep and never print it.
tool() {
  docker compose --profile tools -p "$LATTICE_DEV_PROJECT" -f "$root/dev/compose.yaml" run --rm --no-deps -T lattice-cli "$@"
}

# Containers and volumes that carry this project's label.
project_containers() {
  docker ps -aq --filter "label=com.docker.compose.project=$LATTICE_DEV_PROJECT"
}
project_volumes() {
  docker volume ls -q --filter "label=com.docker.compose.project=$LATTICE_DEV_PROJECT"
}

expected_rows=$'1|write the spec|f\n2|check the restart|t'
expected_rows_3=$'1|write the spec|f\n2|check the restart|t\n3|written by a held session|f'
notes_ddl="CREATE TABLE notes (id INT PRIMARY KEY, title TEXT NOT NULL, done BOOLEAN)"
notes_rows="INSERT INTO notes VALUES (1, 'write the spec', false), (2, 'check the restart', true)"

echo "project $LATTICE_DEV_PROJECT, state dir $LATTICE_DEV_HOME, port $LATTICE_DEV_PORT"

# --- 1. up before any account: the stack is prepared and latticed does not start
expect_ok up "$dev" up
if [ -n "$(docker ps -q --filter "label=com.docker.compose.project=$LATTICE_DEV_PROJECT" --filter label=com.docker.compose.service=latticed)" ]; then
  fail "latticed started before any account exists"
fi
ok "up_without_accounts_prepares_the_stack_and_does_not_start_the_server"

[ "$(mode_of "$LATTICE_DEV_HOME")" = 700 ] || fail "state dir is not mode 700"
for f in ca-key.pem tls/server-key.pem; do
  [ "$(mode_of "$LATTICE_DEV_HOME/$f")" = 600 ] || fail "$f is not mode 600"
done
ok "state_dir_is_0700_and_key_files_are_0600"

# --- 1b. names and state-dir paths that must be refused before any server or account is touched
long=
printf -v long '%064d' 0
bad_names=(-x --read a/b "bad name" "" "$long" _leading .dotted)
i=0
for n in "${bad_names[@]}"; do
  i=$((i + 1))
  expect_refused "bad-name-$i" "$dev" account add "$n" --read notes
  if ! grep -q 'an account name is' "$work/bad-name-$i.err"; then
    fail "name number $i was not refused by the name check"
  fi
done
expect_refused bad-name-psql "$dev" psql -x -c "SELECT 1"
grep -q 'an account name is' "$work/bad-name-psql.err" || fail "psql accepted the name -x"
ok "invalid_account_names_are_refused_before_the_server_is_used"

home_mode=$(mode_of "$HOME")
symlink_home=$work/home-link
ln -s "$HOME" "$symlink_home"
guard_paths=("$HOME/" "$HOME/." "$HOME/.." "$symlink_home" / "$(dirname "$HOME")")
i=0
for p in "${guard_paths[@]}"; do
  i=$((i + 1))
  expect_refused "home-guard-$i" env LATTICE_DEV_HOME="$p" "$dev" status
done
[ "$(mode_of "$HOME")" = "$home_mode" ] || fail "the home directory mode changed"
# A directory of its own under home is accepted: the guard refuses home and its parents, not home's children.
expect_ok home-child env LATTICE_DEV_HOME="$HOME/.lattice-dev-guard-probe" "$dev" status
[ ! -e "$HOME/.lattice-dev-guard-probe" ] || fail "status created a state dir"
ok "state_dir_that_is_home_or_a_parent_is_refused_in_every_spelling"

# --- 2. two accounts; each add prints its generated password once and restarts the server
expect_ok add-alice "$dev" account add alice --read notes --write notes --ddl notes
alice_pw=$(password_of "$LATTICE_DEV_HOME/accounts/alice.env")
[ "$(grep -cF -- "$alice_pw" "$work/add-alice.out")" = 1 ] || fail "alice's password is not printed exactly once"
grep -qF -- "$alice_pw" "$work/add-alice.err" && fail "alice's password reached stderr"
[ "$(mode_of "$LATTICE_DEV_HOME/accounts/alice.env")" = 600 ] || fail "alice.env is not mode 600"
ok "account_add_prints_the_password_once_and_writes_an_owner_only_env_file"

expect_refused duplicate-alice "$dev" account add alice --read notes
grep -q 'already exists' "$work/duplicate-alice.err" || fail "a duplicate name was not refused as taken"
[ "$(password_of "$LATTICE_DEV_HOME/accounts/alice.env")" = "$alice_pw" ] || fail "a duplicate add changed alice's env file"
ok "duplicate_account_name_is_refused_and_keeps_the_env_file"

expect_ok add-bob "$dev" account add bob --read notes
bob_pw=$(password_of "$LATTICE_DEV_HOME/accounts/bob.env")
[ "$(grep -cF -- "$bob_pw" "$work/add-bob.out")" = 1 ] || fail "bob's password is not printed exactly once"
ok "second_account_added"

# --- 2b. the published port is bound to loopback only (host_ip in compose.yaml). docker port lists every
# host binding: 127.0.0.1 alone passes, and 0.0.0.0 or [::] from a missing host_ip fails.
latticed_id=$(docker ps -q --filter "label=com.docker.compose.project=$LATTICE_DEV_PROJECT" --filter label=com.docker.compose.service=latticed)
[ -n "$latticed_id" ] || fail "latticed is not running, so its published port cannot be checked"
published=$(docker port "$latticed_id" 5432) || fail "docker port could not list the published port"
[ -n "$published" ] || fail "latticed publishes no port"
while IFS= read -r binding; do
  [ "$binding" = "127.0.0.1:$port" ] || fail "published port is bound to '$binding', wanted 127.0.0.1:$port only"
done <<<"$published"
ok "published_port_is_bound_to_loopback_only"

# --- 3. alice creates the table as an account with ddl and writes rows; bob reads them
expect_ok alice-ddl "$dev" psql alice -X -q -v ON_ERROR_STOP=1 -c "$notes_ddl" -c "$notes_rows"
expect_ok bob-read "$dev" psql bob -X -q -t -A -c "SELECT id, title, done FROM notes ORDER BY id"
[ "$(cat "$work/bob-read.out")" = "$expected_rows" ] || fail "bob read '$(cat "$work/bob-read.out")', wanted the two rows"
ok "alice_creates_the_table_and_bob_reads_its_rows"

expect_refused bob-write "$dev" psql bob -X -q -c "INSERT INTO notes VALUES (3, 'bob write', false)"
grep -q 'permission denied' "$work/bob-write.err" || fail "bob's write was refused for another reason"
ok "bob_without_write_grant_is_refused_a_write"

# --- 4. a wrong password is refused for the same account
sed 's/^PGPASSWORD=.*/PGPASSWORD=not-the-password-of-alice/' "$LATTICE_DEV_HOME/accounts/alice.env" \
  > "$LATTICE_DEV_HOME/accounts/alice-wrong.env"
expect_refused wrong-password "$dev" psql alice-wrong -X -c "SELECT 1"
grep -q 'FATAL' "$work/wrong-password.err" || fail "a refused login did not say FATAL"
grep -qF -- "not-the-password-of-alice" "$work/wrong-password.err" && fail "the wrong password was echoed"
rm -f "$LATTICE_DEV_HOME/accounts/alice-wrong.env"
ok "wrong_password_is_refused"

# --- 5. status names the accounts and shows no password
expect_ok status "$dev" status
if ! grep -q 'accounts: alice bob$' "$work/status.out"; then fail "status does not list both accounts"; fi
grep -qF -- "$alice_pw" "$work/status.out" && fail "status printed alice's password"
ok "status_lists_the_accounts_without_passwords"

# --- 6. down, then up on the same volumes: the rows are durable
expect_ok down "$dev" down
[ -z "$(project_containers)" ] || fail "containers remain after down"
[ -n "$(project_volumes)" ] || fail "down removed the volumes; it must keep them"
expect_ok up-again "$dev" up
[ -n "$(docker ps -q --filter health=healthy --filter "label=com.docker.compose.project=$LATTICE_DEV_PROJECT")" ] \
  || fail "latticed is not healthy after the restart"
expect_ok bob-reread "$dev" psql bob -X -q -t -A -c "SELECT id, title, done FROM notes ORDER BY id"
[ "$(cat "$work/bob-reread.out")" = "$expected_rows" ] || fail "rows after the restart were '$(cat "$work/bob-reread.out")'"
ok "rows_read_back_after_down_and_up_on_the_same_volumes"

# --- 6a. remove alice. Her login, grants and token go, and so does her env file. A copy of that file
# stands for one that was lost or copied: its password must stop working. bob is not affected, and
# the name can be added again.
cp "$LATTICE_DEV_HOME/accounts/alice.env" "$LATTICE_DEV_HOME/accounts/alice-stale.env"
expect_ok alice-token tool account token create /etc/lattice alice
grep -q '^token .* created for alice$' "$work/alice-token.out" || fail "alice has no token to revoke"
expect_ok remove-alice "$dev" account remove alice
grep -qx 'account alice removed; 1 bearer tokens revoked' "$work/remove-alice.out" \
  || fail "remove did not report alice's one token as revoked"
[ ! -e "$LATTICE_DEV_HOME/accounts/alice.env" ] || fail "alice.env remains after remove"
ok "remove_deletes_the_env_file_and_revokes_the_token"

expect_ok alice-tokens-after tool account token list /etc/lattice alice
grep -q 'revoked' "$work/alice-tokens-after.out" || fail "alice's token is not listed as revoked"
if grep -q 'active' "$work/alice-tokens-after.out"; then fail "alice still has an active token"; fi
ok "removed_accounts_token_is_revoked"

expect_refused removed-login "$dev" psql alice-stale -X -c "SELECT 1"
grep -q 'FATAL' "$work/removed-login.err" || fail "the removed account's old password was not refused"
rm -f -- "$LATTICE_DEV_HOME/accounts/alice-stale.env"
ok "removed_account_cannot_log_in_with_its_old_password"

expect_ok bob-after-remove "$dev" psql bob -X -q -t -A -c "SELECT id, title, done FROM notes ORDER BY id"
[ "$(cat "$work/bob-after-remove.out")" = "$expected_rows" ] || fail "bob's read changed after alice was removed"
ok "other_accounts_still_read_after_a_remove"

expect_ok readd-alice "$dev" account add alice --read notes --write notes
expect_ok alice-after-readd "$dev" psql alice -X -q -t -A -c "SELECT id FROM notes ORDER BY id"
[ "$(cat "$work/alice-after-readd.out")" = $'1\n2' ] || fail "re-added alice cannot read the two rows"
ok "removed_name_can_be_added_again"

# --- 6b. add while the server runs. alice's session stays open and writes a row through it. A second
# add is refused while that session is open, and changes nothing. When the session ends, the add
# restarts latticed: the acknowledged row survives and the new account can log in.
hold=$work/hold.fifo
mkfifo "$hold"
exec 7<>"$hold"
# 7>&- keeps the write end out of the child: an inherited copy would keep the fifo open and psql would
# never see end of input.
"$dev" psql alice -X -q <"$hold" 7>&- >"$work/held.out" 2>"$work/held.err" &
held_pid=$!
printf "INSERT INTO notes VALUES (3, 'written by a held session', false);\n" >&7
# The session connects in the background, so wait until its write is visible from another connection:
# only then is the session known to be open.
seen=0
for _ in $(seq 1 60); do
  if out=$("$dev" psql bob -X -q -t -A -c "SELECT id FROM notes ORDER BY id" 2>/dev/null) && [ "$out" = $'1\n2\n3' ]; then
    seen=1
    break
  fi
  sleep 1
done
[ "$seen" = 1 ] || fail "the held session's write never became visible"
expect_refused add-carol-while-busy env LATTICE_DEV_DRAIN_SECS=3 "$dev" account add carol --read notes
grep -q 'open connection' "$work/add-carol-while-busy.err" || fail "the busy add was refused for another reason"
[ ! -e "$LATTICE_DEV_HOME/accounts/carol.env" ] || fail "a refused add wrote carol.env"
expect_ok list-after-busy tool account list /etc/lattice
if grep -q '^carol' "$work/list-after-busy.out"; then fail "a refused add changed the server's accounts"; fi
ok "add_is_refused_while_a_session_is_open_and_changes_nothing"

exec 7>&-
# Bounded: a session that does not end after its input closes fails the test instead of hanging the job.
for _ in $(seq 1 60); do
  kill -0 "$held_pid" 2>/dev/null || break
  sleep 1
done
if kill -0 "$held_pid" 2>/dev/null; then
  kill "$held_pid" 2>/dev/null || true
  fail "the held session did not end after its input closed"
fi
if ! wait "$held_pid"; then
  cat "$work/held.err" >&2
  fail "the held session did not finish its write"
fi
expect_ok add-carol "$dev" account add carol --read notes
expect_ok carol-read "$dev" psql carol -X -q -t -A -c "SELECT id FROM notes ORDER BY id"
[ "$(cat "$work/carol-read.out")" = $'1\n2\n3' ] || fail "carol cannot read the three rows"
expect_ok bob-after-held "$dev" psql bob -X -q -t -A -c "SELECT id, title, done FROM notes ORDER BY id"
[ "$(cat "$work/bob-after-held.out")" = "$expected_rows_3" ] || fail "the acknowledged write did not survive the add's restart"
ok "add_waits_for_the_session_then_restarts_and_the_acknowledged_write_survives"

# --- 6c. remove the last accounts. Each remove restarts latticed; removing the last one stops it,
# because latticed cannot start without a verifier file.
for who in carol bob alice; do
  expect_ok "remove-$who" "$dev" account remove "$who"
done
[ -z "$(docker ps -q --filter "label=com.docker.compose.project=$LATTICE_DEV_PROJECT" --filter label=com.docker.compose.service=latticed)" ] \
  || fail "latticed still runs with no account"
ok "removing_the_last_account_stops_latticed"

# --- 6d. the gateway, opt-in. It runs only on LATTICE_DEV_IMAGE, and only when that image's latticed
# serves the gateway over TLS. The pinned default image predates it. The probe reads the image's
# --help: a missing marker is a skip with its reason, never a pass. A usable image that fails is a
# failure.
gateway_ran=0
gateway_reason=""
gateway_probe() {
  local image=$1 help marker
  if ! help=$(docker run --rm "$image" --help 2>/dev/null); then
    gateway_reason="$image could not be pulled or started"
    return 1
  fi
  for marker in "serve the gateway over TLS" "--gateway-allowed-host"; do
    if ! grep -qF -- "$marker" <<<"$help"; then
      gateway_reason="$image does not list '$marker' in latticed --help, so it predates the TLS gateway"
      return 1
    fi
  done
}

# POST /mcp over TLS, verified against the dev CA. NAME names the output: NAME.code holds the HTTP
# status (000 when nothing answered) and NAME.body the reply. AUTH is a header file, or empty for no
# Authorization header. The token stays in that file, never on a command line.
gw_post() {
  local name=$1 auth=$2 body=$3
  local args=(-sS --max-time 10 --cacert "$LATTICE_DEV_HOME/ca.pem" -H "Content-Type: application/json"
    --data "$body" -o "$work/$name.body" -w '%{http_code}')
  if [ -n "$auth" ]; then args+=(-H "@$auth"); fi
  curl "${args[@]}" "$gw_url" >"$work/$name.code" 2>"$work/$name.err" || true
}

gw_status() {
  cat "$work/$1.code"
}

run_gateway_steps() {
  local who tok i published gw_cid plain
  local list_body='{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
  local write_body='{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lattice_write","arguments":{"scope":"default","table":"mcp_kv","key":"greeting","value":"hello from gate-a","expected_version":0}}}'
  local read_body='{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"lattice_read","arguments":{"scope":"default","table":"mcp_kv","key":"greeting"}}}'
  local denied_body='{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"lattice_write","arguments":{"scope":"default","table":"mcp_kv","key":"greeting","value":"written by gate-b","expected_version":1}}}'

  # A free port, the first above the database port's. The gateway is published on it.
  gport=$((port + 1))
  while (exec 3<>"/dev/tcp/127.0.0.1/$gport") 2>/dev/null; do
    gport=$((gport + 1))
  done
  gw_url=https://127.0.0.1:$gport/mcp
  export LATTICE_DEV_IMAGE=$gateway_image

  # gate-a writes and reads mcp_kv, gate-b only reads it, gate-c holds no grant at all.
  expect_ok gw-add-a env LATTICE_DEV_GATEWAY_PORT="$gport" "$dev" account add gate-a --ddl mcp_kv --read mcp_kv --write mcp_kv
  expect_ok gw-add-b env LATTICE_DEV_GATEWAY_PORT="$gport" "$dev" account add gate-b --read mcp_kv
  expect_ok gw-add-c env LATTICE_DEV_GATEWAY_PORT="$gport" "$dev" account add gate-c
  expect_ok gw-create "$dev" psql gate-a -X -q -v ON_ERROR_STOP=1 \
    -c "CREATE TABLE mcp_kv (key text PRIMARY KEY, value text, version bigint NOT NULL)"
  ok "gateway_accounts_added_and_the_table_created"

  # The bearer token is printed once; it goes into an owner-only header file and is never echoed.
  for who in gate-a gate-b gate-c; do
    expect_ok "gw-token-$who" tool account token create /etc/lattice "$who"
    tok=$(sed -n 's/^token (shown once, only a salted hash is stored): //p' "$work/gw-token-$who.out")
    rm -f -- "$work/gw-token-$who.out"
    [ -n "$tok" ] || fail "lattice account token create printed no token for $who"
    (umask 077 && printf 'Authorization: Bearer %s\n' "$tok" > "$work/$who.auth")
  done
  tok=""
  (umask 077 && printf 'Authorization: Bearer %s\n' "$(openssl rand -hex 32)" > "$work/wrong.auth")
  ok "bearer_tokens_created_once_each"

  # latticed starts the gateway after its SCRAM listener is healthy: poll, bounded, for an answer.
  for i in $(seq 1 30); do
    gw_post gw-probe "" "$list_body"
    if [ "$(gw_status gw-probe)" = 401 ]; then break; fi
    sleep 1
  done
  [ "$(gw_status gw-probe)" = 401 ] || fail "the gateway gave no 401 to a request with no token (status $(gw_status gw-probe))"
  ok "gateway_answers_over_tls_and_refuses_a_request_with_no_token"

  gw_post gw-wrong "$work/wrong.auth" "$list_body"
  [ "$(gw_status gw-wrong)" = 401 ] || fail "a wrong token got status $(gw_status gw-wrong), wanted 401"
  ok "gateway_refuses_a_wrong_token"

  # The TLS port reads plain HTTP as a broken handshake and closes: curl reports no HTTP status at all.
  plain=$(curl -sS --max-time 5 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$gport/mcp" 2>/dev/null || true)
  [ "$plain" = 000 ] || fail "plain http to the gateway got status '$plain', wanted no answer (000)"
  ok "plain_http_to_the_gateway_gets_no_answer"

  gw_post gw-list-a "$work/gate-a.auth" "$list_body"
  [ "$(gw_status gw-list-a)" = 200 ] || fail "tools/list with gate-a's token got status $(gw_status gw-list-a)"
  # The shapes below are the ones docs/mcp-v0.md gives: tools/list names the tools in result.tools; a
  # tool result is one text block holding a JSON envelope; a refused call is a JSON-RPC error whose
  # code is 40301 (tool not granted). The text is parsed, not grepped, so escaping does not matter.
  python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
names = {t["name"] for t in d["result"]["tools"]}
assert {"lattice_read", "lattice_write"} <= names, names
' "$work/gw-list-a.body" || fail "tools/list does not name lattice_read and lattice_write"
  ok "tools_list_with_a_valid_token_names_the_tools"

  gw_post gw-write-a "$work/gate-a.auth" "$write_body"
  [ "$(gw_status gw-write-a)" = 200 ] || fail "lattice_write by gate-a got status $(gw_status gw-write-a)"
  python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
assert "error" not in d, d
t = json.loads(d["result"]["content"][0]["text"])
assert t["tool"] == "lattice_write" and t["applied"] is True, t
' "$work/gw-write-a.body" || fail "gate-a's lattice_write was not applied"
  ok "writer_with_a_grant_writes_a_row_through_the_gateway"

  gw_post gw-read-b "$work/gate-b.auth" "$read_body"
  python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
t = json.loads(d["result"]["content"][0]["text"])
assert t["untrusted"]["rows"][0]["value"] == "hello from gate-a", t
' "$work/gw-read-b.body" || fail "gate-b did not read the row gate-a wrote"
  ok "reader_sees_the_row_another_principal_wrote"

  gw_post gw-deny-b "$work/gate-b.auth" "$denied_body"
  python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
assert d["error"]["code"] == 40301, d
' "$work/gw-deny-b.body" || fail "gate-b without a write grant was not refused with 40301"
  ok "reader_without_a_write_grant_is_refused_the_tool"

  gw_post gw-deny-c "$work/gate-c.auth" "$read_body"
  python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
assert d["error"]["code"] == 40301, d
' "$work/gw-deny-c.body" || fail "gate-c without any grant was not refused with 40301"
  ok "principal_without_grants_is_refused_the_tool"

  gw_cid=$(docker ps -q --filter "label=com.docker.compose.project=$LATTICE_DEV_PROJECT" --filter label=com.docker.compose.service=latticed)
  [ -n "$gw_cid" ] || fail "latticed is not running, so the gateway port cannot be checked"
  published=$(docker port "$gw_cid" 7070) || fail "docker port could not list the gateway port"
  [ "$published" = "127.0.0.1:$gport" ] || fail "the gateway is bound to '$published', wanted 127.0.0.1:$gport only"
  ok "gateway_port_is_bound_to_loopback_only"

  # Removing gate-a restarts latticed, which reloads the token file: gate-a's token must stop working,
  # and gate-b must keep reading.
  expect_ok gw-remove-a env LATTICE_DEV_GATEWAY_PORT="$gport" "$dev" account remove gate-a
  for i in $(seq 1 30); do
    gw_post gw-revoked "$work/gate-a.auth" "$list_body"
    if [ "$(gw_status gw-revoked)" = 401 ]; then break; fi
    sleep 1
  done
  [ "$(gw_status gw-revoked)" = 401 ] || fail "a removed account's token still gets past the gateway"
  gw_post gw-read-b2 "$work/gate-b.auth" "$read_body"
  python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
t = json.loads(d["result"]["content"][0]["text"])
assert t["untrusted"]["rows"][0]["value"] == "hello from gate-a", t
' "$work/gw-read-b2.body" || fail "gate-b lost its read after gate-a was removed"
  ok "removed_accounts_token_stops_working_and_others_still_read"

  # These two removes pass no LATTICE_DEV_GATEWAY_PORT: the state dir remembers the gateway, so the
  # restarts keep it on, and the script says so.
  expect_ok gw-remove-b "$dev" account remove gate-b
  grep -q 'the gateway stays on' "$work/gw-remove-b.err" || fail "a remembered gateway was not reported"
  expect_ok gw-remove-c "$dev" account remove gate-c
  [ -z "$(docker ps -q --filter "label=com.docker.compose.project=$LATTICE_DEV_PROJECT" --filter label=com.docker.compose.service=latticed)" ] \
    || fail "latticed still runs with no account"
  rm -f -- "$work"/*.auth
  ok "gateway_setting_is_remembered_and_latticed_stopped_with_the_last_account"

  # LATTICE_DEV_GATEWAY_PORT=0 turns the gateway off and forgets the port.
  expect_ok gw-off env LATTICE_DEV_GATEWAY_PORT=0 "$dev" up
  [ ! -e "$LATTICE_DEV_HOME/gateway.port" ] || fail "gateway.port remains after LATTICE_DEV_GATEWAY_PORT=0"
  ok "gateway_turned_off_and_forgotten"

  unset LATTICE_DEV_IMAGE
  gateway_ran=1
}

if [ -z "$gateway_image" ]; then
  echo "skip    gateway: LATTICE_DEV_IMAGE is not set, and the default image predates the TLS gateway"
elif ! gateway_probe "$gateway_image"; then
  echo "skip    gateway: $gateway_reason; step 6d did not run"
else
  run_gateway_steps
fi

# --- 7. reset asks first; a no leaves everything in place
printf 'no\n' >"$work/answer-no"
expect_refused reset-declined "$dev" reset <"$work/answer-no"
[ -f "$LATTICE_DEV_HOME/ca.pem" ] || fail "a declined reset deleted the CA"
ok "reset_without_yes_is_refused_and_keeps_the_data"

# --- 7b. a rename that fails must not leave the password in a temp file. A fake mv on the PATH makes
# the rename fail; the account is created on the server first, so the test's reset removes it too.
mkdir -p "$work/fakebin"
printf '#!/bin/sh\nexit 1\n' > "$work/fakebin/mv"
chmod 0755 "$work/fakebin/mv"
expect_refused rename-fails env PATH="$work/fakebin:$PATH" "$dev" account add carol --read notes
for leftover in "$LATTICE_DEV_HOME"/accounts/.carol.*; do
  [ -e "$leftover" ] && fail "a temp file with the password was left behind by a failed rename"
done
[ ! -e "$LATTICE_DEV_HOME/accounts/carol.env" ] || fail "carol.env exists after a failed rename"
ok "failed_rename_removes_the_temp_file_with_the_password"

# --- 8. reset, answered yes at the prompt, removes what the test created (cleanup uses --yes)
printf 'yes\n' >"$work/answer-yes"
expect_ok reset "$dev" reset <"$work/answer-yes"
[ -z "$(project_containers)" ] || fail "containers remain after reset"
[ -z "$(project_volumes)" ] || fail "volumes remain after reset"
[ ! -e "$LATTICE_DEV_HOME" ] || [ -z "$(ls -A "$LATTICE_DEV_HOME")" ] || fail "state dir still holds files"
ok "reset_removes_containers_volumes_and_state"

if [ "$gateway_ran" != 1 ]; then
  echo "SKIPPED gateway steps: see the skip line above"
fi
echo "dev stack test passed"
