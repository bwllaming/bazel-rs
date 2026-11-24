#!/bin/bash
echo "== env"
env | LC_ALL=C sort
echo "== pwd"
pwd
echo "== args"
echo "$0" "$@"
echo "== umask"
umask
echo "== runfiles"
find . | LC_ALL=C sort
echo "== data"
cat data.txt lib_data.txt
