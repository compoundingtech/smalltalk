#!/bin/sh
set -eu

st3 trace wait resource/local-file-refresh/file --for ready --timeout 30s >/dev/null
printf '%s\n' 'changed content stays local' > watched.txt
st3 --json resource refresh resource/local-file-refresh/file \
  --timeout 30s \
  --as person/eval-requester > refresh.json
