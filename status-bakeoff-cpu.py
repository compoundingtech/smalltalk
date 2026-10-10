"""Boundary CPU accounting; no sampler/poll loop or synthetic load."""
import json,os,sys,time
from pathlib import Path
phase,out,*pids=sys.argv[1:]
records=[]
for pid in pids:
    try:
        stat=Path('/proc/'+pid+'/stat').read_text();fields=stat[stat.rindex(')')+2:].split()
        records.append({'pid':int(pid),'start_ticks':int(fields[19]),'user_ticks':int(fields[11]),'system_ticks':int(fields[12]),'rss_pages':int(fields[21])})
    except (FileNotFoundError,ProcessLookupError) as error:records.append({'pid':int(pid),'missing':True})
record={'phase':phase,'wall_ns':time.time_ns(),'mono_ns':time.monotonic_ns(),'clk_tck':os.sysconf('SC_CLK_TCK'),'page_size':os.sysconf('SC_PAGE_SIZE'),'processes':records,'cpu_pressure':Path('/proc/pressure/cpu').read_text(),'loadavg':Path('/proc/loadavg').read_text()}
with open(out,'a') as f:f.write(json.dumps(record)+'\n')
print(json.dumps(record))
