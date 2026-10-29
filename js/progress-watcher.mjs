// The engine's progress watcher (worker.mjs, progressPort): a worker of its own, since the engine's
// worker is inside the proof's synchronous call while the count changes. It reads the striped
// commitments the proof has started (src/progress.rs) from the module's shared memory every 100 ms
// and posts the count on the page's port each time it changes. A file of its own, not a blob: URL,
// for pages that allow only their own workers (the extension's CSP: worker-src 'self').
onmessage = ({ data: { memory, address, port } }) => {
  const count = new Int32Array(memory.buffer, address, 1);
  let last = -1;
  postMessage("ready");
  setInterval(() => {
    const now = Atomics.load(count, 0);
    if (now !== last) port.postMessage((last = now));
  }, 100);
};
