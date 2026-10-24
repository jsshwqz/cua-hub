// Anthropic Messages stub that enforces the documented pairing rule: every
// tool_use block in a request must have a matching tool_result, or the API
// answers 400. That rule is the whole of issue #9, so the failure and its fix
// can be reproduced without a model key.
//
//   node tests/anthropic-pairing/stub.cjs            # listens on 127.0.0.1:8899
//   STUB_PORT=9001 STUB_LOG=/tmp/requests.jsonl node ...
const http = require("http");
const fs = require("fs");

const LOG = process.env.STUB_LOG || "stub-requests.jsonl";
const PORT = Number(process.env.STUB_PORT || 8899);

// Scripted turns: a tool the vision engine cannot execute, then a completion.
const turns = [
  {
    content: [
      { type: "text", text: "Let me press the mouse button down." },
      {
        type: "tool_use",
        id: "tu_down_1",
        name: "computer",
        input: { action: "left_mouse_down" },
      },
    ],
    stop_reason: "tool_use",
  },
  {
    content: [{ type: "text", text: "TASK_COMPLETE: recovered after the rejected call" }],
    stop_reason: "end_turn",
  },
];

let turn = 0;

function scan(messages) {
  const uses = [];
  const results = [];
  for (const message of messages) {
    const blocks = Array.isArray(message.content) ? message.content : [];
    for (const block of blocks) {
      if (block && block.type === "tool_use") uses.push(block.id);
      if (block && block.type === "tool_result") results.push(block.tool_use_id);
    }
  }
  return { uses, results };
}

fs.writeFileSync(LOG, "");

http
  .createServer((req, res) => {
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => {
      let json;
      try {
        json = JSON.parse(body || "{}");
      } catch {
        res.writeHead(400).end("{}");
        return;
      }
      const { uses, results } = scan(json.messages || []);
      const unanswered = uses.filter((id) => !results.includes(id));
      fs.appendFileSync(
        LOG,
        JSON.stringify({ step: turn, tool_uses: uses, tool_results: results, unanswered }) + "\n"
      );

      res.setHeader("content-type", "application/json");
      if (unanswered.length > 0) {
        res.writeHead(400);
        res.end(
          JSON.stringify({
            type: "error",
            error: {
              type: "invalid_request_error",
              message: `messages.${json.messages.length - 1}: each tool_use block must have a corresponding tool_result block in the next message`,
            },
          })
        );
        return;
      }
      const scripted = turns[Math.min(turn, turns.length - 1)];
      turn += 1;
      res.writeHead(200);
      res.end(
        JSON.stringify({
          id: "msg_stub",
          type: "message",
          role: "assistant",
          model: json.model,
          content: scripted.content,
          stop_reason: scripted.stop_reason,
          usage: { input_tokens: 10, output_tokens: 10 },
        })
      );
    });
  })
  .listen(PORT, "127.0.0.1", () => console.log(`anthropic pairing stub on http://127.0.0.1:${PORT}`));
