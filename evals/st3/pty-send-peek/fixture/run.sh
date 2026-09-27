#!/usr/bin/env bash
set -euo pipefail

subject="pty/$ST_MISSION_RUN/psp"
for _ in $(seq 1 100); do
  st3 terminals peek "$subject" >pre.txt 2>/dev/null && grep -Fq READY pre.txt && break
  sleep 0.05
done
grep -Fq READY pre.txt
! grep -Fq ACK:PSP-9f3a7c2e pre.txt
st3 terminals send "$subject" PSP-9f3a7c2e >/dev/null
for _ in $(seq 1 100); do
  st3 terminals peek "$subject" >post.txt
  grep -Fq ACK:PSP-9f3a7c2e post.txt && exit 0
  sleep 0.05
done
exit 1
