// Drive st's actual extension assets through handoff, read evidence, reconnect and live labels.
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { pathToFileURL } from "node:url";
const driver = process.argv[2];
const asset = process.argv[3];
assert.ok(["pi", "omp"].includes(driver));
const dir = fs.mkdtempSync(path.join(os.tmpdir(), "st-channel-smoke-"));
const frames = path.join(dir, "frames.jsonl");
const outgoing = path.join(dir, "outgoing.jsonl");
const label = path.join(dir, "label.json");
const recorder = path.join(dir, "recorder.mjs");
fs.writeFileSync(label, JSON.stringify({ subject:"agent/eval.worker",desired:{display_name:"Quartz"},member:{tags:{persona:"gen"}} }));
fs.writeFileSync(recorder, `#!${process.execPath}
import fs from "node:fs";
process.stdout.write(JSON.stringify({type:"hello",protocol:2,sessionContext:""})+"\\n");
process.stdout.write(JSON.stringify({type:"seat",seat:JSON.parse(fs.readFileSync(${JSON.stringify(label)},"utf8"))})+"\\n");
process.stdin.on("data",chunk=>fs.appendFileSync(${JSON.stringify(frames)},chunk));
process.stdin.on("end",()=>process.exit(0));
let offset=0;
setInterval(()=>{let text="";try{text=fs.readFileSync(${JSON.stringify(outgoing)},"utf8");}catch{}
if(text.length>offset){process.stdout.write(text.slice(offset));offset=text.length;}},10);
`,{mode:0o755});
const prefix = `ST_${driver.toUpperCase()}_CHANNEL_`;
for (const [key, value] of Object.entries({BIN:recorder,CATALOG:"/test/catalog",IDENTITY:"eval.worker",RUNTIME_ID:"eval.worker",SESSION:"wrapper",SEQ:"1"})) {
  process.env[prefix+key] = value;
}
const events = new Map();
let title = "Stale saved title";
let nativeSession = "native-1";
let synchronousContext = false;
const handoffs = [];
const api = {
  on:(event,callback)=>events.set(event,callback),
  setSessionName:(label)=>{title=label;},
  sendMessage:()=>{},
  sendUserMessage:async(content)=>{
    handoffs.push(content);
    if (synchronousContext) await events.get("context")({messages:[{role:"user",content}]},ctx);
  },
};
const ctx = { isIdle:()=>true,sessionManager:{getSessionId:()=>nativeSession,getEntries:()=>[]},ui:{notify:()=>{}} };
const read = ()=>fs.existsSync(frames)?fs.readFileSync(frames,"utf8").trim().split("\n").filter(Boolean).map(JSON.parse):[];
const send = (frame)=>fs.appendFileSync(outgoing,JSON.stringify(frame)+"\n");
const until = async(predicate)=>{for(let i=0;i<200;i++){if(predicate())return;await new Promise(r=>setTimeout(r,20));}throw new Error("smoke deadline");};
const {default:extension}=await import(pathToFileURL(path.resolve(asset)));
extension(api);
await events.get("session_start")({},ctx);
await until(()=>title==="Quartz [gen]");
assert.ok(events.has("session_switch"),"OMP /new uses session_switch");
const meta={messageId:"message/quartz-1"};
send({type:"message",content:"QUARTZ SIGNAL",meta});
await until(()=>read().some(frame=>frame.type==="delivered"&&frame.meta?.messageId===meta.messageId));
assert.equal(read().filter(frame=>frame.type==="read").length,0,"native queue acceptance is not read evidence");
await events.get("context")({messages:[{role:"user",content:"QUARTZ SIGNAL"}]},ctx);
await until(()=>read().some(frame=>frame.type==="read"&&frame.meta?.messageId===meta.messageId));
const count=handoffs.length;
// A successful native handoff whose receipt acknowledgement was lost is replayed as receipts.
await events.get("session_switch")({},ctx);
await until(()=>title==="Quartz [gen]");
send({type:"message",content:"QUARTZ SIGNAL",meta});
await new Promise(r=>setTimeout(r,100));
assert.equal(handoffs.length,count,"channel replacement must not repeat accepted native input");
// /new and in-process resume rebind the authority title; cold resume already replaced the saved one.
nativeSession="native-2";
title="Native local rename";
await events.get("session_switch")({},ctx);
await until(()=>title==="Quartz [gen]");
assert.equal(title,"Quartz [gen]");
api.setSessionName("Native temporary rename");
send({type:"seat",seat:{subject:"agent/eval.worker",desired:{display_name:"Indigo"},member:{tags:{persona:"gen"}}}});
await until(()=>title==="Indigo [gen]");
// Some providers raise context synchronously from the handoff call itself.
synchronousContext=true;
const synchronous={messageId:"message/quartz-2"};
send({type:"message",content:"INDIGO SIGNAL",meta:synchronous});
await until(()=>read().some(frame=>frame.type==="read"&&frame.meta?.messageId===synchronous.messageId));
send({type:"settled",meta:synchronous});
await until(()=>!globalThis[driver==="pi"?"__stPiChannel":"__stOmpChannel"].accepted.has(synchronous.messageId));
assert.ok(!fs.existsSync(path.join(dir,"resources/inbox")));
assert.ok(!fs.existsSync(path.join(dir,"resources/archive")));
await events.get("session_shutdown")(driver==="pi"?{reason:"quit"}:{},ctx);
fs.rmSync(dir,{recursive:true,force:true});
console.log(`${driver} st channel smoke: ok`);
process.exit(0);
