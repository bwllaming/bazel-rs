#!/bin/bash
echo "GREETING_FILE=$GREETING_FILE"
cat "$RUNFILES_DIR/$GREETING_FILE" 2>/dev/null || cat "../$GREETING_FILE" 2>/dev/null || echo unreadable
echo "LIBS=$LIBS COVERAGE=$COVERAGE"
