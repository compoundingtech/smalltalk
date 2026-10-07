import { describe, expect, it } from 'vitest'
import { observeSyncStatus, syncLine } from '../../../../packages/fractal-ui/src/assistant-ui/st3-views/sync-line.ts'
describe('portable sync presentation', () => {
 it('retains same-stage transition time and advances only on status or stage changes', () => {
  const first=observeSyncStatus(undefined,{_tag:'Requested',since:100},100)
  expect(observeSyncStatus(first,{_tag:'Requested',since:200},200).observedAt).toBe(100)
  const progress={_tag:'Progress' as const,stage:'reading' as const,elapsedMs:500,stageSince:200,reportedAt:300}
  const reading=observeSyncStatus(first,progress,300)
  expect(reading.observedAt).toBe(300)
  expect(observeSyncStatus(reading,{...progress,reportedAt:400,done:2,total:3},400).observedAt).toBe(300)
  expect(observeSyncStatus(reading,{...progress,stage:'routing'},400).observedAt).toBe(400)
 })
 it('uses explicit clocks and separates a slow unanswered request from live data', () => {
  const status={_tag:'Requested' as const,since:1000}
  expect(syncLine({status,label:'conversation',now:1399,observedAt:1000})).toBeUndefined()
  expect(syncLine({status,label:'conversation',now:6000,observedAt:1000})).toMatchObject({tone:'warning',text:'No reply from st for 5s',animate:false})
  expect(syncLine({status:{_tag:'Live',since:1000},label:'conversation',now:6000,observedAt:1000})).toBeUndefined()
 })
 it('does not keep animating progress after reports stop', () => {
  const status={_tag:'Progress' as const,stage:'reading' as const,elapsedMs:1000,stageSince:1000,reportedAt:2000,done:2,total:7}
  expect(syncLine({status,label:'conversation',now:2500,observedAt:1000})).toMatchObject({text:'Reading conversation · 2 of 7',animate:true,tone:'neutral'})
  expect(syncLine({status,label:'conversation',now:7000,observedAt:1000})).toMatchObject({text:'st stopped reporting at reading · 5s',animate:false,tone:'warning'})
 })
 it('distinguishes stale content from current live status and hides an explicit eviction', () => {
  expect(syncLine({status:{_tag:'Stale',reason:{_tag:'Quiet',lastFrameAt:1000}},label:'conversation',now:6000,observedAt:5000})).toMatchObject({text:'Last heard from st 5s ago',tone:'warning',animate:false})
  expect(syncLine({status:{_tag:'Stale',reason:{_tag:'Evicted'}},label:'conversation',now:6000,observedAt:5000})).toBeUndefined()
 })
 it('preserves a reported failure rather than announcing synchronization', () => {
  expect(syncLine({status:{_tag:'Failed',code:'forbidden',message:'Denied'},label:'conversation',now:6000,observedAt:5000})).toMatchObject({text:'No access to conversation · ask the gateway owner',tone:'error',animate:false})
  expect(syncLine({status:{_tag:'Failed',code:'reported-code',message:'Actual reported detail'},label:'conversation',now:6000,observedAt:5000})).toMatchObject({text:"Couldn't load conversation: Actual reported detail",tone:'error'})
 })
})
