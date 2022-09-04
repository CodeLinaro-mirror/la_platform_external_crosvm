#!/bin/bash

# Run cargo2android.py with the appropriate arguments for the crate in the
# supplied directory.

set -e

if [ "$#" -ne 1 ]; then
  echo "Usage: $0 CRATE_DIRECTORY"
  exit 1
fi
cd $1

if ! [ -x "$(command -v bpfmt)" ]; then
  echo 'Error: bpfmt not found.' >&2
  exit 1
fi

# C2A gives visibility to all APEXs by default. Restrict to "platform" (the
# Soong default).
C2A_ARGS="--apex-available //apex_available:platform"
if [[ -f "cargo2android.json" ]]; then
  # If the crate has a cargo2android config, let it handle all the flags.
  C2A_ARGS+=" --config cargo2android.json"
else
  # Otherwise, set common flags.
  C2A_ARGS+=" --run --device --tests --global_defaults=crosvm_defaults --add_workspace"
  # If there are subdirectories with crates, then pass --no-subdir.
  if [ -n "$(find . -mindepth 2 -name "Cargo.toml")" ]; then
    C2A_ARGS+=" --no-subdir"
  fi
fi

C2A=${C2A:-cargo2android.py}
echo "Processing \"$1\" using $C2A $C2A_ARGS"
$C2A $C2A_ARGS
rm -f cargo.out
rm -rf target.tmp || /bin/true

if [[ -f "Android.bp" ]]; then
  bpfmt -w Android.bp || /bin/true
fi

# Fix workstation specific path in "metrics" crate's generated files.
# TODO(b/232150148): Find a better solution for protobuf generated files.
if [[ `basename $1` == "metrics" ]]; then
  sed --in-place 's/path = ".*\/out/path = "./' out/generated.rs
fi
