#!/usr/bin/env bash
# Build a .deb for the pg_ask extension out of `cargo pgrx package`'s
# staging tree. Produces:
#
#   dist/postgresql-${PG_MAJOR}-pg-ask_${VERSION}-1+${DISTRO}_${ARCH}.deb
#
# Designed to be called from .github/workflows/apt.yml inside a Debian
# / Ubuntu container; assumes:
#
#   * postgresql-server-dev-${PG_MAJOR} is installed (provides
#     pg_config + the system extension paths we install into)
#   * Rust stable + cargo-pgrx are on PATH (the workflow handles this)
#
# Env (workflow sets all of them):
#
#   VERSION    upstream version, e.g. 0.5.3
#   PG_MAJOR   PostgreSQL major, e.g. 18
#   DISTRO     debian codename, e.g. bookworm / trixie / jammy / noble
#   ARCH       debian arch,     e.g. amd64 / arm64
#
# The resulting package name follows the PGDG convention
# (postgresql-${PG_MAJOR}-${shortname}) so distros that have an
# existing postgresql-${PG_MAJOR}-* package family slot it in
# naturally. The `+${DISTRO}` revision suffix lets us host multiple
# distros in the same reprepro tree without name collisions.

set -euo pipefail

: "${VERSION:?VERSION env required (e.g. 0.5.3)}"
: "${PG_MAJOR:?PG_MAJOR env required (e.g. 18)}"
: "${DISTRO:?DISTRO env required (e.g. bookworm)}"
: "${ARCH:?ARCH env required (e.g. amd64)}"

PG_CONFIG="/usr/lib/postgresql/${PG_MAJOR}/bin/pg_config"
[[ -x "$PG_CONFIG" ]] || { echo "missing $PG_CONFIG — install postgresql-server-dev-${PG_MAJOR}"; exit 1; }

PKGLIBDIR=$("$PG_CONFIG" --pkglibdir)     # e.g. /usr/lib/postgresql/18/lib
SHAREDIR=$("$PG_CONFIG"  --sharedir)      # e.g. /usr/share/postgresql/18
EXTDIR="$SHAREDIR/extension"

PKG_NAME="postgresql-${PG_MAJOR}-pg-ask"
PKG_VERSION="${VERSION}-1+${DISTRO}"
WORK="$(pwd)/dist/work-${ARCH}"
DEB_ROOT="${WORK}/${PKG_NAME}_${PKG_VERSION}_${ARCH}"

# ---------------------------------------------------------------------
# 1. Build the extension into a clean staging dir.
#
# `cargo pgrx package` runs cargo build --release and stages the
# artefacts under target/release/pg_ask-pg${PG_MAJOR}/ with a layout
# that mirrors the destination filesystem:
#
#   target/release/pg_ask-pgN/
#     usr/lib/postgresql/N/lib/pg_ask.so
#     usr/share/postgresql/N/extension/pg_ask.control
#     usr/share/postgresql/N/extension/pg_ask--*.sql
#
# Perfect for a deb that ships exactly those paths.
# ---------------------------------------------------------------------
echo "==> cargo pgrx package (pg${PG_MAJOR}, ${ARCH})"
rm -rf target/release/pg_ask-pg${PG_MAJOR}
cargo pgrx package \
    --no-default-features --features "pg${PG_MAJOR}" \
    --pg-config "$PG_CONFIG"

STAGED="target/release/pg_ask-pg${PG_MAJOR}"
[[ -d "$STAGED" ]] || { echo "expected $STAGED after pgrx package"; exit 1; }

# ---------------------------------------------------------------------
# 2. Build the .deb tree by copying the staged files under DEB_ROOT
#    and adding DEBIAN/{control,postinst,prerm,copyright}.
# ---------------------------------------------------------------------
echo "==> assemble ${PKG_NAME}_${PKG_VERSION}_${ARCH}.deb"
rm -rf "$DEB_ROOT"
mkdir -p "$DEB_ROOT/DEBIAN"
mkdir -p "$DEB_ROOT/usr/share/doc/${PKG_NAME}"

# Mirror /usr from the staging tree. cp -a preserves modes/symlinks.
cp -a "${STAGED}/usr" "$DEB_ROOT/"

# We also bundle every handwritten upgrade script so an operator on any
# older version can step through with `ALTER EXTENSION pg_ask UPDATE`.
# pgrx-package only stages the current-version files. Globbing (rather
# than a hardcoded list) keeps this correct as new upgrade paths land --
# the old list referenced a nonexistent 0.5.2--0.5.3 file and omitted
# 0.5.3--0.5.4 onward.
shopt -s nullglob
upgrade_scripts=(sql/pg_ask--*--*.sql)
shopt -u nullglob
if [[ ${#upgrade_scripts[@]} -eq 0 ]]; then
    echo "WARNING: no upgrade scripts found under sql/" >&2
fi
for u in "${upgrade_scripts[@]}"; do
    cp "$u" "$DEB_ROOT/$EXTDIR/"
done

# Installed-size in KiB (dpkg-deb expects an integer in the control
# file). Approximation — close enough for apt's display purposes.
INSTALLED_SIZE=$(du -sk "$DEB_ROOT/usr" | cut -f1)

cat > "$DEB_ROOT/DEBIAN/control" <<EOF
Package: ${PKG_NAME}
Version: ${PKG_VERSION}
Section: database
Priority: optional
Architecture: ${ARCH}
Installed-Size: ${INSTALLED_SIZE}
Depends: postgresql-${PG_MAJOR}, libc6, libssl3 | libssl3t64
Maintainer: Sentirum APT <apt@sentirum.ai>
Homepage: https://github.com/sentirum/pg_ask
Description: Ask your PostgreSQL database in natural language
 pg_ask is a PostgreSQL extension that lets you query your database
 in plain English (or any language). It bundles a built-in tool-using
 LLM agent — schema-aware, with safe SQL execution, memory, and
 multi-turn chat sessions — all driven from inside the database.
 .
 This package installs the extension for PostgreSQL ${PG_MAJOR}.
 Enable it per-database with: CREATE EXTENSION pg_ask;
EOF

# Postinst: print the next-steps banner. We don't auto-create the
# extension because pg_ask needs a provider/api_key config; the user
# must do that explicitly.
cat > "$DEB_ROOT/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e

if [ "$1" = "configure" ]; then
    cat <<'BANNER'

  pg_ask installed for PostgreSQL. To enable it:

    sudo -u postgres psql -c "CREATE EXTENSION pg_ask;"

  Then configure a provider and API key (example: Anthropic):

    sudo -u postgres psql -c "SELECT ask.config('provider', 'anthropic');"
    sudo -u postgres psql -c "SELECT ask.config('api_key',  'sk-ant-...');"
    sudo -u postgres psql -c "SELECT ask.ask('how many tables are in this database?');"

  Documentation: https://github.com/sentirum/pg_ask
  Security:      https://github.com/sentirum/pg_ask/blob/main/docs/SECURITY.md

BANNER
fi

exit 0
EOF
chmod 0755 "$DEB_ROOT/DEBIAN/postinst"

# Prerm: warn (but don't block) if the extension is still in use.
cat > "$DEB_ROOT/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e

if [ "$1" = "remove" ] || [ "$1" = "upgrade" ]; then
    cat <<'BANNER'
  pg_ask: if any database still has the extension enabled, run
    sudo -u postgres psql -d <dbname> -c "DROP EXTENSION pg_ask CASCADE;"
  before removing this package.
BANNER
fi

exit 0
EOF
chmod 0755 "$DEB_ROOT/DEBIAN/prerm"

cp LICENSE     "$DEB_ROOT/usr/share/doc/${PKG_NAME}/copyright"
cp CHANGELOG.md "$DEB_ROOT/usr/share/doc/${PKG_NAME}/changelog.md"
gzip -9 -n "$DEB_ROOT/usr/share/doc/${PKG_NAME}/changelog.md"

# ---------------------------------------------------------------------
# 3. Actually build the .deb. fakeroot lets dpkg-deb set 0:0 ownership
#    even though we're running as a non-root CI user.
# ---------------------------------------------------------------------
mkdir -p dist
DEB="dist/${PKG_NAME}_${PKG_VERSION}_${ARCH}.deb"
echo "==> dpkg-deb --build → $DEB"
fakeroot dpkg-deb --build --root-owner-group "$DEB_ROOT" "$DEB"

echo
echo "==> built $DEB"
dpkg-deb -I "$DEB"
echo
echo "==> contents:"
# Capture the full listing first, THEN truncate. Piping dpkg-deb
# straight into `head` makes head close the pipe early; dpkg-deb then
# dies with SIGPIPE ("tar subprocess was killed by signal (Broken
# pipe)") and `set -o pipefail` turns that into a build failure. Whether
# it triggers is a race on the 64KiB pipe buffer, which is why only some
# matrix legs failed. Buffering the output sidesteps it entirely.
contents="$(dpkg-deb -c "$DEB")"
printf '%s\n' "$contents" | head -20
