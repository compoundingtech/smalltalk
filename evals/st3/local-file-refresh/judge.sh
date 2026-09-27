#!/bin/sh
set -eu

jq -e '.changed == true and (.observers | length == 1)' refresh.json >/dev/null
st3 --json inspect resource/local-file-refresh/file > resource.json
grep -F 'content_hash' resource.json >/dev/null
if grep -F 'changed content stays local' resource.json >/dev/null; then
  echo 'The resource exposed the file content.' >&2
  exit 1
fi
