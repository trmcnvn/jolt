// Jolt-owned MCP bridge for Pi. This file is embedded in jolt-harness and
// materialized to a process-lifetime temporary file for explicit `--extension` loading.

const URL_ENV = "JOLT_MCP_URL";
const TOKEN_ENV = "JOLT_MCP_BEARER_TOKEN";
const PROTOCOL_VERSION = "2025-03-26";

export default function joltMcpExtension(pi) {
  const url = process.env[URL_ENV];
  const token = process.env[TOKEN_ENV];
  delete process.env[URL_ENV];
  delete process.env[TOKEN_ENV];

  const registered = new Set();
  let session;
  let protocolVersion = PROTOCOL_VERSION;
  let nextId = 1;

  pi.on("session_start", async () => {
    if (!url || !token) return;

    session?.abort();
    session = new AbortController();

    try {
      const initialized = await post(
        url,
        token,
        {
          jsonrpc: "2.0",
          id: nextId++,
          method: "initialize",
          params: {
            protocolVersion: PROTOCOL_VERSION,
            capabilities: {},
            clientInfo: { name: "jolt-pi", version: "1" },
          },
        },
        session.signal,
      );
      protocolVersion = initialized?.result?.protocolVersion ?? PROTOCOL_VERSION;
      await post(
        url,
        token,
        { jsonrpc: "2.0", method: "notifications/initialized" },
        session.signal,
        protocolVersion,
      );
      if (initialized?.result?.capabilities?.tools) {
        const tools = await listTools(
          url,
          token,
          session.signal,
          protocolVersion,
          () => nextId++,
        );
        registerTools(pi, tools, registered, async (tool, params, signal) => {
          const response = await post(
            url,
            token,
            {
              jsonrpc: "2.0",
              id: nextId++,
              method: "tools/call",
              params: { name: tool.name, arguments: params ?? {} },
            },
            combineSignals(signal, session?.signal),
            protocolVersion,
          );
          return piToolResult(tool, response.result);
        });
      }
    } catch (error) {
      if (error?.name !== "AbortError") {
        console.error(`[jolt-mcp] unavailable: ${safeError(error)}`);
      }
    }
  });

  pi.on("session_shutdown", () => {
    session?.abort();
    session = undefined;
  });
}

async function listTools(url, token, signal, protocolVersion, id) {
  const tools = [];
  let cursor;
  do {
    const response = await post(
      url,
      token,
      {
        jsonrpc: "2.0",
        id: id(),
        method: "tools/list",
        params: cursor ? { cursor } : {},
      },
      signal,
      protocolVersion,
    );
    tools.push(...(response.result?.tools ?? []));
    cursor = response.result?.nextCursor;
  } while (cursor);
  return tools;
}

function registerTools(pi, tools, registered, execute) {
  const existing = new Set(pi.getAllTools().map((tool) => tool.name));
  const activated = [];
  for (const tool of tools) {
    const name = piToolName(tool?.name);
    if (!name || existing.has(name) || registered.has(name)) continue;
    pi.registerTool({
      name,
      label: tool.title || `Jolt ${tool.name}`,
      description: tool.description || `Call Jolt tool ${tool.name}`,
      promptSnippet: tool.description || `Call Jolt tool ${tool.name}`,
      parameters: tool.inputSchema ?? { type: "object", properties: {} },
      async execute(_toolCallId, params, signal) {
        return execute(tool, params, signal);
      },
    });
    registered.add(name);
    existing.add(name);
    activated.push(name);
  }
  if (activated.length > 0) {
    pi.setActiveTools([...new Set([...pi.getActiveTools(), ...activated])]);
  }
}

function piToolName(name) {
  if (typeof name !== "string" || !name.trim()) return undefined;
  const normalized = name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9_]+/g, "_")
    .replace(/^_+|_+$/g, "");
  return normalized ? `jolt_${normalized}` : undefined;
}

function piToolResult(tool, result) {
  const content = [];
  for (const block of result?.content ?? []) {
    if (block?.type === "text" && typeof block.text === "string") {
      content.push({ type: "text", text: block.text });
    } else if (
      block?.type === "image" &&
      typeof block.data === "string" &&
      typeof block.mimeType === "string"
    ) {
      content.push({ type: "image", data: block.data, mimeType: block.mimeType });
    } else if (block != null) {
      content.push({ type: "text", text: JSON.stringify(block) });
    }
  }
  if (content.length === 0 && result?.structuredContent !== undefined) {
    content.push({ type: "text", text: JSON.stringify(result.structuredContent) });
  }
  if (result?.isError) {
    const message = content
      .filter((block) => block.type === "text")
      .map((block) => block.text)
      .join("\n");
    throw new Error(message || `Jolt tool ${tool.name} failed`);
  }
  if (content.length === 0) content.push({ type: "text", text: "Done" });
  return {
    content,
    details: { server: "jolt", tool: tool.name, structuredContent: result?.structuredContent },
  };
}

function combineSignals(first, second) {
  const signals = [first, second].filter(Boolean);
  if (signals.length === 0) return undefined;
  if (signals.length === 1) return signals[0];
  return AbortSignal.any(signals);
}

async function post(url, token, message, signal, protocolVersion) {
  const headers = {
    Accept: "application/json, text/event-stream",
    Authorization: `Bearer ${token}`,
    "Content-Type": "application/json",
  };
  if (protocolVersion) headers["MCP-Protocol-Version"] = protocolVersion;

  const response = await fetch(url, {
    method: "POST",
    headers,
    body: JSON.stringify(message),
    signal,
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  if (message.id === undefined) return undefined;

  const body = await response.json();
  if (body?.error) throw new Error(body.error.message || "MCP request failed");
  if (!body?.result) throw new Error("invalid MCP response");
  return body;
}

function safeError(error) {
  return error instanceof Error ? `${error.name}: ${error.message}` : `thrown ${typeof error}`;
}
