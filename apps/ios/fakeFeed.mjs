// A fake client whose collections sockets the test drives frame by frame.
export function fakeClient() {
  const sockets = [];
  const client = {
    collectionStream: async options => {
      const socket = { options, sent: [], closed: false };
      socket.subscribe = (id, collection, limit, filters = {}) => socket.sent.push({ kind: 'subscribe', id, collection, limit, ...filters });
      socket.subscribeTerminal = (id, terminal, incarnation, capability) => socket.sent.push({ kind: 'subscribe', id, collection: 'terminal', terminal, incarnation, capability });
      socket.subscribeConversation = (id, conversation) => socket.sent.push({ kind: 'subscribe', id, collection: 'conversation', conversation });
      socket.unsubscribe = id => socket.sent.push({ kind: 'unsubscribe', id });
      socket.close = () => { socket.closed = true; };
      socket.frame = frame => options.onFrame(frame);
      socket.drop = error => options.onEnd(error);
      sockets.push(socket);
      return socket;
    },
  };
  return { client, sockets };
}
