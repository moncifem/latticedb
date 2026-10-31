# Getting started

The commands on this page are the ones in the [alpha guide](../alpha.md), the [README](../../README.md)
and the [listener guide](../../crates/latticed/README.md). Read the alpha guide first: this is test
data only, and nothing here is a phase-gate claim.

Platforms: macOS arm64, Linux x86_64 and Linux arm64. Windows is not supported.

## 1. Run a loopback server from source

You need Rust. `rustup` installs the toolchain pinned in `rust-toolchain.toml`. In a checkout of the
repository:

```bash
cargo run -p latticed -- --data-dir ./data --development-policy allow-all
```

It listens on `127.0.0.1:5432` and trusts every connection. The `--development-policy allow-all` flag is
required and is never implied. Stop it with Ctrl-C, or type `shutdown` on stdin. To run it detached, add
`--stdin-control off`.

## 2. Connect with psql and try SQL

```bash
psql -h 127.0.0.1 -p 5432 -U dev
```

```sql
CREATE TABLE notes (id INT PRIMARY KEY, title TEXT NOT NULL, done BOOLEAN);
INSERT INTO notes VALUES (1, 'write the spec', false);
SELECT * FROM notes;
```

After a restart on the same `--data-dir`, the tables, their columns and constraints and the committed rows
are recovered. A data directory written by a build that stored only table names refuses its first
`CREATE TABLE` until you start with `--allow-format-upgrade on`. Older builds cannot open that directory
afterwards, so take a backup first. A new directory needs nothing. The SQL that is accepted is described on
the [SQL subset](sql.md) page.

## 3. Install an archive

The pre-release `v0.1.0-alpha.1` is published. Fetch the install script from the tag, read it, then run it:

```bash
curl -fsSL -o install.sh https://raw.githubusercontent.com/moncifem/latticedb/v0.1.0-alpha.1/scripts/install.sh
sh install.sh 0.1.0-alpha.1
```

The script checks the archive against the release `SHA256SUMS`, verifies the build attestation when the
`gh` CLI is installed, and puts `lattice` and `latticed` in `~/.local/bin`. It also accepts `nightly`
as a version, which installs the nightly pre-release. Those archives may predate the latest source. See
[releasing](../releasing.md#installing-a-release).

## 4. SCRAM-SHA-256 over TLS (for anything reachable from another host)

Loopback trust mode refuses a non-loopback bind. For any other host, provision accounts with the
`lattice account` tool. It edits files and does not talk to a running server:

```bash
lattice account init /secure/lattice          # decoy secret, empty policy and token files; prints the flags
printf '%s\n' "$PASSWORD" | lattice account add /secure/lattice alice --read notes --write notes --password-stdin
lattice account list /secure/lattice
```

`--read`, `--write` and `--ddl` take table names, and an account with no grants is denied everything. The
password comes from stdin or is generated and printed once. The tool does not make TLS keys, so create the
certificate with `openssl` (the [alpha guide](../alpha.md#run-latticed) has a pinned example). Then start
the server with the flags the tool prints, plus your data directory and the certificate:

```bash
latticed --auth scram-tls --data-dir ./data --account-policies policies.txt \
  --tls-cert server-chain.pem --tls-key server-key.pem \
  --scram-verifiers accounts.txt --scram-decoy-secret decoy-secret.bin
```

Clients must verify the certificate. Use libpq `sslmode=verify-full` with your CA:

```bash
PGPASSWORD="$PASSWORD" psql "host=localhost port=5432 user=alice dbname=lattice sslmode=verify-full sslrootcert=ca.pem"
```

Channel binding is not implemented ([#360](https://github.com/moncifem/latticedb/issues/360)). The TLS key,
verifier, decoy-secret and policy files must each be owner-only (mode 0600 or 0400), in a directory that
group and others cannot write to; a symlink is refused, and Windows refuses these files. The [listener guide](../../crates/latticed/README.md#provisioning-with-lattice-account)
covers token files, rotation and the other flags.

## 5. Container image

The image `ghcr.io/moncifem/latticedb` holds `latticed` and `lattice` on Ubuntu 24.04. It runs as uid
10001 and keeps data in `/var/lib/lattice`. With no arguments it prints the flags it needs and exits 2:

```bash
docker run --rm ghcr.io/moncifem/latticedb:nightly
```

A published port needs SCRAM over TLS. The full sequence from the alpha guide creates the account and
certificate volumes, starts the server and connects:

```bash
IMAGE=ghcr.io/moncifem/latticedb:nightly
docker volume create lattice-config && docker volume create lattice-tls
docker run --rm --entrypoint lattice -v lattice-config:/etc/lattice "$IMAGE" account init /etc/lattice
printf '%s\n' "$PASSWORD" | docker run --rm -i --entrypoint lattice -v lattice-config:/etc/lattice "$IMAGE" \
  account add /etc/lattice alice --read notes --write notes --ddl notes --password-stdin

# a throwaway CA and a server certificate for localhost (use your own names and a real CA otherwise)
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca-key.pem -out ca.pem -days 30 -subj "/CN=lattice test CA"
openssl req -newkey rsa:2048 -nodes -keyout server-key.pem -out server.csr -subj "/CN=localhost"
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial -days 30 \
  -extfile server.ext -out server-cert.pem

# the key must be owned by uid 10001 and mode 0600, so copy it in as root with install
for f in server-key.pem:0600 server-cert.pem:0644; do
  name=${f%%:*}; mode=${f##*:}
  docker run --rm --user 0:0 --entrypoint install -v lattice-tls:/etc/lattice-tls -v "$PWD:/src:ro" "$IMAGE" \
    -o 10001 -g 10001 -m "$mode" "/src/$name" "/etc/lattice-tls/$name"
done

docker run -d --name lattice -p 127.0.0.1:5432:5432 \
  -v lattice-data:/var/lib/lattice -v lattice-config:/etc/lattice:ro -v lattice-tls:/etc/lattice-tls:ro \
  "$IMAGE" --data-dir /var/lib/lattice --auth scram-tls --listen 0.0.0.0:5432 \
  --tls-cert /etc/lattice-tls/server-cert.pem --tls-key /etc/lattice-tls/server-key.pem \
  --scram-verifiers /etc/lattice/scram-verifiers --scram-decoy-secret /etc/lattice/scram-decoy-secret \
  --account-policies /etc/lattice/account-policies
PGPASSWORD="$PASSWORD" psql "host=localhost port=5432 user=alice dbname=lattice sslmode=verify-full sslrootcert=ca.pem"
```

`docker stop` is a clean stop once the server has printed its listening line. Wait for that line before you
stop it. A `docker start lattice` keeps the same volumes and rows. The
[alpha guide](../alpha.md#run-in-a-container) explains the ownership rules for bind mounts, what the CI
image smoke test covers and what it does not. Until the next nightly build, the `nightly` tag may not yet
include the entrypoint described there.

## Report a problem

Bugs and questions go to [issues](https://github.com/moncifem/latticedb/issues) and
[Discussions](https://github.com/moncifem/latticedb/discussions). Vulnerabilities go to the private
channel in [SECURITY.md](../../SECURITY.md), never to a public issue.
