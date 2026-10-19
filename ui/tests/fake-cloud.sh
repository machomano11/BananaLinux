#!/bin/sh
# banana-cloud's stand-in for the headless test: says it ran (and how) in $BANANA_RUN/cloud.ran, and that it is done
echo "${1:-saves}${2:+ $2}" > "$BANANA_RUN/cloud.ran"
echo "done 0 0 0 0" > "$BANANA_RUN/cloud.status"
