#!/usr/bin/env bash
# Build an .apk for the pg_ask extension out of `cargo pgrx package`'s
# staging tree. The Alpine/musl counterpart of build-deb.sh / build-rpm.sh.
# Produces (under ~/packages or the abuild repodest):
#
#   pg_ask18-${VERSION}-r0.apk
#
# Designed to be called from .github/workflows/apk.yml inside an
# alpine:edge container (PostgreSQL 18 dev packages currently only
# exist in edge, not in the 3.20–3.22 stable branches). Assumes:
#
#   * postgresql18-dev is installed (pg_config at /usr/bin, system
#     extension paths /usr/lib/postgresql18 + /usr/share/postgresql18)
#   * Rust + cargo + cargo-pgrx are on PATH (workflow handles this)
#   * abuild + abuild signing key are configured (workflow handles this)
#
# Env (workflow sets all of them):
#
#   VERSION    upstream version, e.g. 0.5.6
#   PG_MAJOR   PostgreSQL major, e.g. 18
#
# abuild wants to own the whole build; we instead pre-build with pgrx and
# have APKBUILD's package() just copy the staged tree into $pkgdir. This
# keeps the build path identical across deb/rpm/apk (one pgrx invocation,
# three packagers consuming the same staging tree).
#
# Package name follows Alpine's postgresqlNN-* family convention:
# pg_ask${PG_MAJOR} (e.g. pg_ask18), matching how Alpine names
# per-major postgres extension packages.

set -euo pipefail

: "${VERSION:?VERSION env required (e.g. 0.5.6)}"
: "${PG_MAJOR:?PG_MAJOR env required (e.g. 18)}"

PG_CONFIG="/usr/bin/pg_config"
[[ -x "$PG_CONFIG" ]] || { echo "missing $PG_CONFIG — install postgresql${PG_MAJOR}-dev"; exit 1; }

ACTUAL_MAJOR=$("$PG_CONFIG" --version | grep -oE '[0-9]+' | head -1)
[[ "$ACTUAL_MAJOR" == "$PG_MAJOR" ]] || {
    echo "::error::pg_config reports PG${ACTUAL_MAJOR} but PG_MAJOR=${PG_MAJOR}"; exit 1; }

SHAREDIR=$("$PG_CONFIG" --sharedir)       # e.g. /usr/share/postgresql18
EXTDIR="$SHAREDIR/extension"

PKG_NAME="pg_ask${PG_MAJOR}"
PKG_REL=0                                  # -r0 (bump only for repackages)

# ---------------------------------------------------------------------
# 1. Build the extension into a clean staging dir (same step as the deb
#    and rpm builds). pgrx stages a destination-mirroring tree under
#    target/release/pg_ask-pgN/, e.g.:
#
#      usr/lib/postgresql18/pg_ask.so
#      usr/share/postgresql18/extension/pg_ask.control
#      usr/share/postgresql18/extension/pg_ask--*.sql
# ---------------------------------------------------------------------
STAGED="target/release/pg_ask-pg${PG_MAJOR}"
if [[ "${SKIP_PGRX_PACKAGE:-0}" == "1" && -d "$STAGED" ]]; then
    # Reuse an already-staged tree (e.g. when the pgrx build ran in a
    # previous step or a cached layer). Lets the packaging step run in a
    # cargo-less container.
    echo "==> SKIP_PGRX_PACKAGE=1, reusing existing $STAGED"
else
    echo "==> cargo pgrx package (pg${PG_MAJOR}, musl)"
    rm -rf "$STAGED"
    cargo pgrx package \
        --no-default-features --features "pg${PG_MAJOR}" \
        --pg-config "$PG_CONFIG"
fi

[[ -d "$STAGED" ]] || { echo "expected $STAGED after pgrx package"; exit 1; }

# Bundle every handwritten upgrade script (pgrx only stages the current
# version's files). Same glob approach as the deb/rpm builds.
shopt -s nullglob
upgrade_scripts=(sql/pg_ask--*--*.sql)
shopt -u nullglob
for u in "${upgrade_scripts[@]}"; do
    cp "$u" "${STAGED}${EXTDIR}/"
done

# ---------------------------------------------------------------------
# 2. Lay out an abuild workspace. We generate an APKBUILD whose
#    package() simply copies our pre-staged tree into $pkgdir — no
#    build/check phases (the pgrx build already happened above).
# ---------------------------------------------------------------------
WORK="$(pwd)/dist/apkbuild"
rm -rf "$WORK"
mkdir -p "$WORK"

# abuild copies $pkgdir contents verbatim into the .apk, so we hand it an
# absolute path to our staged tree via a build-time variable.
STAGED_ABS="$(pwd)/${STAGED}"
DOC_SRC="$(pwd)"

cat > "$WORK/APKBUILD" <<EOF
# Maintainer: Sentirum <packaging@sentirum.ai>
pkgname=${PKG_NAME}
pkgver=${VERSION}
pkgrel=${PKG_REL}
pkgdesc="Ask your PostgreSQL database in natural language (PG${PG_MAJOR} extension)"
url="https://github.com/sentirum/pg_ask"
arch="x86_64 aarch64"
license="PostgreSQL"
depends="postgresql${PG_MAJOR}"
# The build already ran (pgrx) before abuild is invoked; we only package.
options="!check !strip !fhs"
source=""

package() {
	# abuild normally creates \$pkgdir during the build phase, but with an
	# empty source="" (we pre-build with pgrx, no tarball) that path can
	# be skipped — so ensure it exists before copying into it.
	mkdir -p "\$pkgdir"

	# Copy the pgrx-staged, destination-mirroring tree into \$pkgdir.
	cp -a "${STAGED_ABS}/usr" "\$pkgdir/"

	# License + docs into the conventional Alpine locations.
	install -Dm644 "${DOC_SRC}/LICENSE" \\
		"\$pkgdir/usr/share/licenses/${PKG_NAME}/LICENSE"
	install -Dm644 "${DOC_SRC}/CHANGELOG.md" \\
		"\$pkgdir/usr/share/doc/${PKG_NAME}/CHANGELOG.md"
}
EOF

echo "==> generated APKBUILD:"
cat "$WORK/APKBUILD"

# ---------------------------------------------------------------------
# 3. abuild. We disable the checksum step (no source tarball) and point
#    abuild at our workspace. abuild signs the .apk with the configured
#    key and drops it under $REPODEST (default ~/packages/<repo>/<arch>).
#    Cloudsmith re-signs the repository INDEX on push, but a valid
#    package signature is still required for the .apk to be accepted.
# ---------------------------------------------------------------------
echo "==> abuild -r (package only)"
cd "$WORK"
# -F: allow running as root (CI); -d: don't try to install missing
# makedepends (we manage deps in the workflow); -P: repodest.
export REPODEST="$(pwd)/repo"
abuild -F -d -P "$REPODEST" rootpkg

BUILT=$(find "$REPODEST" -name "${PKG_NAME}-${VERSION}-r${PKG_REL}.apk" | head -1)
[[ -n "$BUILT" ]] || { echo "::error::abuild produced no .apk"; exit 1; }

OUTDIR="$(cd "$DOC_SRC" && pwd)/dist"
mkdir -p "$OUTDIR"
cp "$BUILT" "$OUTDIR/"
OUT="$OUTDIR/$(basename "$BUILT")"

echo
echo "==> built $OUT"
echo "==> contents:"
tar -tzf "$OUT" 2>/dev/null | grep -vE '^\.(SIGN|PKGINFO|pre|post)' | head -20 || true
