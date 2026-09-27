#!/usr/bin/env node
import fs from "node:fs"

const [path, mode, expectedRaw] = process.argv.slice(2)
if (!path || !mode) process.exit(2)
const data = fs.existsSync(path) ? fs.readFileSync(path) : Buffer.alloc(0)
const packets = []
let offset = 0
while (offset + 5 <= data.length) {
  const type = data.readUInt8(offset)
  const length = data.readUInt32BE(offset + 1)
  if (length > 32 * 1024 * 1024) throw new Error(`oversized frame: ${length}`)
  if (offset + 5 + length > data.length) break
  packets.push({ type, payload: data.subarray(offset + 5, offset + 5 + length) })
  offset += 5 + length
}
const snapshots = []
for (let index = 0; index + 1 < packets.length; index++) {
  if (packets[index].type === 10 && packets[index + 1].type === 5) snapshots.push(index)
}
if (mode === "snapshots") process.exit(snapshots.length >= Number(expectedRaw) ? 0 : 1)
if (mode !== "final") process.exit(2)
if (offset !== data.length) throw new Error("truncated trailing frame")
if (packets[0]?.type !== 10 || packets[1]?.type !== 5) throw new Error("initial snapshot order")
if (snapshots.length !== 2) throw new Error(`expected two snapshots, got ${snapshots.length}`)
for (const index of snapshots) {
  const geometry = packets[index].payload
  if (geometry.length !== 4 || geometry.readUInt16BE(0) === 0 || geometry.readUInt16BE(2) === 0) throw new Error("invalid geometry")
}
const initial = packets[snapshots[0] + 1].payload
const reconnected = packets[snapshots[1] + 1].payload
if (!initial.includes(Buffer.from("INITIAL_COLOR_61e8")) || !initial.includes(Buffer.from("\x1b"))) throw new Error("initial screen")
if (!reconnected.includes(Buffer.from("AFTER_DROP_61e8"))) throw new Error("reconnect screen")
const exits = packets.filter((packet) => packet.type === 4)
if (exits.length !== 1 || packets.at(-1).type !== 4) throw new Error("terminal exit")
if (!packets.some((packet) => packet.type === 0 && packet.payload.includes(Buffer.from("FINAL_DATA_61e8")))) throw new Error("final data")
if (packets.some((packet) => ![0, 4, 5, 10].includes(packet.type))) throw new Error("packet type")
console.log("PACKAGED-FD-GREEN-61e8")
console.log("INITIAL-SNAPSHOT-GREEN-61e8")
console.log("RECONNECT-SNAPSHOT-GREEN-61e8")
console.log("FRAMED-TERMINAL-STREAM-GREEN-61e8")
