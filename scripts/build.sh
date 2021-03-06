#!/bin/bash
set -xeo pipefail

# `x86_64-osx` is also available, but requires osxcross installed (see builder-os.Dockerfile)
TARGETS=${TARGETS:-x86_64-linux,x86_64-win,arm32v7,arm64v8}

build() {
  name=$1; target=$2; features=$3
  ext=$([[ $target != *"-windows-"* ]] || echo .exe)
  dest=dist/$name
  mkdir -p $dest

  echo Building $name for $target with features $features

  cargo build --release --target $target --no-default-features --features "$features"

  mv target/$target/release/bwt$ext $dest/
  strip_symbols $target $dest/bwt$ext

  cp README.md LICENSE $dest/

  pack $name
}

strip_symbols() {
  case $1 in
    "x86_64-unknown-linux-gnu" | "x86_64-pc-windows-gnu") strip $2 ;;
    "x86_64-apple-darwin") x86_64-apple-darwin15-strip $2 ;;
    "armv7-unknown-linux-gnueabihf") arm-linux-gnueabihf-strip $2 ;;
    "aarch64-unknown-linux-gnu") aarch64-linux-gnu-strip $2 ;;
  esac
}

# pack an tar.gz/zip archive file, with fixed/removed metadata attrs and deterministic file order for reproducibility
pack() {
  name=$1; dir=${2:-$1}
  pushd dist
  touch -t 1711081658 $name $name/*
  if [[ $name == *"-linux" || $name == *"-arm"* ]]; then
    TZ=UTC tar --mtime='2017-11-08 16:58:00' --owner=0 --sort=name -I 'gzip --no-name' -chf $name.tar.gz $dir
  else
    find -H $dir | sort | xargs zip -X -q $name.zip
  fi
  popd
}

version=`cat Cargo.toml | egrep '^version =' | cut -d'"' -f2`

for cfg in x86_64-linux,x86_64-unknown-linux-gnu \
           x86_64-osx,x86_64-apple-darwin \
           x86_64-win,x86_64-pc-windows-gnu \
           arm32v7,armv7-unknown-linux-gnueabihf \
           arm64v8,aarch64-unknown-linux-gnu; do
  IFS=',' read platform target <<< $cfg
  if [[ $TARGETS != *"$platform"* ]]; then continue; fi

  # The OpenSSL dependency enabled by the webhooks feature causes an error on ARM targets.
  # Disable it for now on ARM, follow up at https://github.com/shesek/bwt/issues/52
  complete_feat=cli,http,electrum,track-spends$([[ $platform == "arm"* ]] || echo ',webhooks')
  electrum_feat=cli,electrum

  build bwt-$version-$platform $target $complete_feat
  build bwt-$version-electrum_only-$platform $target $electrum_feat
done

echo Building electrum plugin
for platform in x86_64-linux x86_64-win x86_64-osx arm32v7 arm64v8; do
  if [[ $TARGETS != *"$platform"* ]]; then continue; fi

  name=bwt-$version-electrum_plugin-$platform
  dest=dist/$name
  mkdir $dest
  cp contrib/electrum-plugin/*.py $dest
  cp dist/bwt-$version-electrum_only-$platform/* $dest
  # needs to be inside a directory with a name that matches the plugin module name for electrum to load it,
  # create a temporary link to get tar/zip to pack it properly. (can also be done for tar.gz with --transform)
  ln -s $name dist/bwt
  pack $name bwt
  rm dist/bwt
done

# remove subdirectories, keep release tarballs
rm -r dist/*/
