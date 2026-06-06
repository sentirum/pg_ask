#!/usr/bin/env bash
# Build an .rpm for the pg_ask extension out of `cargo pgrx package`'s
# staging tree. The RPM counterpart of packaging/debian/build-deb.sh.
# Produces:
#
#   dist/pg_ask_${PG_MAJOR}-${VERSION}-1.${DIST}.${RPMARCH}.rpm
#
# Designed to be called from .github/workflows/rpm.yml inside a RHEL-
# family container (Rocky / AlmaLinux / Fedora); assumes:
#
#   * the PGDG `postgresql${PG_MAJOR}-devel` package is installed
#     (provides pg_config at /usr/pgsql-${PG_MAJOR}/bin and the system
#     extension paths we install into)
#   * Rust stable + cargo-pgrx are on PATH (the workflow handles this)
#   * rpmbuild + rpmdevtools are installed
#
# Env (workflow sets all of them):
#
#   VERSION    upstream version, e.g. 0.5.6
#   PG_MAJOR   PostgreSQL major, e.g. 18
#   DIST       rpm dist tag,  e.g. el9 / el8 / fc40
#   ARCH       package arch,  e.g. amd64 / arm64 (debian-style; we map
#              to the rpm arch x86_64 / aarch64 below)
#
# The resulting package name follows the PGDG yum convention
# (<extname>_<pgmajor>, e.g. pg_ask_18 — same shape as pgvector_18 /
# pg_cron_18) so it slots naturally next to the postgresqlNN-* family.

set -euo pipefail

: "${VERSION:?VERSION env required (e.g. 0.5.6)}"
: "${PG_MAJOR:?PG_MAJOR env required (e.g. 18)}"
: "${DIST:?DIST env required (e.g. el9)}"
: "${ARCH:?ARCH env required (e.g. amd64)}"

# Map debian-style arch (what the workflow matrix uses, kept in lockstep
# with apt.yml) to the rpm arch the toolchain expects.
case "$ARCH" in
    amd64) RPMARCH=x86_64 ;;
    arm64) RPMARCH=aarch64 ;;
    *) echo "::error::unknown ARCH '$ARCH' (expected amd64/arm64)"; exit 1 ;;
esac

PG_CONFIG="/usr/pgsql-${PG_MAJOR}/bin/pg_config"
[[ -x "$PG_CONFIG" ]] || { echo "missing $PG_CONFIG — install postgresql${PG_MAJOR}-devel"; exit 1; }

PKGLIBDIR=$("$PG_CONFIG" --pkglibdir)     # e.g. /usr/pgsql-18/lib
SHAREDIR=$("$PG_CONFIG"  --sharedir)      # e.g. /usr/pgsql-18/share
EXTDIR="$SHAREDIR/extension"

PKG_NAME="pg_ask_${PG_MAJOR}"

# ---------------------------------------------------------------------
# 1. Build the extension into a clean staging dir.
#
# `cargo pgrx package` runs cargo build --release and stages the
# artefacts under target/release/pg_ask-pg${PG_MAJOR}/ with a layout
# that mirrors the destination filesystem (the same staging tree the
# .deb build consumes), e.g.:
#
#   target/release/pg_ask-pgN/
#     usr/pgsql-N/lib/pg_ask.so
#     usr/pgsql-N/share/extension/pg_ask.control
#     usr/pgsql-N/share/extension/pg_ask--*.sql
#
# (The exact prefix follows pg_config, so on PGDG it is /usr/pgsql-N.)
# ---------------------------------------------------------------------
echo "==> cargo pgrx package (pg${PG_MAJOR}, ${ARCH})"
rm -rf "target/release/pg_ask-pg${PG_MAJOR}"
cargo pgrx package \
    --no-default-features --features "pg${PG_MAJOR}" \
    --pg-config "$PG_CONFIG"

STAGED="target/release/pg_ask-pg${PG_MAJOR}"
[[ -d "$STAGED" ]] || { echo "expected $STAGED after pgrx package"; exit 1; }

# ---------------------------------------------------------------------
# 2. Lay out the rpmbuild tree. We build from a BUILDROOT we populate
#    by hand (the staged files + bundled upgrade scripts), so the spec
#    has no %build/%install of its own — it just packages the buildroot.
# ---------------------------------------------------------------------
TOPDIR="$(pwd)/dist/rpmbuild"
BUILDROOT="${TOPDIR}/BUILDROOT/${PKG_NAME}-${VERSION}-1.${RPMARCH}"
rm -rf "$TOPDIR"
mkdir -p "$TOPDIR"/{SPECS,RPMS,SRPMS,BUILD,BUILDROOT}
mkdir -p "$BUILDROOT"

echo "==> assemble buildroot for ${PKG_NAME}-${VERSION}-1.${DIST}.${RPMARCH}"

# Mirror the staged filesystem tree into the buildroot. cp -a preserves
# modes/symlinks (matters for the .so).
cp -a "${STAGED}/usr" "$BUILDROOT/"

# Bundle every handwritten upgrade script so an operator on any older
# version can step through with `ALTER EXTENSION pg_ask UPDATE`.
# pgrx-package only stages the current-version files. Globbing keeps
# this correct as new upgrade paths land (same approach as build-deb.sh).
shopt -s nullglob
upgrade_scripts=(sql/pg_ask--*--*.sql)
shopt -u nullglob
if [[ ${#upgrade_scripts[@]} -eq 0 ]]; then
    echo "WARNING: no upgrade scripts found under sql/" >&2
fi
for u in "${upgrade_scripts[@]}"; do
    cp "$u" "${BUILDROOT}${EXTDIR}/"
done

# Bundle license + changelog into the standard rpm doc/license dirs
# inside the buildroot directly. We deliberately do NOT use the spec's
# %doc/%license macros: those copy from %{_builddir}/%{name}-%{version},
# which doesn't exist here because we have no %prep/%setup (we package a
# hand-populated buildroot). Staging the files ourselves and listing
# them in %files is the reliable equivalent.
DOCDIR="/usr/share/doc/${PKG_NAME}"
LICENSEDIR="/usr/share/licenses/${PKG_NAME}"
mkdir -p "${BUILDROOT}${DOCDIR}" "${BUILDROOT}${LICENSEDIR}"
cp LICENSE      "${BUILDROOT}${LICENSEDIR}/LICENSE"
cp CHANGELOG.md "${BUILDROOT}${DOCDIR}/CHANGELOG.md"

# Build a manifest of packaged files (absolute paths) for the %files
# section. Using an explicit list (rather than globs in the spec) keeps
# ownership tight and lets rpmbuild fail loudly if something is missing.
FILELIST="${TOPDIR}/files.lst"
( cd "$BUILDROOT" && find usr -type f -o -type l | sed 's#^#/#' ) > "$FILELIST"

# ---------------------------------------------------------------------
# 3. Write the spec. No %prep/%build/%install — we package the
#    pre-populated buildroot directly (BuildArch matches RPMARCH).
# ---------------------------------------------------------------------
SPEC="${TOPDIR}/SPECS/${PKG_NAME}.spec"
cat > "$SPEC" <<EOF
Name:           ${PKG_NAME}
Version:        ${VERSION}
Release:        1%{?dist}
Summary:        Ask your PostgreSQL database in natural language
License:        PostgreSQL
URL:            https://github.com/sentirum/pg_ask
BuildArch:      ${RPMARCH}

Requires:       postgresql${PG_MAJOR}-server
# The .so is dlopened by the system postgres binary; let rpm's
# automatic dependency generator pull in the right libc/openssl.
AutoReqProv:    yes

%description
pg_ask is a PostgreSQL extension that lets you query your database
in plain English (or any language). It bundles a built-in tool-using
LLM agent — schema-aware, with safe SQL execution, memory, and
multi-turn chat sessions — all driven from inside the database.

This package installs the extension for PostgreSQL ${PG_MAJOR}.
Enable it per-database with: CREATE EXTENSION pg_ask;

%files -f ${FILELIST}

%post
cat <<'BANNER'

  pg_ask installed for PostgreSQL ${PG_MAJOR}. To enable it:

    sudo -u postgres /usr/pgsql-${PG_MAJOR}/bin/psql -c "CREATE EXTENSION pg_ask;"

  Then configure a provider and API key (example: Anthropic):

    SELECT ask.config('provider', 'anthropic');
    SELECT ask.config('api_key',  'sk-ant-...');
    SELECT ask.ask('how many tables are in this database?');

  Documentation: https://github.com/sentirum/pg_ask
  Security:      https://github.com/sentirum/pg_ask/blob/main/docs/SECURITY.md

BANNER

%preun
if [ "\$1" = "0" ]; then
  cat <<'BANNER'
  pg_ask: if any database still has the extension enabled, run
    DROP EXTENSION pg_ask CASCADE;
  before removing this package.
BANNER
fi

%changelog
* $(date '+%a %b %d %Y') Sentirum RPM <rpm@sentirum.ai> - ${VERSION}-1
- Automated build of pg_ask ${VERSION} for PostgreSQL ${PG_MAJOR}.
EOF

# ---------------------------------------------------------------------
# 4. Build the binary rpm. We point %_topdir at our tree, hand rpmbuild
#    the already-populated buildroot, and disable the debuginfo/strip
#    machinery (the .so is a Rust cdylib; PGDG ships extension .so's
#    without separate debuginfo packages, and pgrx already builds
#    release-stripped enough for distribution).
# ---------------------------------------------------------------------
mkdir -p dist

# `%{dist}` normally comes from the build host's redhat-release. We set
# it explicitly so the artefact name encodes el9/el8/fc40 regardless of
# what the container reports.
echo "==> rpmbuild -bb ${PKG_NAME}.spec"
rpmbuild -bb "$SPEC" \
    --define "_topdir ${TOPDIR}" \
    --define "dist .${DIST}" \
    --buildroot "$BUILDROOT" \
    --define "debug_package %{nil}" \
    --define "__brp_strip %{nil}" \
    --define "__brp_strip_static_archive %{nil}" \
    --define "__brp_strip_comment_note %{nil}"

BUILT=$(find "${TOPDIR}/RPMS" -name '*.rpm' | head -1)
[[ -n "$BUILT" ]] || { echo "::error::rpmbuild produced no rpm"; exit 1; }

OUT="dist/$(basename "$BUILT")"
cp "$BUILT" "$OUT"

echo
echo "==> built $OUT"
rpm -qip "$OUT"
echo
echo "==> contents:"
contents="$(rpm -qlp "$OUT")"
printf '%s\n' "$contents" | head -20
