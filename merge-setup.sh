#!/bin/bash

set -ex

sync=""
while getopts 's' FLAG; do
  case ${FLAG} in
    s)
      sync="sync"
      ;;
    ?)
      "unknown argument."
      exit 1
      ;;
  esac
done


if [ "$sync" = "sync" ]
then
  read -p "This script will sync your crosvm project. Do you wish to proceed? [y/N]" -n 1 -r
  if [[ ! $REPLY =~ ^[Yy]$ ]]
  then
    exit 1;
  fi
fi

if [ -z $ANDROID_BUILD_TOP ]; then echo "forgot to source build/envsetup.sh?" && exit 1; fi
cd $ANDROID_BUILD_TOP/external/crosvm

if ! [[ -z $(git branch --list merge) ]];
  then
    echo "branch merge already exists. Forgot to clean up?" && exit 1;
fi
rustup update
if [ "$sync" = "sync" ]
then
  repo sync . -c -j96
  git fetch --all --prune
fi

source $ANDROID_BUILD_TOP/build/envsetup.sh
m blueprint_tools
m crosvm
repo start merge
git merge --log aosp/upstream-main
$ANDROID_BUILD_TOP/external/crosvm/tools/install-deps
