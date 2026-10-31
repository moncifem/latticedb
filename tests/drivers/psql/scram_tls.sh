#!/usr/bin/env bash
# SCRAM-SHA-256 over TLS with sslmode=verify-full, against a built latticed (issue 575).
#
#   tests/drivers/psql/scram_tls.sh <path-to-latticed> [work-dir]
#
# Builds a throwaway CA, a server certificate for localhost and 127.0.0.1, a decoy secret and one
# account verifier with openssl and python3 (PBKDF2 and HMAC from the standard library, following
# RFC 5802 and RFC 7677, not latticed's own code), starts latticed with --auth scram-tls and checks:
#   - the right password connects with verify-full and runs SQL (expected output in scram_tls.expected)
#   - a wrong password and an unknown account are refused with the same libpq message (FATAL,
#     password authentication failed, which is how libpq shows SQLSTATE 28P01)
#   - verify-full refuses an untrusted CA and a host name the certificate does not carry
#   - sslmode=disable (no TLS) is refused
# psql prints the SQLSTATE of a query error but not of a connection failure, so the 28P01 of the
# refused login is checked through libpq's message text, not through the code.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck disable=SC2034  # used by lib.sh
bin=${1:?usage: scram_tls.sh <path-to-latticed> [work-dir]}
bin=$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")  # absolute: the scripts change directory
psql_bin=${PSQL:-psql}
password='correct horse battery staple'

# shellcheck source=lib.sh
. "$here/lib.sh"

pick_port
if [ -n "${2:-}" ]; then
  work=$2
else
  work=$(mktemp -d)
  own_dirs="$own_dirs $work"
fi
dir="$work/scram-tls"

command -v openssl >/dev/null || { echo "openssl not found" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 not found" >&2; exit 1; }
rm -rf "$dir"
mkdir -p "$dir"
# latticed refuses a secret directory that group or others can write, whatever the umask is.
chmod 700 "$dir"
cd "$dir"

# --- certificates: CA and a server certificate with SAN localhost and 127.0.0.1
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out ca-key.pem 2>/dev/null
openssl req -x509 -new -key ca-key.pem -subj "/CN=lattice psql test CA" -days 2 -out ca.pem
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out server-key.pem 2>/dev/null
openssl req -new -key server-key.pem -subj "/CN=localhost" -out server.csr
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial -days 2 \
  -extfile server.ext -out server-cert.pem 2>/dev/null
# a second CA that did not sign the server certificate
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out other-key.pem 2>/dev/null
openssl req -x509 -new -key other-key.pem -subj "/CN=other CA" -days 2 -out other-ca.pem

# --- account verifier (RFC 5802 / 7677) and decoy secret
LATTICE_TEST_PASSWORD="$password" python3 -I - > accounts.txt <<'PY'
import base64, hashlib, hmac, os
password = os.environ["LATTICE_TEST_PASSWORD"].encode()
salt = os.urandom(16)
iterations = 4096
salted = hashlib.pbkdf2_hmac("sha256", password, salt, iterations)
client_key = hmac.new(salted, b"Client Key", hashlib.sha256).digest()
stored_key = hashlib.sha256(client_key).digest()
server_key = hmac.new(salted, b"Server Key", hashlib.sha256).digest()
b64 = lambda b: base64.b64encode(b).decode()
print(f"lattice\tSCRAM-SHA-256${iterations}:{b64(salt)}${b64(stored_key)}:{b64(server_key)}")
PY
head -c 32 /dev/urandom > decoy.bin
printf 'lattice\tallow-all\n' > policies.txt
chmod 600 server-key.pem decoy.bin accounts.txt policies.txt

start_server "$dir" scram --auth scram-tls --listen "127.0.0.1:$port" \
  --tls-cert "$dir/server-cert.pem" --tls-key "$dir/server-key.pem" \
  --scram-verifiers "$dir/accounts.txt" --scram-decoy-secret "$dir/decoy.bin" \
  --account-policies "$dir/policies.txt"

failed=0
conninfo() { # user rootcert host
  echo "host=$3 hostaddr=127.0.0.1 port=$port user=$1 dbname=lattice sslmode=verify-full sslrootcert=$2 connect_timeout=10"
}
psql_scram() { # user password conninfo-args... -- psql args
  local user=$1 pw=$2 root=$3 host=$4; shift 4
  PGPASSWORD="$pw" PGPASSFILE="$dir/no-pgpass" "$psql_bin" -X -A -P null='[null]' -v VERBOSITY=sqlstate \
    "$(conninfo "$user" "$root" "$host")" "$@"
}

# 1. right password, verify-full: SQL works and the output is the expected one.
psql_scram lattice "$password" "$dir/ca.pem" localhost < "$here/scram_tls.sql" > "$dir/scram_tls.actual" 2>&1 || true
if diff -u "$here/scram_tls.expected" "$dir/scram_tls.actual"; then
  echo "ok      psql_scram_tls_verify_full_succeeds"
else
  echo "FAILED  psql_scram_tls_verify_full_succeeds" >&2; failed=$((failed + 1))
fi

# expect_refused <name> <message fragment> <user> <password> <root cert> <host>
expect_refused() {
  local name=$1 fragment=$2; shift 2
  local out status=0
  out=$(psql_scram "$@" -c 'SELECT 1' 2>&1) || status=$?
  if [ "$status" -eq 2 ] && printf '%s' "$out" | grep -qF -- "$fragment" && ! printf '%s' "$out" | grep -q '^1$'; then
    echo "ok      $name"
  else
    echo "FAILED  $name (psql status $status, wanted 2 and '$fragment'): $out" >&2
    failed=$((failed + 1))
  fi
}
expect_refused psql_scram_tls_wrong_password_fails_with_28P01 \
  'FATAL:  password authentication failed' lattice "not the password" "$dir/ca.pem" localhost
expect_refused psql_scram_tls_unknown_account_fails_like_a_wrong_password \
  'FATAL:  password authentication failed' nobody "$password" "$dir/ca.pem" localhost
expect_refused psql_scram_tls_verify_full_refuses_an_untrusted_ca \
  'certificate verify failed' lattice "$password" "$dir/other-ca.pem" localhost
expect_refused psql_scram_tls_verify_full_refuses_a_wrong_host_name \
  'does not match host name "wrong.example"' \
  lattice "$password" "$dir/ca.pem" wrong.example

# 5. no TLS: the server must not accept SCRAM over plaintext, and the reason must be about TLS.
status=0
out=$(PGPASSWORD="$password" PGPASSFILE="$dir/no-pgpass" "$psql_bin" -X -A \
  "host=localhost hostaddr=127.0.0.1 port=$port user=lattice dbname=lattice sslmode=disable connect_timeout=10" \
  -c 'SELECT 1' 2>&1) || status=$?
if [ "$status" -eq 2 ] && printf '%s' "$out" | grep -qF 'FATAL:  TLS required' && ! printf '%s' "$out" | grep -q '^1$'; then
  echo "ok      psql_scram_tls_refuses_plaintext"
else
  echo "FAILED  psql_scram_tls_refuses_plaintext (status $status): $out" >&2; failed=$((failed + 1))
fi

# 6. The refusal must not tell the two cases apart.
wrong=$(psql_scram lattice "not the password" "$dir/ca.pem" localhost -c 'SELECT 1' 2>&1 || true)
unknown=$(psql_scram nobody "$password" "$dir/ca.pem" localhost -c 'SELECT 1' 2>&1 || true)
if [ -n "$wrong" ] && [ "$wrong" = "$unknown" ]; then
  echo "ok      psql_scram_tls_unknown_account_message_equals_wrong_password_message"
else
  echo "FAILED  refusal messages differ: [$wrong] [$unknown]" >&2; failed=$((failed + 1))
fi

stop_server || failed=$((failed + 1))
echo "scram-tls: $failed failed"
[ "$failed" -eq 0 ]
