# Pinned baseline evidence, 2026-10-08

Candidate: v0.3.17, source f4cec96e414fee2a780910730b019fe6bdbddc1d.
Archive SHA256: d30869ac1251dbeb30862bc3f5c74db4a0ccfe385110e43c5c474aa385dcddb2.
BUILD.json is retained in both JSON receipts. This old archive includes its bundled
runtime; it does not establish compatibility of a new native host binary.

Both runs used one fresh systemd container at a time, user ada on studio, two CPUs,
3 GiB RAM and no host mounts. Neither had gh, developer tools or a provider. Docker
restart was used for the container boot check. Each test container was removed.

| Run | PASS | XFAIL | FAIL / XPASS | Exit |
| --- | ---: | ---: | ---: | ---: |
| Ubuntu 22.04, systemd 249 | 9 | 10 | 0 | 0 |
| Ubuntu 24.04, systemd 255 | 9 | 9 | 0 | 0 |

The 22.04 run additionally verifies the requested guest release. Its first attempt
misclassified a missing-harness error because a glibc loader warning contained
"not found". The classifier now removes those lines and rejects generic spawn
errors; the complete corrected 22.04 run is the retained receipt. On 24.04 the
recorded wait reaches the 15-second bound without a missing-provider diagnosis.
On 22.04 the output contains the unsupported --expand-environment=no option.

Expected failures reproduce plain st printing help in a PTY, absent setup, manually
required person config, daemon-down text without setup guidance, irrelevant doctor
warnings, missing provider diagnosis, non-idle outside-seat MCP, no daemon before
login after restart, release version wording, and the additional weak-version
loader warning on 22.04. Baseline 24's combined diagnostic label mentions both
possible old seat failures; its exact command output shows the silent wait.

Commands (the archive was downloaded once and checksum verified by the runner):

```sh
scripts/onboarding-e2e baseline --ubuntu 22.04 --image onb-sysd \
  --archive /tmp/smalltalk-x86_64-unknown-linux-gnu.tar.gz --out /tmp/baseline-22
scripts/onboarding-e2e baseline --ubuntu 24.04 --image onb-sysd2404 \
  --archive /tmp/smalltalk-x86_64-unknown-linux-gnu.tar.gz --out /tmp/baseline-24
python3 scripts/onboarding-fixtures/test_rig.py -v
```

JSON files retain every exact guest command, separate stdout/stderr, exit code and
elapsed time. Text files retain the runner output. Five local checks passed for
archive integrity/traversal, the CI VM guard, run identity and consent-before-start.
Incus CLI absence and lack of read/write /dev/kvm access were separately confirmed;
the runner exits 2 before creating output or a guest when those prerequisites are
absent. Real Incus/Tart boot, polkit and SSH login remain untested. Candidate
provider/mission scenarios and the default image build are not claimed by these
baseline receipts. See docs/onboarding-vm-runbook.md for the manual next pass.
