#!/bin/sh
# The SigIgn line of a test or `run` target (capture.py).
echo "probe $(grep SigIgn /proc/self/status)"
