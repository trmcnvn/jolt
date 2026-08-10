import { describe, expect, it } from "vitest";
import { parseSubmittedHubCommand } from "./session-hub";

const runPayload = {
  kind: "run",
  request: {
    prompt: "hello",
    model: null,
    reasoning: null,
    cwd: "/repo",
    sandbox: "workspace-write",
    resume: null
  },
  messageId: "message-1"
};

describe("SessionHub command boundary", () => {
  it("parses the complete typed command envelope", () => {
    expect(parseSubmittedHubCommand({
      id: "command-1",
      kind: "run",
      payload: runPayload,
      issuedBy: "device-a",
      issuedAt: 100,
      expiresAt: 200,
      basedOn: { turnId: "turn-1", frontier: null }
    })).toEqual({
      id: "command-1",
      kind: "run",
      payload: runPayload,
      issuedBy: "device-a",
      issuedAt: 100,
      expiresAt: 200,
      basedOn: { turnId: "turn-1", frontier: null }
    });
  });

  it("accepts every typed session command variant", () => {
    const variants = [
      ["run", runPayload],
      ["run", { kind: "hiddenPrompt", request: runPayload.request }],
      ["queue", { kind: "queue", request: runPayload.request, messageId: "message-queue" }],
      ["resumeQueue", { kind: "resumeQueue" }],
      ["bash", {
        kind: "bash", command: "pwd", excludeFromContext: false, cwd: "/repo",
        messageId: "message-bash"
      }],
      ["steer", { kind: "steer", prompt: "/extension", messageId: "message-steer" }],
      ["interrupt", { kind: "interrupt" }],
      ["respondInput", {
        kind: "respondInput", requestId: "request-1",
        answers: [{ questionId: "question-1", labels: ["yes"] }]
      }],
      ["goal", { kind: "goal", operation: { action: "create", objective: "ship it" } }]
    ] as const;
    for (const [index, [kind, payload]] of variants.entries()) {
      expect(parseSubmittedHubCommand({
        id: `command-${index}`,
        kind,
        payload,
        issuedBy: "device-a",
        issuedAt: 100,
        expiresAt: 200
      }), `${kind}/${payload.kind}`).toBeDefined();
    }
  });

  it("accepts mobile run requests with omitted optional fields", () => {
    const payload = {
      kind: "run",
      request: {
        prompt: "hello",
        modelOptions: {},
        cwd: "/repo",
        sandbox: "workspace-write",
        autoApprove: true,
        attachments: []
      },
      messageId: "message-mobile"
    };
    expect(parseSubmittedHubCommand({
      id: "command-mobile",
      kind: "run",
      payload,
      issuedBy: "device-mobile",
      issuedAt: 100,
      expiresAt: 200
    })?.payload).toEqual(payload);
  });

  it("rejects malformed, unbounded, and non-finite commands", () => {
    expect(parseSubmittedHubCommand(null)).toBeUndefined();
    expect(parseSubmittedHubCommand({
      id: "bad id",
      kind: "run",
      payload: {},
      issuedBy: "device-a",
      issuedAt: 100,
      expiresAt: 200
    })).toBeUndefined();
    expect(parseSubmittedHubCommand({
      id: "command-1",
      kind: "run",
      payload: {},
      issuedBy: "device-a",
      issuedAt: Number.NaN,
      expiresAt: 200
    })).toBeUndefined();
    expect(parseSubmittedHubCommand({
      id: "command-1",
      kind: "interrupt",
      payload: runPayload,
      issuedBy: "device-a",
      issuedAt: 100,
      expiresAt: 200
    })).toBeUndefined();
    expect(parseSubmittedHubCommand({
      id: "command-1",
      kind: "run",
      payload: { ...runPayload, request: { ...runPayload.request, prompt: "x".repeat(600 * 1024) } },
      issuedBy: "device-a",
      issuedAt: 100,
      expiresAt: 200
    })).toBeUndefined();
  });
});
