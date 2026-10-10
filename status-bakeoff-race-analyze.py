"""Require exercised delay, newer accepted inputs during it, and final actual flushed glyph."""
import datetime,json,sys
from pathlib import Path
INPUT,FRACTAL,WIRE,OUT=sys.argv[1:5]
def load(path):return [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]
inputs=load(INPUT);frames=load(FRACTAL);wire=load(WIRE)
delays=[r for r in wire if r.get('kind')=='delayed_view'];releases=[r for r in wire if r.get('kind')=='released_delayed_view']
assert len(delays)==1 and len(releases)==1,('delay not actually exercised',delays,releases)
lo,hi=delays[0]['wall_ns']/1e6,releases[0]['wall_ns']/1e6
newer=[r for r in inputs if r.get('kind')=='input' and r.get('case') in ['newer-idle','final-human-ask']]
assert len(newer)==2 and all(lo<r['receipt']['accepted_ms']<hi for r in newer),('newer invalidations not both inside held success',lo,hi,newer)
final=newer[-1]['receipt'];matches=[]
published={};cache={}
for message in wire:
    value=message.get('value')
    if not isinstance(value,dict):continue
    value=value.get('value',value)
    if not isinstance(value,dict) or not value.get('publication'):continue
    connection=message['connection']
    if value.get('kind')=='snapshot':rows={r['id']:r for r in value.get('items',[])}
    elif value.get('kind')=='changes':
        rows=dict(cache.get(connection,{}));rows.update({r['id']:r for r in value.get('upserts',[])})
        for removed in value.get('removes',[]):rows.pop(removed,None)
    else:continue
    cache[connection]=rows;p=value['publication']
    published[(p['node_epoch'],p['revision'])]=rows.get('agent/status-bakeoff/scratch')
for frame in frames:
    if frame.get('stage')!='flushed' or not frame.get('publication'):continue
    publication=frame['publication']
    resource=published.get((publication['node_epoch'],publication['revision']))
    if publication['store_index']<final['store_index'] or not resource or resource.get('incarnation_id')!=final['input']['incarnation']:continue
    if not resource.get('since') or round(datetime.datetime.fromisoformat(resource['since'].replace('Z','+00:00')).timestamp()*1000)!=final['record']['body']['fields']['observed_at_ms']:continue
    lines=[line for line in frame.get('screen') or [] if 'scratch' in line and '▲' in line.split('scratch')[0]]
    if lines:matches.append({'at_ms':frame['at_ms'],'publication':publication,'glyph_line':lines[0]})
assert matches,'newer final human glyph never flushed after the delayed success and producer went quiet'
latest=max((r for r in frames if r.get('stage')=='flushed'),key=lambda r:r['at_ms'])
assert any('scratch' in line and '▲' in line.split('scratch')[0] for line in latest.get('screen') or []),'final quiet screen regressed'
result={'pass':True,'delay_ms':delays[0]['delay_ms'],'held_success_begin_ms':lo,'held_success_release_ms':hi,'newer_accepted_inputs':newer,'first_final_glyph':matches[0],'last_quiet_glyph':{'at_ms':latest['at_ms'],'publication':latest['publication']},'rejections':[r for r in frames if r.get('stage')=='rejected']}
Path(OUT).write_text(json.dumps(result,indent=2)+'\n');print(json.dumps({'pass':True,'delay_ms':result['delay_ms'],'final_revision':matches[0]['publication']['revision'],'final_glyph_ms':matches[0]['at_ms']},indent=2))
