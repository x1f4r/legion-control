// Incremental Server-Sent Events parser for POST /v1/call text/event-stream responses.

export function createSseParser(onFrame) {
  let buffer = '';

  function dispatchBlock(block) {
    if (!block) return;
    const lines = block.split('\n');
    let eventName = 'message';
    const dataLines = [];
    for (const rawLine of lines) {
      const line = rawLine.endsWith('\r') ? rawLine.slice(0, -1) : rawLine;
      if (line === '' || line.startsWith(':')) continue;
      const colon = line.indexOf(':');
      const field = colon === -1 ? line : line.slice(0, colon);
      let value = colon === -1 ? '' : line.slice(colon + 1);
      if (value.startsWith(' ')) value = value.slice(1);
      if (field === 'event') eventName = value;
      else if (field === 'data') dataLines.push(value);
    }
    if (dataLines.length === 0) return;
    const rawData = dataLines.join('\n');
    let parsed = rawData;
    try {
      parsed = JSON.parse(rawData);
    } catch {
      // Keep raw string when a frame carries non-JSON text.
    }
    onFrame({ event: eventName, data: parsed, raw: rawData });
  }

  return {
    push(chunk) {
      if (!chunk) return;
      buffer += chunk.replace(/\r\n/g, '\n');
      let sep = buffer.indexOf('\n\n');
      while (sep !== -1) {
        const block = buffer.slice(0, sep);
        buffer = buffer.slice(sep + 2);
        dispatchBlock(block);
        sep = buffer.indexOf('\n\n');
      }
    },
    finish() {
      if (buffer.trim()) {
        dispatchBlock(buffer);
      }
      buffer = '';
    },
  };
}
