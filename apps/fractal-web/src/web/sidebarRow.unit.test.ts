import { describe, expect, it } from 'vitest'
import { sidebarRow } from './sidebarRow.ts'
import type { Agent } from '../data/source.ts'
const agent: Agent = { ref:'agent/fixture/readonly', terminal:'', name:'Reported name', lifecycle: {_tag:'Unknown'}, host:'Reported host', connected:true, activity:'idle', status:'idle', usage: {_tag:'Unknown'}, checkout: {_tag:'Unknown'}, workspace: {_tag:'Unknown'}, startedAt: {_tag:'Unknown'}, endedAt: {_tag:'Unknown'}, blockedOn: {_tag:'Unknown'}, ask: {_tag:'Unknown'}, lastActivityAt: {_tag:'Unknown'} }
const row = (change: Partial<Agent> = {}, stale = false) => sidebarRow({agent:{...agent,...change},stale,now:10000})
describe('controlled native sidebar observations', () => {
  it('never exposes an unknown raw status in hover copy', () => {
    for (const status of ['indeterminate', 'unexpected-runtime-value']) {
      expect(row({ status }).statusLabel).toBe('Not observed')
      expect(row({ status }, true).statusLabel).toBe('Last verified · Not observed')
      expect(row({ status, connected: false }).statusLabel).toBe('Offline · Not observed')
    }
  })
  it('does not invent usage, duration, attention, unread or children', () => {
    const result=row()
    expect(result.usage).toEqual({_tag:'Unknown'})
    expect(result.duration).toEqual({_tag:'Unknown'})
    expect(result.lastTurn).toEqual({_tag:'Unknown'})
    expect(result.needsMe).toBeUndefined()
    expect(result.unread).toBeUndefined()
    expect(result.childrenKnown).toBe(false)
    expect(result.terminal).toBeUndefined()
  })
  it('keeps generic activity distinct from a completed turn', () => {
    expect(row({lastActivityAt:{_tag:'Known',value:9000}}).lastTurn).toEqual({_tag:'Known',kind:'activity',at:9000})
    expect(row({lastActivityAt:{_tag:'Known',value:9000}}).statusSince).toBeUndefined()
  })
  it('rejects future, nonfinite and non-date timestamps before render', () => {
    for(const at of [10001,NaN,Infinity,-1,8640000000000001]) {
      expect(row({lastActivityAt:{_tag:'Known',value:at},statusSince:at}).lastTurn).toEqual({_tag:'Unknown'})
      expect(row({lastActivityAt:{_tag:'Known',value:at},statusSince:at}).statusSince).toBeUndefined()
    }
  })
  it('retains distinct native lifecycle states and freshness', () => {
    for(const [state,status] of [['retired','retired'],['suspended','suspended'],['stopped','ended'],['starting','pending']] as const)
      expect(row({state}).status).toBe(status)
    expect(row({connected:false}).status).toBe('offline')
    expect(row({activity:'working'},true)).toMatchObject({status:'stale',freshness:'stale'})
    expect(row({activity:'waiting'}).status).toBe('waiting')
    expect(row({activity:'waiting'}).needsMe).toBeUndefined()
  })
  it('retains canonical identity and reported metadata without a new read', () => {
    expect(row({description:'Actual description',harness:'codex',terminal:'terminal/native',mission:'mission/native'})).toMatchObject({ref:agent.ref,id:agent.ref,title:agent.name,host:agent.host,description:'Actual description',harness:'codex',terminal:'terminal/native',mission:'mission/native'})
  })
})
