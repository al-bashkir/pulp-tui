#!/usr/bin/env bash
# Build local test fixtures (idempotent: skipped when already built).
#   target/fixtures/rpms/*.rpm             tiny noarch packages for uploads
#   target/fixtures/upstream/rpm/          yum repository (+ updateinfo advisory) for sync
#   target/fixtures/upstream/rpm2/         same repository with one newer package
#   target/fixtures/upstream/file/         File repository with PULP_MANIFEST
#   target/fixtures/files/                 loose files for uploads
# Requires rpmbuild, createrepo_c, modifyrepo_c.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OUT=${FIXTURES_DIR:-$ROOT/target/fixtures}
STAMP=$OUT/.built-v1
[ -f "$STAMP" ] && { echo "fixtures up to date: $OUT"; exit 0; }
rm -rf "$OUT"
mkdir -p "$OUT"/{rpms,files,build,upstream/rpm,upstream/rpm2,upstream/file}

build_rpm() { # name version release
  local name=$1 version=$2 release=$3
  cat >"$OUT/build/$name-$version.spec" <<EOF
Name: $name
Version: $version
Release: $release
Summary: pulp-tui test fixture $name
License: MIT
BuildArch: noarch
%description
Fixture package $name-$version-$release for pulp-tui integration tests.
%install
mkdir -p %{buildroot}/usr/share/$name
echo "$name $version-$release" > %{buildroot}/usr/share/$name/version
%files
/usr/share/$name/version
EOF
  rpmbuild -bb --quiet --define "_topdir $OUT/build/top" --define "_rpmdir $OUT/rpms" \
    --define '_build_name_fmt %%{NAME}-%%{VERSION}-%%{RELEASE}.%%{ARCH}.rpm' \
    --undefine source_date_epoch_from_changelog \
    "$OUT/build/$name-$version.spec" >/dev/null
}

build_rpm fixture-alpha 1.0 1
build_rpm fixture-alpha 1.1 1
build_rpm fixture-beta 2.0 1
build_rpm fixture-beta-debuginfo 2.0 1
build_rpm fixture-gamma 0.5 1
build_rpm fixture-upload 3.0 1

# Upstream v1: alpha-1.0, beta, beta-debuginfo + advisory for beta.
cp "$OUT"/rpms/fixture-alpha-1.0-1.noarch.rpm "$OUT"/rpms/fixture-beta-*.rpm "$OUT/upstream/rpm/"
createrepo_c --quiet "$OUT/upstream/rpm"
cat >"$OUT/build/updateinfo.xml" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<updates>
  <update from="pulp-tui@example.com" status="final" type="bugfix" version="1">
    <id>FIXTURE-2026:0001</id>
    <title>fixture-beta bug fix</title>
    <issued date="2026-01-01 00:00:00"/>
    <release>fixture</release>
    <severity>Low</severity>
    <description>Fixture advisory referencing fixture-beta.</description>
    <pkglist>
      <collection short="fixture">
        <name>fixture</name>
        <package name="fixture-beta" version="2.0" release="1" epoch="0" arch="noarch" src="fixture-beta-2.0-1.src.rpm">
          <filename>fixture-beta-2.0-1.noarch.rpm</filename>
        </package>
      </collection>
    </pkglist>
  </update>
</updates>
EOF
modifyrepo_c "$OUT/build/updateinfo.xml" "$OUT/upstream/rpm/repodata"

# Upstream v2: adds alpha-1.1 and gamma.
cp "$OUT"/upstream/rpm/*.rpm "$OUT"/rpms/fixture-alpha-1.1-1.noarch.rpm "$OUT"/rpms/fixture-gamma-0.5-1.noarch.rpm "$OUT/upstream/rpm2/"
createrepo_c --quiet "$OUT/upstream/rpm2"
modifyrepo_c "$OUT/build/updateinfo.xml" "$OUT/upstream/rpm2/repodata"

# File content: upstream manifest repository and loose upload files.
mkdir -p "$OUT/upstream/file/docs" "$OUT/upstream/file/isos"
echo "readme v1" >"$OUT/upstream/file/docs/readme.txt"
echo "guide v1" >"$OUT/upstream/file/docs/guide.txt"
head -c 4096 /dev/zero >"$OUT/upstream/file/isos/boot.iso"
(cd "$OUT/upstream/file" && for f in docs/readme.txt docs/guide.txt isos/boot.iso; do
  echo "$f,$(sha256sum "$f" | cut -d' ' -f1),$(stat -c %s "$f")"
done >PULP_MANIFEST)
echo "upload one" >"$OUT/files/one.txt"
echo "upload two" >"$OUT/files/two.txt"
head -c $((3 * 1024 * 1024 + 17)) /dev/urandom >"$OUT/files/big.bin"

rm -rf "$OUT/build"
touch "$STAMP"
echo "fixtures built: $OUT"
