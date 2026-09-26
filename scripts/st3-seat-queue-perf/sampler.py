#!/usr/bin/env python3
"""Sample one daemon's CPU, context switches, and memory once a minute.

Usage: sampler.py PID MINUTES OUT_CSV

CPU time comes from /proc/PID/stat, which covers every thread, including ones
that exited. Context switches are kept per thread, so the sampler polls every
thread each second and keeps the last count of threads that have exited. A
thread that lives for less than a second can still be missed.
"""

import glob
import os
import sys
import time


def read_status(path):
    values = {}
    try:
        with open(path) as handle:
            for line in handle:
                name, _, value = line.partition(":")
                values[name] = value.split()[0] if value.split() else ""
    except (FileNotFoundError, ProcessLookupError):
        return None
    return values


def cpu_ticks(pid):
    with open(f"/proc/{pid}/stat") as stat:
        fields = stat.read().rsplit(")", 1)[1].split()
    return int(fields[11]) + int(fields[12])


def main():
    pid, minutes, out = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
    switches = {}

    def poll():
        for path in glob.glob(f"/proc/{pid}/task/*/status"):
            status = read_status(path)
            if status:
                tid = path.split("/")[4]
                switches[tid] = (int(status["voluntary_ctxt_switches"]), int(status["nonvoluntary_ctxt_switches"]))

    with open(out, "w") as handle:
        handle.write("unix_ms,cpu_ticks,voluntary_switches,involuntary_switches,rss_kib,hwm_kib,threads\n")
        started = time.monotonic()
        for minute in range(minutes + 1):
            while time.monotonic() < started + minute * 60:
                poll()
                time.sleep(1.0)
            poll()
            status = read_status(f"/proc/{pid}/status")
            voluntary = sum(counts[0] for counts in switches.values())
            involuntary = sum(counts[1] for counts in switches.values())
            handle.write(
                f"{int(time.time() * 1000)},{cpu_ticks(pid)},{voluntary},{involuntary},"
                f"{status['VmRSS']},{status['VmHWM']},{status['Threads']}\n"
            )
            handle.flush()


if __name__ == "__main__":
    main()
