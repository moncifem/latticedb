#!/usr/bin/env bash
# Downloads the pgjdbc jar named in pom.xml into jdbc/lib and checks it against pgjdbc.sha256
# (issue 607). The hash is the one of the file on Maven Central; a mismatch deletes the download.
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
version=$(python3 -I -c '
import re, sys
text = open(sys.argv[1], encoding="utf-8").read()
m = re.search(r"<artifactId>postgresql</artifactId>\s*<version>([0-9][0-9.]*)</version>", text)
print(m.group(1) if m else "")
' "$here/pom.xml")
[ -n "$version" ] || { echo "no pgjdbc version in pom.xml" >&2; exit 1; }
jar="postgresql-$version.jar"
mkdir -p "$here/lib"
curl --fail --silent --show-error --location --retry 3 -o "$here/lib/$jar" \
  "https://repo1.maven.org/maven2/org/postgresql/postgresql/$version/$jar"
if ! (cd "$here/lib" && grep -F "  $jar" "$here/pgjdbc.sha256" | sha256sum --check --strict - >&2); then
  rm -f "$here/lib/$jar"
  echo "pgjdbc $version does not match pgjdbc.sha256" >&2
  exit 1
fi
echo "$here/lib/$jar"
