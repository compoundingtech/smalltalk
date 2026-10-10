"""Join real observations to FIRST matching publication and actual flushed glyphs."""
import json,math,sys,os,datetime
from pathlib import Path
INPUTS,FRACTAL,WIRE,CPU,OUTPUT=sys.argv[1:6]
PUBLICATIONS=sys.argv[6] if len(sys.argv)>6 else None
ORIGIN_PUBLICATIONS=sys.argv[7] if len(sys.argv)>7 else None
def load(p):return [json.loads(line) for line in Path(p).read_text().splitlines() if line.strip()]
def stats(values):
    values=sorted(values)
    return {'n':len(values),'p50_ms':values[math.ceil(.5*len(values))-1] if values else None,'p95_ms':values[math.ceil(.95*len(values))-1] if values else None,'max_ms':max(values) if values else None}
def identity(p):return (p['node_epoch'],p['revision'],p['generation'])
def glyph(record,expected):
    for line in record.get('screen') or []:
        if 'scratch' not in line:continue
        before=line.split('scratch')[0]
        if expected=='human' and '▲' in before:return line
        if expected=='idle' and '○' in before:return line
        if expected=='working' and any('\u2800'<=c<='\u28ff' for c in before):return line
    return None
def matches(resource,receipt):
    if not resource or resource.get('incarnation_id')!=receipt['input']['incarnation'] or not resource.get('since'):return False
    since_ms=round(datetime.datetime.fromisoformat(resource['since'].replace('Z','+00:00')).timestamp()*1000)
    record=receipt['record'].get('value',receipt['record'])
    return since_ms==record['body']['fields']['observed_at_ms']
def observed(path):
    result=[]
    if path:
        for r in load(path):
            value=r['envelope'].get('value',{})
            if value.get('kind')=='snapshot':
                resource=next((a for a in value['items'] if a.get('id')=='agent/status-bakeoff/scratch'),None)
                result.append((value['publication'],resource))
    return result
inputs=load(INPUTS);events=load(FRACTAL);wire=load(WIRE);cpu=load(CPU)
cross_host=bool(os.environ.get('BAKEOFF_CROSS_HOST'));dest=observed(PUBLICATIONS);origin=observed(ORIGIN_PUBLICATIONS)
publication_rows={};connection_rows={}
for message in wire:
    value=message.get('value')
    if not isinstance(value,dict):continue
    value=value.get('value',value)
    if not isinstance(value,dict) or not value.get('publication'):continue
    publication=value['publication'];connection=message['connection']
    if value.get('kind')=='snapshot':cache={r['id']:r for r in value.get('items',[]) if isinstance(r,dict) and 'id' in r}
    elif value.get('kind')=='changes':
        cache=dict(connection_rows.get(connection,{}));cache.update({r['id']:r for r in value.get('upserts',[]) if isinstance(r,dict) and 'id' in r})
        for removed in value.get('removes',[]):cache.pop(removed,None)
    else:continue
    connection_rows[connection]=cache;resource=cache.get('agent/status-bakeoff/scratch')
    publication_rows[(publication['node_epoch'],publication['revision'])]=resource
    if not PUBLICATIONS:dest.append((publication,resource))
flushed=[r for r in events if r.get('stage')=='flushed' and r.get('publication')];apply={}
for event in events:
    if event.get('publication') and event.get('stage')=='apply':apply.setdefault(identity(event['publication']),event['at_ms'])
rows=[];censored=[]
cohort=[r for r in inputs if r.get('kind')=='input' and r.get('case') in ['idle-to-working','working-to-idle','continuation-start','human-ask','human-approval','quiet-last-idle']]
for event in cohort:
    receipt=event['receipt'];fields=receipt['input']['fields'];expected='human' if fields.get('blocked_on')=='human' else fields['state']
    publications=[p for p,r in dest if matches(r,receipt)]
    if not publications:
        censored.append({'case':event['case'],'receipt':receipt,'reason':'Input not observed in an independently materialized downstream publication'});continue
    first=min(publications,key=lambda p:p['materialized_at_ms']);origin_candidates=[p for p,r in origin if matches(r,receipt)];first_origin=min(origin_candidates,key=lambda p:p['materialized_at_ms']) if origin_candidates else None
    match=None
    for frame in flushed:
        p=frame['publication']
        if p['node_epoch']!=first['node_epoch'] or p['revision']<first['revision']:continue
        # Match the exact native observation below. The injector's all-local tail
        # is not the roster's active-local frontier once that observation exports.
        if not cross_host and p['store_index']<receipt['store_index']:continue
        if not matches(publication_rows.get((p['node_epoch'],p['revision'])),receipt):continue
        line=glyph(frame,expected)
        if line:match=(frame,line);break
    if not match:
        censored.append({'case':event['case'],'receipt':receipt,'first_materialized_publication':first,'reason':'Materialized input never had a matching actual flushed scratch-row glyph'});continue
    frame,line=match;p=frame['publication'];key=identity(p);start=first['materialized_at_ms']
    row={'case':event['case'],'sequence':receipt['input']['sequence'],'first_materialized_publication':first,'rendered_publication':p,'glyph_line':line,'materialized_to_projection_ms':p['projected_at_ms']-start,'materialized_to_apply_ms':apply[key]-start if key in apply else None,'materialized_to_glyph_ms':frame['at_ms']-start,'accepted_to_materialized_ms':start-receipt['accepted_ms'],'accepted_to_glyph_ms':frame['at_ms']-receipt['accepted_ms']}
    if first_origin:row.update({'first_origin_publication':first_origin,'origin_materialized_to_downstream_materialized_ms':start-first_origin['materialized_at_ms'],'origin_materialized_to_glyph_ms':frame['at_ms']-first_origin['materialized_at_ms'],'accepted_to_origin_materialized_ms':first_origin['materialized_at_ms']-receipt['accepted_ms']})
    rows.append(row)
quiet=[]
for start in [r for r in inputs if r.get('kind') in ['quiet_begin','final_quiet_begin']]:
    endkind='quiet_end' if start['kind']=='quiet_begin' else 'final_quiet_end';end=next(r for r in inputs if r.get('kind')==endkind);q=[r for r in wire if start['wall_ms']*1e6<=r['wall_ns']<=end['wall_ms']*1e6]
    quiet.append({'duration_s':(end['wall_ms']-start['wall_ms'])/1000,'request_bytes':sum(r['bytes'] for r in q if r.get('kind')=='wire' and r['direction']=='request'),'response_bytes':sum(r['bytes'] for r in q if r.get('kind')=='wire' and r['direction']=='response'),'http_requests':sum(r.get('kind')=='http' and r['direction']=='request' for r in q),'ws_data_frames':sum(r.get('kind')=='frame' and r['direction']=='response' and r.get('opcode')==1 for r in q)})
resources=[]
if len(cpu)>=2:
    a,b=cpu[0],cpu[-1];elapsed=(b['mono_ns']-a['mono_ns'])/1e9
    for before in a['processes']:
        after=next((p for p in b['processes'] if p['pid']==before['pid'] and p.get('start_ticks')==before.get('start_ticks')),None)
        if not after or before.get('missing'):continue
        seconds=(after['user_ticks']+after['system_ticks']-before['user_ticks']-before['system_ticks'])/a['clk_tck'];resources.append({'pid':before['pid'],'cpu_seconds':seconds,'elapsed_seconds':elapsed,'cpu_percent_one_core':100*seconds/elapsed,'rss_end_bytes':after['rss_pages']*b['page_size']})
agent_wire=[r for r in wire if r.get('kind') in ['http','frame'] and any(route in (r.get('path') or '') for route in ['agents/poll','collections/stream'])]
result={'glyph':stats([r['materialized_to_glyph_ms'] for r in rows]),'projection':stats([r['materialized_to_projection_ms'] for r in rows]),'upstream':stats([r['accepted_to_materialized_ms'] for r in rows]),'end_to_end':stats([r['accepted_to_glyph_ms'] for r in rows]),'origin_materialized_to_glyph':stats([r['origin_materialized_to_glyph_ms'] for r in rows if 'origin_materialized_to_glyph_ms' in r]),'origin_to_downstream':stats([r['origin_materialized_to_downstream_materialized_ms'] for r in rows if 'origin_materialized_to_downstream_materialized_ms' in r]),'censored':censored,'samples':rows,'bytes':{d:sum(r['bytes'] for r in wire if r.get('kind')=='wire' and r['direction']==d) for d in ['request','response']},'agents_wire_bytes':{d:sum(r['wire_bytes'] for r in agent_wire if r['direction']==d) for d in ['request','response']},'quiet':quiet,'cpu':resources,'replay':[r for r in inputs if r.get('kind')=='replay_outcome'],'disconnects':[r for r in events if r.get('stage')=='disconnected'],'rejections':[r for r in events if r.get('stage')=='rejected']}
continuations=[]
for event in [r for r in inputs if r.get('kind')=='input' and r.get('case')=='continuation']:
    start=event['receipt']['accepted_ms']
    end=next(r['receipt']['accepted_ms'] for r in inputs if r.get('kind')=='input' and r.get('case')=='human-ask' and r['receipt']['accepted_ms']>start)
    observed_frames=[r for r in flushed if start<=r['at_ms']<end]
    continuations.append({'accepted_ms':start,'frames':len(observed_frames),'working_preserved':bool(observed_frames) and all(glyph(r,'working') is not None for r in observed_frames)})
result['continuation_checks']=continuations
Path(OUTPUT).write_text(json.dumps(result,indent=2)+'\n');print(json.dumps({k:v for k,v in result.items() if k not in ['samples','censored','disconnects','rejections','replay']},indent=2))
