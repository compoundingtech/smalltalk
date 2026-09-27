#!/usr/bin/env node
import fs from "node:fs"
import net from "node:net"
import path from "node:path"

const [listenPath, targetPath, dropPath] = process.argv.slice(2)
if (!listenPath || !targetPath || !dropPath) process.exit(2)
const sockets = new Set()
const server = net.createServer((client) => {
  const target = net.createConnection(targetPath)
  sockets.add(client); sockets.add(target)
  client.pipe(target); target.pipe(client)
  const close = () => { sockets.delete(client); sockets.delete(target); client.destroy(); target.destroy() }
  client.on("close", close); target.on("close", close)
})
try { fs.unlinkSync(listenPath) } catch {}
server.listen(listenPath)
const watcher = fs.watch(path.dirname(dropPath), (_event, name) => {
  if (name !== dropPath.split("/").at(-1) || !fs.existsSync(dropPath)) return
  for (const socket of sockets) socket.destroy()
  watcher.close(); server.close()
})
const stop = () => {
  watcher.close()
  for (const socket of sockets) socket.destroy()
  server.close(() => process.exit(0))
}
process.on("SIGTERM", stop); process.on("SIGINT", stop)
