#!/bin/bash
echo "bin $# $*"
cat sh/greeting.txt 2>/dev/null || echo no-greeting
echo "RUNFILES_DIR=${RUNFILES_DIR:+set}"
ls
