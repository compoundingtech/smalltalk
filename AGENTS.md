# Repository instructions

## Fix causes; do not hide broken paths

Fix the cause of a slow or broken path. Do not add fallbacks, retries, caches or timeouts that hide it. A workaround that makes the symptom disappear while leaving the defect in place is not an acceptable fix.

Retries for peers and networks are valid because those connections can be unreachable by nature. Keep that distinction explicit; retries must not hide a defect in a local path.

Build reactive behavior instead of polling. Use incremental view maintenance when necessary to keep updates and reads efficient.

Nathan approved this policy on 2026-10-06. See issue #1566 and st document doc/fleet/smalltalk/no-workarounds/hunt-2026-10-06 for examples.
