#!/usr/bin/env bash
# Build local test fixtures (idempotent: skipped when already built).
#   target/fixtures/rpms/*.rpm             tiny noarch packages for uploads
#   target/fixtures/upstream/rpm/          yum repository (+ updateinfo advisory) for sync
#   target/fixtures/upstream/rpm2/         same repository with one newer package
#   target/fixtures/upstream/rpm-new/      only the two packages rpm2 adds (none in common with rpm)
#   target/fixtures/upstream/file/         File repository with PULP_MANIFEST
#   target/fixtures/upstream/git/files.git  bare Git repository (dumb HTTP), tag v1 and branch main
#   target/fixtures/upstream/deb/          APT repository (dist fixture, component main, amd64)
#   target/fixtures/files/                 loose files for uploads
# Requires rpmbuild, createrepo_c, modifyrepo_c, git, and GNU ar, tar, gzip (the .debs are built
# without dpkg-deb).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OUT=${FIXTURES_DIR:-$ROOT/target/fixtures}
STAMP=$OUT/.built-v5
[ -f "$STAMP" ] && { echo "fixtures up to date: $OUT"; exit 0; }
rm -rf "$OUT"
mkdir -p "$OUT"/{rpms,files,build,upstream/rpm,upstream/rpm2,upstream/rpm-new,upstream/file,upstream/deb}

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
    --undefine source_date_epoch_from_changelog --define '_buildhost pulp-tui-fixtures' \
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

# Second ACS path with no package in common with rpm: Pulp syncs ACS paths at once (docs/COMPATIBILITY.md).
cp "$OUT"/rpms/fixture-alpha-1.1-1.noarch.rpm "$OUT"/rpms/fixture-gamma-0.5-1.noarch.rpm "$OUT/upstream/rpm-new/"
createrepo_c --quiet "$OUT/upstream/rpm-new"

# File content: upstream manifest repository and loose upload files.
mkdir -p "$OUT/upstream/file/docs" "$OUT/upstream/file/isos"
echo "readme v1" >"$OUT/upstream/file/docs/readme.txt"
echo "guide v1" >"$OUT/upstream/file/docs/guide.txt"
head -c 4096 /dev/zero >"$OUT/upstream/file/isos/boot.iso"
(cd "$OUT/upstream/file" && for f in docs/readme.txt docs/guide.txt isos/boot.iso; do
  echo "$f,$(sha256sum "$f" | cut -d' ' -f1),$(stat -c %s "$f")"
done >PULP_MANIFEST)

# Git repository for a pulp_file Git remote: bare, for the dumb HTTP protocol (update-server-info).
# Tag v1: docs/readme.txt. Branch main (HEAD): also docs/guide.txt.
GITW=$OUT/build/git
git_fx() {
  GIT_AUTHOR_DATE=2026-01-01T00:00:00Z GIT_COMMITTER_DATE=2026-01-01T00:00:00Z \
    git -C "$GITW" -c user.name=pulp-tui -c user.email=pulp-tui@example.com -c commit.gpgsign=false -c tag.gpgsign=false "$@"
}
git init -q -b main "$GITW"
mkdir -p "$GITW/docs"
echo "readme v1" >"$GITW/docs/readme.txt"
git_fx add docs && git_fx commit -q -m v1 && git_fx tag v1
echo "guide v1" >"$GITW/docs/guide.txt"
git_fx add docs && git_fx commit -q -m v2
git clone -q --bare "$GITW" "$OUT/upstream/git/files.git"
git -C "$OUT/upstream/git/files.git" update-server-info

# Debian content: a .deb is an ar archive of debian-binary, control.tar.gz and data.tar.gz,
# in that order. Fixed mtime, owner and gzip -n keep the archives reproducible.
DEB=$OUT/upstream/deb
PACKAGES=$DEB/dists/fixture/main/binary-amd64/Packages
mkdir -p "$(dirname "$PACKAGES")"
: >"$PACKAGES"

build_deb() ( # name version (subshell: umask stays local)
  umask 022
  local name=$1 version=$2 w="$OUT/build/deb-$1-$2"
  local pool="pool/main/${name:0:1}/$name" file="${name}_${version}_amd64.deb"
  mkdir -p "$w/control" "$w/data/usr/share/$name" "$DEB/$pool"
  echo "$name $version" >"$w/data/usr/share/$name/version"
  cat >"$w/control/control" <<EOF
Package: $name
Version: $version
Architecture: amd64
Maintainer: pulp-tui <pulp-tui@example.com>
Description: pulp-tui test fixture $name
EOF
  echo 2.0 >"$w/debian-binary"
  for part in control data; do
    tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner -C "$w/$part" -cf - . | gzip -n >"$w/$part.tar.gz"
  done
  # D: deterministic archive (no current mtime/uid in the member headers).
  (cd "$w" && ar rcD "$DEB/$pool/$file" debian-binary control.tar.gz data.tar.gz)
  {
    cat "$w/control/control"
    echo "Filename: $pool/$file"
    echo "Size: $(stat -c %s "$DEB/$pool/$file")"
    echo "SHA256: $(sha256sum "$DEB/$pool/$file" | cut -d' ' -f1)"
    echo
  } >>"$PACKAGES"
)

build_deb fixture-alpha 1.0-1
build_deb fixture-alpha 1.1-1
build_deb fixture-beta 2.0-1
build_deb fixture-beta-dbg 2.0-1

# Only the plain Packages index is listed; a sync needs no compressed variant.
cat >"$DEB/dists/fixture/Release" <<EOF
Origin: pulp-tui
Label: pulp-tui
Suite: fixture
Codename: fixture
Date: Thu, 01 Jan 2026 00:00:00 UTC
Architectures: amd64
Components: main
Description: pulp-tui test fixture
SHA256:
 $(sha256sum "$PACKAGES" | cut -d' ' -f1) $(stat -c %s "$PACKAGES") main/binary-amd64/Packages
EOF

echo "upload one" >"$OUT/files/one.txt"
echo "upload two" >"$OUT/files/two.txt"
head -c $((3 * 1024 * 1024 + 17)) /dev/urandom >"$OUT/files/big.bin"

rm -rf "$OUT/build"
touch "$STAMP"
echo "fixtures built: $OUT"
