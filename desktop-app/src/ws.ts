// Singleton WS client to the daemon hub, role "webpage".

type Handler = (data: any) => void;

let socket: WebSocket | null = null;
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
let connected = false;
const connectionHandlers = new Set<(c: boolean) => void>();
const topicHandlers = new Map<string, Set<Handler>>();

// The control page used to be served BY the daemon's own HTTP server, so a
// same-origin relative URL just worked. Now it's loaded straight into the
// Tauri window from bundled assets (origin tauri://localhost, not
// 127.0.0.1:8790), so the daemon's fixed host+port has to be named outright.
function daemonPort() {
  return (window as any).__CLAUDE_THING_PORT || 8790;
}

function daemonOrigin() {
  return `http://127.0.0.1:${daemonPort()}`;
}

function wsUrl() {
  return `ws://127.0.0.1:${daemonPort()}/ws`;
}

function setConnected(c: boolean) {
  connected = c;
  connectionHandlers.forEach((h) => h(c));
}

export function connect() {
  if (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)) return;
  socket = new WebSocket(wsUrl());

  socket.onopen = () => {
    if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
    socket!.send(JSON.stringify({
      type: 'request', id: 'hello-webpage', method: 'bridge.hello',
      params: { role: 'webpage', info: { ua: navigator.userAgent.slice(0, 60) } },
    }));
    setConnected(true);
  };

  socket.onmessage = (e) => {
    try {
      const msg = JSON.parse(e.data);
      if (msg.type === 'event' && msg.topic) {
        topicHandlers.get(msg.topic)?.forEach((h) => h(msg.data));
      }
    } catch {}
  };

  socket.onclose = () => {
    setConnected(false);
    reconnectTimer = setTimeout(connect, 2000);
  };
  socket.onerror = () => socket?.close();
}

// Unsubscribers return void, not Set.delete's boolean — these are handed
// straight back from useEffect, where a non-void return is a type error.
export function onTopic(topic: string, handler: Handler): () => void {
  if (!topicHandlers.has(topic)) topicHandlers.set(topic, new Set());
  topicHandlers.get(topic)!.add(handler);
  return () => { topicHandlers.get(topic)!.delete(handler); };
}

export function onConnection(handler: (c: boolean) => void): () => void {
  connectionHandlers.add(handler);
  handler(connected);
  return () => { connectionHandlers.delete(handler); };
}

export async function getStatus() {
  const r = await fetch(`${daemonOrigin()}/status`);
  if (!r.ok) throw new Error(`status ${r.status}`);
  return r.json();
}

export async function postApi(path: string, body?: unknown) {
  const r = await fetch(`${daemonOrigin()}${path}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body ?? {}),
  });
  if (!r.ok) throw new Error(await r.text());
  return r.json();
}
