// Rayline hybrid event adapter for Pi. Written by `rayline hybrid run pi`.
//
// Pi cannot add per-request headers, so this extension states what just
// happened in a top-level `rayline_event` member of each provider payload. The
// Rayline execution core removes it before the request reaches Decisions or a
// model. The core reports the event; the ARC policy decides what it means.
//
//   task_start    first provider request of the session
//   user_message  first request after a typed (or queued) user prompt
//   tool_result   any other request in the agent loop
//   compaction    first request after a compaction: summary index, retained
//                 tail start (or null) and new: true
//
// A provider retry of an identical transcript keeps its original event.

const COMPACTION_PREFIX =
  "The conversation history before this point was compacted into the following summary:";

type Message = { role?: string; content?: unknown };

function isCompactionSummary(message: Message): boolean {
  if (message?.role !== "user") return false;
  const content = message.content;
  if (typeof content === "string") return content.startsWith(COMPACTION_PREFIX);
  if (Array.isArray(content)) {
    const first = content[0] as { type?: string; text?: string } | undefined;
    return first?.type === "text" && typeof first.text === "string" && first.text.startsWith(COMPACTION_PREFIX);
  }
  return false;
}

function transcriptKey(messages: Message[]): string {
  // Cache directives move between requests; they are not part of identity.
  return JSON.stringify(messages, (key, value) => (key === "cache_control" ? undefined : value));
}

export default function (pi: any) {
  let started = false;
  let typed = false;
  let compacted = false;
  let lastKey: string | undefined;
  let lastEvent: unknown;

  pi.on("session_start", (event: { reason?: string }) => {
    // A resumed or forked session already has history: never a task start.
    started = event?.reason === "resume" || event?.reason === "fork";
    typed = false;
    compacted = false;
    lastKey = undefined;
    lastEvent = undefined;
  });
  pi.on("input", () => {
    typed = true;
  });
  pi.on("session_compact", () => {
    compacted = true;
  });
  pi.on("before_provider_request", (event: { payload?: any }) => {
    const payload = event?.payload;
    if (!payload || typeof payload !== "object" || !Array.isArray(payload.messages)) return undefined;
    const messages = payload.messages as Message[];
    const key = transcriptKey(messages);
    if (key === lastKey && lastEvent !== undefined) {
      return { ...payload, rayline_event: lastEvent };
    }
    let next: unknown;
    if (compacted) {
      const summary = messages.findIndex(isCompactionSummary);
      if (summary >= 0) {
        // A new prompt after /compact is input, not retained history.
        const lastRetained = typed ? messages.length - 2 : messages.length - 1;
        const retainedFrom = summary + 1 <= lastRetained ? summary + 1 : null;
        next = { kind: "compaction", compaction: { summary_message: summary, retained_from: retainedFrom, new: true } };
      }
      compacted = false;
    }
    if (next === undefined) {
      next = { kind: !started ? "task_start" : typed ? "user_message" : "tool_result" };
    }
    started = true;
    typed = false;
    lastKey = key;
    lastEvent = next;
    return { ...payload, rayline_event: next };
  });
}
