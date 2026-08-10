/**
 * Jolt edge Worker (docs/architecture.md): JWT auth at the
 * edge, then forwarding into per-session, registry, and per-device
 * Durable Objects. It also serves content-addressed R2 attachments and WorkOS
 * authentication routes.
 *
 * Routes:
 *   GET  /health
 *   POST /auth/exchange               — WorkOS code → tokens
 *   POST /auth/refresh                — WorkOS refresh → fresh tokens
 *   GET  /auth/orgs                   — caller's active org memberships
 *   POST /auth/orgs                   — create org + admin membership
 *   GET  /auth/cli/callback           — headless sign-in paste-code page
 *   GET  /hub/:chatId/ws              — fenced host/viewer SessionHub socket
 *   POST /hub/:chatId/command         — typed idempotent command
 *   GET  /hub/:chatId/commands        — revision-paged command reconciliation
 *   GET  /hub/:chatId/bootstrap       — bounded transcript projection
 *   PUT  /hub/:chatId/pages/:sha256   — immutable transcript page
 *   GET  /session/:chatId/ws          — legacy rollback room during cutover
 *   GET  /tail/:chatId                — legacy rollback tail during cutover
 *   GET  /diff/:chatId                — latest paged working-tree manifest
 *   GET  /diff/:chatId/page?id=       — one immutable patch page
 *   GET  /diff/:chatId/ws             — manifest update stream
 *   POST /diff/:chatId                — host publishes manifest + missing pages
 *   GET  /snapshot/:chatId            — legacy repair snapshot during cutover
 *   POST /append/:chatId              — legacy repair import during cutover
 *   GET  /registry/:orgId/ws          — workspace registry room `reg1/{orgId}/{user}` (wss)
 *   GET  /registry/:orgId/stats       — registry seq/rows/attribution
 *   GET  /registry/:orgId/rows        — registry full-table repair read
 *   POST /registry/:orgId/reset       — registry operator wipe (self-healing)
 *   GET  /device/:deviceId/ws?role=   — device-room byte pipe (§8)
 *   GET  /device/:deviceId/sidecar/:name
 *   POST /device/:deviceId/sidecar/:name
 *   GET  /device/:deviceId/status
 *   PUT  /attachments/:chatId/:sha256 — chat-scoped upload
 *   GET  /attachments/:chatId/:sha256
 *   HEAD /attachments/:chatId/:sha256
 */
import { authenticate } from "./auth";
import { handleAuthRoute } from "./auth-routes";
import { AUTH_USER_HEADER, type Env } from "./env";
import { SessionRoom } from "./session-room";
import { SessionHub } from "./session-hub";
import { DeviceRoom } from "./device-room";
import { RegistryRoom } from "./registry-room";
import { parseDiffSidecar, type CheckoutDiffPage, type StoredDiffSidecar } from "./session-doc";
import installSh from "./install.sh";

export { SessionRoom, SessionHub, DeviceRoom, RegistryRoom };

const ID_RE = /^[A-Za-z0-9_-]{1,128}$/;
const SHA256_RE = /^[a-f0-9]{64}$/;
const MAX_ATTACHMENT_BYTES = 32 * 1024 * 1024; // mirrors today's upload cap
const MAX_TRANSCRIPT_PAGE_BYTES = 4 * 1024 * 1024;

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });

/** Forward into a DO with the verified user stamped on the request. */
const forward = (
  ns: DurableObjectNamespace,
  name: string,
  request: Request,
  userId: string,
  path: string,
  search?: string
): Promise<Response> => {
  const stub = ns.get(ns.idFromName(name));
  const url = new URL(request.url);
  url.pathname = path;
  if (search !== undefined) url.search = search;
  const headers = new Headers(request.headers);
  headers.set(AUTH_USER_HEADER, userId);
  return stub.fetch(new Request(url.toString(), { ...requestInit(request), headers }));
};

const requestInit = (request: Request): RequestInit => ({
  method: request.method,
  body: request.body
});

/** Carry the dialing engine's `&device=` through to the DO (socket
 * attribution in logs — the 2026-08-04 deaf socket was only identifiable by
 * reverse-engineering rotating IPv6 privacy addresses). Validated so a
 * hand-crafted value can't inject into log lines or the DO's query. */
const deviceParam = (url: URL): string => {
  const device = url.searchParams.get("device") ?? "";
  return ID_RE.test(device) ? `&device=${device}` : "";
};

const orgMismatch = ({
  route,
  requestedOrgId,
  tokenOrgId,
  url
}: {
  route: "registry";
  requestedOrgId: string;
  tokenOrgId: string | undefined;
  url: URL;
}): Response => {
  const device = url.searchParams.get("device");
  console.warn("organization claim mismatch", {
    route,
    requestedOrgId,
    tokenOrgId: tokenOrgId ?? null,
    device: device && ID_RE.test(device) ? device : null
  });
  return json({ error: "forbidden", code: "org_mismatch" }, 403);
};

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const parts = url.pathname.split("/").filter(Boolean);

    if (url.pathname === "/health") {
      return json({ ok: true, auth: env.AUTH_MODE === "dev" ? "dev" : "workos" });
    }

    // ── public install surface (also routed from jolt.trmcnvn.dev): the
    //    `curl | sh` installer and the release artifacts it downloads ───────
    if (url.pathname === "/install.sh" && (request.method === "GET" || request.method === "HEAD")) {
      return new Response(request.method === "HEAD" ? null : installSh, {
        headers: {
          "content-type": "application/x-sh",
          "cache-control": "public, max-age=0, must-revalidate"
        }
      });
    }
    if (
      parts[0] === "releases" &&
      parts.length >= 2 &&
      (request.method === "GET" || request.method === "HEAD")
    ) {
      const key = decodeURIComponent(url.pathname.slice("/releases/".length));
      if (key.length === 0 || key.includes("..")) return json({ error: "bad request" }, 400);
      const object = await env.RELEASES.get(key);
      if (!object) return json({ error: "not_found" }, 404);
      // latest.txt / manifest.json flip on release; artifacts are immutable by name.
      const mutable = key.endsWith(".txt") || key.endsWith(".json");
      const headers = new Headers({
        "content-type": key.endsWith(".txt")
          ? "text/plain; charset=utf-8"
          : key.endsWith(".json")
            ? "application/json"
            : "application/octet-stream",
        "content-length": String(object.size),
        "cache-control": mutable ? "public, max-age=60" : "public, max-age=86400, immutable",
        etag: object.httpEtag
      });
      return new Response(request.method === "HEAD" ? null : object.body, { headers });
    }

    // ── WorkOS auth routes (pre-bearer: exchange/refresh/callback have no
    //    access token yet; the org routes verify the bearer themselves) ─────
    const authRouted = await handleAuthRoute(request, env, url);
    if (authRouted) return authRouted;

    const auth = await authenticate(env, request);
    if (!auth) return json({ error: "unauthenticated" }, 401);

    // ── SessionHub: typed commands + host-published transcript projection ──
    if (parts[0] === "hub" && parts[1] && ID_RE.test(parts[1])) {
      const chatId = parts[1];
      const room = `hub1/${chatId}`;
      if (parts[2] === "ws") {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
          return json({ error: "expected websocket" }, 426);
        }
        const role = url.searchParams.get("role") === "host" ? "host" : "viewer";
        const device = url.searchParams.get("device") ?? "";
        const search = `?chatId=${encodeURIComponent(chatId)}&role=${role}${
          ID_RE.test(device) ? `&device=${encodeURIComponent(device)}` : ""
        }`;
        return forward(env.SESSION_HUBS, room, request, auth.userId, "/ws", search);
      }
      if (parts[2] === "bootstrap" && request.method === "GET") {
        return forward(
          env.SESSION_HUBS,
          room,
          request,
          auth.userId,
          "/bootstrap",
          `?chatId=${encodeURIComponent(chatId)}`
        );
      }
      if (parts[2] === "commands" && parts[3] === undefined && request.method === "GET") {
        const after = url.searchParams.get("after") ?? "0";
        if (!/^\d+$/.test(after)) return json({ error: "invalid_command_cursor" }, 400);
        return forward(
          env.SESSION_HUBS,
          room,
          request,
          auth.userId,
          "/commands",
          `?chatId=${encodeURIComponent(chatId)}&after=${encodeURIComponent(after)}`
        );
      }
      if (parts[2] === "command" && parts[3] === undefined && request.method === "POST") {
        return forward(
          env.SESSION_HUBS,
          room,
          request,
          auth.userId,
          "/command",
          `?chatId=${encodeURIComponent(chatId)}`
        );
      }
      if (parts[2] === "command" && parts[3] === "cancel" && request.method === "POST") {
        return forward(
          env.SESSION_HUBS,
          room,
          request,
          auth.userId,
          "/command/cancel",
          `?chatId=${encodeURIComponent(chatId)}`
        );
      }
      if (parts[2] === "stats" && request.method === "GET") {
        return forward(env.SESSION_HUBS, room, request, auth.userId, "/stats", "");
      }
      if (parts[2] === "retire" && request.method === "POST") {
        return forward(env.SESSION_HUBS, room, request, auth.userId, "/retire", "");
      }
      if (
        parts[2] === "pages" &&
        parts[3] &&
        SHA256_RE.test(parts[3]) &&
        parts.length === 4
      ) {
        const hash = parts[3];
        const key = `transcript/${auth.userId}/${chatId}/${hash}`;
        if (request.method === "PUT") {
          const bytes = await request.arrayBuffer();
          if (bytes.byteLength > MAX_TRANSCRIPT_PAGE_BYTES) {
            return json({ error: "too_large" }, 413);
          }
          const digest = await crypto.subtle.digest("SHA-256", bytes);
          const actual = [...new Uint8Array(digest)]
            .map((byte) => byte.toString(16).padStart(2, "0"))
            .join("");
          if (actual !== hash) return json({ error: "hash_mismatch" }, 400);
          if (await env.BLOBS.head(key) === null) {
            await env.BLOBS.put(key, bytes, {
              httpMetadata: {
                contentType: request.headers.get("content-type") ?? "application/json"
              }
            });
          }
          return json({ ok: true, hash, bytes: bytes.byteLength });
        }
        if (request.method === "GET" || request.method === "HEAD") {
          const object = await env.BLOBS.get(key);
          if (!object) return json({ error: "not_found" }, 404);
          return new Response(request.method === "HEAD" ? null : object.body, {
            headers: {
              "content-type": object.httpMetadata?.contentType ?? "application/json",
              "content-length": String(object.size),
              "cache-control": "private, max-age=31536000, immutable",
              etag: object.httpEtag
            }
          });
        }
      }
    }

    // ── legacy Loro session rooms ───────────────────────────────────────────
    if (parts[0] === "session" && parts[1] && ID_RE.test(parts[1]) && parts[2] === "ws") {
      if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
        return json({ error: "expected websocket" }, 426);
      }
      // `s2/` = the WorkOS staging→production identity break: rooms are
      // claim-on-first-join per user id, and prod issued a fresh id for
      // everyone — a new namespace lets prod identities claim fresh rooms
      // while hosts re-upload doc state from their local snapshots.
      // Frame-level room ids stay the bare chatId.
      return forward(
        env.SESSION_ROOMS,
        `s2/${parts[1]}`,
        request,
        auth.userId,
        "/ws",
        `?chatId=${parts[1]}${deviceParam(url)}`
      );
    }
    if (parts[0] === "tail" && parts[1] && ID_RE.test(parts[1]) && request.method === "GET") {
      return forward(env.SESSION_ROOMS, `s2/${parts[1]}`, request, auth.userId, "/tail", "");
    }
    if (parts[0] === "transcript" && parts[1] && ID_RE.test(parts[1]) && request.method === "GET") {
      const chatId = parts[1];
      const room = `hub1/${chatId}`;
      if (parts[2] === "ws") {
        return forward(
          env.SESSION_HUBS,
          room,
          request,
          auth.userId,
          "/ws",
          `?chatId=${encodeURIComponent(chatId)}&role=viewer`
        );
      }
      if (parts[2] === "page") {
        const pageId = url.searchParams.get("id");
        if (!pageId || !ID_RE.test(pageId)) return json({ error: "invalid_page_id" }, 400);
        const bootstrapResponse = await forward(
          env.SESSION_HUBS,
          room,
          request,
          auth.userId,
          "/bootstrap",
          `?chatId=${encodeURIComponent(chatId)}`
        );
        if (!bootstrapResponse.ok) return bootstrapResponse;
        const bootstrap = await bootstrapResponse.json<{
          manifest?: { pages?: Array<{ id?: unknown; contentHash?: unknown }> };
        }>();
        const descriptor = bootstrap.manifest?.pages?.find((page) => page.id === pageId);
        if (!descriptor || typeof descriptor.contentHash !== "string" || !SHA256_RE.test(descriptor.contentHash)) {
          return json({ error: "page_not_found" }, 404);
        }
        const object = await env.BLOBS.get(
          `transcript/${auth.userId}/${chatId}/${descriptor.contentHash}`
        );
        return object
          ? new Response(object.body, {
              headers: {
                "content-type": object.httpMetadata?.contentType ?? "application/json",
                "cache-control": "private, max-age=31536000, immutable",
                etag: object.httpEtag
              }
            })
          : json({ error: "page_not_found" }, 404);
      }
      return forward(
        env.SESSION_HUBS,
        room,
        request,
        auth.userId,
        "/bootstrap",
        `?chatId=${encodeURIComponent(chatId)}`
      );
    }
    if (parts[0] === "command" && parts[1] && ID_RE.test(parts[1]) && request.method === "POST") {
      return forward(
        env.SESSION_HUBS,
        `hub1/${parts[1]}`,
        request,
        auth.userId,
        "/command",
        `?chatId=${encodeURIComponent(parts[1])}`
      );
    }
    if (parts[0] === "stats" && parts[1] && ID_RE.test(parts[1]) && request.method === "GET") {
      return forward(env.SESSION_ROOMS, `s2/${parts[1]}`, request, auth.userId, "/stats", "");
    }
    if (parts[0] === "diff" && parts[1] && ID_RE.test(parts[1])) {
      const chatId = parts[1];
      if (parts[2] === "page" && request.method === "GET") {
        const pageId = url.searchParams.get("id") ?? "";
        if (!SHA256_RE.test(pageId)) return json({ error: "invalid_page_id" }, 400);
        const manifestResponse = await forward(
          env.SESSION_HUBS,
          `hub1/${chatId}`,
          request,
          auth.userId,
          "/diff",
          `?chatId=${encodeURIComponent(chatId)}`
        );
        if (!manifestResponse.ok) return manifestResponse;
        const stored = await manifestResponse.json<StoredDiffSidecar>();
        if (!stored.manifest.pages.some((page) => page.id === pageId)) {
          return json({ error: "page_not_found" }, 404);
        }
        const object = await env.BLOBS.get(
          `diff-pages/${auth.userId}/${stored.manifest.checkoutId}/${pageId}`
        );
        return object ? new Response(object.body, { headers: { "content-type": "application/json" } }) : json({ error: "page_not_found" }, 404);
      }
      if (request.method === "POST" && parts[2] === undefined) {
        const sidecar = parseDiffSidecar(await request.clone().json());
        if (!sidecar
          || sidecar.chatId !== chatId
          || !ID_RE.test(sidecar.deviceId)
          || sidecar.deviceId !== sidecar.manifest.deviceId
          || !ID_RE.test(sidecar.manifest.checkoutId)) {
          return json({ error: "invalid_diff_sidecar" }, 400);
        }
        const prefix = `diff-pages/${auth.userId}/${sidecar.manifest.checkoutId}`;
        const previousIndex = await env.BLOBS.get(`${prefix}/manifest.json`);
        const previousPageIds = previousIndex
          ? await previousIndex.json<{ pageIds: string[] }>().then((value) => value.pageIds).catch(() => [])
          : [];
        for (const page of sidecar.pages) {
          const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(page.patch));
          const hash = [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
          if (hash !== page.id) return json({ error: "diff_page_hash_mismatch" }, 400);
          const key = `${prefix}/${page.id}`;
          if (await env.BLOBS.head(key) === null) {
            await env.BLOBS.put(key, JSON.stringify(page satisfies CheckoutDiffPage), {
              httpMetadata: { contentType: "application/json" },
              customMetadata: { checkoutId: sidecar.manifest.checkoutId }
            });
          }
        }
        const currentPageIds = sidecar.manifest.pages.map((page) => page.id);
        await env.BLOBS.put(`${prefix}/manifest.json`, JSON.stringify({ pageIds: currentPageIds }), {
          httpMetadata: { contentType: "application/json" }
        });
        await Promise.all(previousPageIds
          .filter((pageId) => !currentPageIds.includes(pageId))
          .map((pageId) => env.BLOBS.delete(`${prefix}/${pageId}`)));
        const stored: StoredDiffSidecar = { ...sidecar, pages: [] };
        const forwarded = new Request(request.url, {
          method: "POST",
          headers: request.headers,
          body: JSON.stringify(stored)
        });
        return forward(
          env.SESSION_HUBS,
          `hub1/${chatId}`,
          forwarded,
          auth.userId,
          "/diff",
          `?chatId=${encodeURIComponent(chatId)}`
        );
      }
      const path = parts[2] === "ws" ? "/diff/ws" : "/diff";
      return forward(
        env.SESSION_HUBS,
        `hub1/${chatId}`,
        request,
        auth.userId,
        path,
        `?chatId=${encodeURIComponent(chatId)}`
      );
    }
    if (parts[0] === "snapshot" && parts[1] && ID_RE.test(parts[1]) && request.method === "GET") {
      return forward(env.SESSION_ROOMS, `s2/${parts[1]}`, request, auth.userId, "/snapshot", "");
    }
    if (parts[0] === "append" && parts[1] && ID_RE.test(parts[1]) && request.method === "POST") {
      return forward(env.SESSION_ROOMS, `s2/${parts[1]}`, request, auth.userId, "/append", "");
    }

    // ── registry rooms (docs/sync.md): org claim must match the URL,
    //    room derived from the caller's OWN user id, and the DO
    //    trusts the stamped header. `reg1` = first registry generation. ─────
    if (parts[0] === "registry" && parts[1] && ID_RE.test(parts[1])) {
      const orgId = parts[1];
      if (auth.orgId !== orgId) {
        return orgMismatch({ route: "registry", requestedOrgId: orgId, tokenOrgId: auth.orgId, url });
      }
      const room = `reg1/${orgId}/${auth.userId}`;
      if (parts[2] === "ws") {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
          return json({ error: "expected websocket" }, 426);
        }
        return forward(
          env.REGISTRY_ROOMS,
          room,
          request,
          auth.userId,
          "/ws",
          `?${deviceParam(url).replace(/^&/, "")}`
        );
      }
      if (parts[2] === "stats" && request.method === "GET") {
        return forward(env.REGISTRY_ROOMS, room, request, auth.userId, "/stats", "");
      }
      // Repair/inspection read: the full current row table.
      if (parts[2] === "rows" && request.method === "GET") {
        return forward(env.REGISTRY_ROOMS, room, request, auth.userId, "/rows", "");
      }
      // Operator wipe. Unlike the CRDT rooms this needs no recipe: clients
      // detect the seq regression on their next hello and re-seed the table
      // from local rows with original clocks, automatically.
      if (parts[2] === "reset" && request.method === "POST") {
        return forward(env.REGISTRY_ROOMS, room, request, auth.userId, "/reset", "");
      }
    }

    // ── device rooms ────────────────────────────────────────────────────────
    if (parts[0] === "device" && parts[1] && ID_RE.test(parts[1])) {
      const deviceId = parts[1];
      if (parts[2] === "ws") {
        if (request.headers.get("upgrade")?.toLowerCase() !== "websocket") {
          return json({ error: "expected websocket" }, 426);
        }
        const role = url.searchParams.get("role") === "host" ? "host" : "client";
        const connId = url.searchParams.get("connId") ?? crypto.randomUUID();
        // `d2/` — same staging→prod identity break as `s2/` above.
        return forward(
          env.DEVICE_ROOMS,
          `d2/${deviceId}`,
          request,
          auth.userId,
          "/ws",
          `?role=${role}&connId=${encodeURIComponent(connId)}`
        );
      }
      if (parts[2] === "sidecar" && parts[3] && /^[a-z0-9-]{1,64}$/.test(parts[3])) {
        return forward(env.DEVICE_ROOMS, `d2/${deviceId}`, request, auth.userId, `/sidecar/${parts[3]}`, "");
      }
      if (parts[2] === "status") {
        return forward(env.DEVICE_ROOMS, `d2/${deviceId}`, request, auth.userId, "/status", "");
      }
      // Durable command nudge (§7): "chat X has pending commands — open its
      // doc". Delivered live if the host is connected, else queued in the DO
      // and replayed on the host's next join.
      if (parts[2] === "nudge" && request.method === "POST") {
        return forward(env.DEVICE_ROOMS, `d2/${deviceId}`, request, auth.userId, "/nudge", "");
      }
    }

    // ── R2 attachments (§1.2): content-addressed within one chat. The chat
    // prefix makes every blob owned by the registry row whose deletion purges
    // it; cross-chat dedupe is deliberately traded for unambiguous cleanup.
    if (
      parts[0] === "attachments" &&
      parts.length === 3 &&
      parts[1] &&
      ID_RE.test(parts[1]) &&
      parts[2] &&
      SHA256_RE.test(parts[2])
    ) {
      const chatId = parts[1];
      const hash = parts[2];
      const key = `att/${auth.userId}/${chatId}/${hash}`;
      if (request.method === "PUT") {
        const body = await request.arrayBuffer();
        if (body.byteLength > MAX_ATTACHMENT_BYTES) return json({ error: "too_large" }, 413);
        const digest = await crypto.subtle.digest("SHA-256", body);
        const hex = [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
        if (hex !== hash) return json({ error: "hash_mismatch" }, 400);
        await env.BLOBS.put(key, body, {
          httpMetadata: {
            contentType: request.headers.get("content-type") ?? "application/octet-stream"
          }
        });
        return json({ ok: true, hash: hex, bytes: body.byteLength });
      }
      if (request.method === "GET" || request.method === "HEAD") {
        const object =
          request.method === "GET" ? await env.BLOBS.get(key) : await env.BLOBS.head(key);
        if (!object) return json({ error: "not_found" }, 404);
        const headers = new Headers();
        object.writeHttpMetadata(headers);
        headers.set("etag", object.httpEtag);
        headers.set("cache-control", "private, max-age=31536000, immutable");
        const body =
          request.method === "GET" && "body" in object ? (object as R2ObjectBody).body : null;
        return new Response(body, { headers });
      }
    }

    return json({ error: "not_found" }, 404);
  }
} satisfies ExportedHandler<Env>;
