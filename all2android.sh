#!/bin/bash

# Run cargo2android.py for every crate in crosvm.

set -e

for i in $(find . -type f -name Cargo.toml | xargs dirname | sort); do
    ./run_c2a.sh "$i"
done
