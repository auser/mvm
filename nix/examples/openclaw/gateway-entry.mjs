// Entry point wrapper for OpenClaw gateway in mvm microVMs.
//
// Fixes the double-start bug: runGatewayLoop fires two concurrent
// startGatewayServer() calls; the loser's lock check calls process.exit(1),
// killing the working server. We detect when the gateway is running and
// suppress both the exit AND the noise from the losing attempt.
//
// IMPORT_PATH is replaced at build time with the actual openclaw module path.
import { runCli } from "IMPORT_PATH";

process.env.OPENCLAW_NODE_OPTIONS_READY = "1";

const realExit = process.exit;
let gatewayRunning = false;

// Conflict patterns from the losing start attempt.
const CONFLICT_PATTERNS = [
  "already running",
  "already listening",
  "already in use",
  "lock timeout",
  "Gateway failed to start",
];

for (const stream of [process.stdout, process.stderr]) {
  const orig = stream.write.bind(stream);
  stream.write = function (chunk, ...args) {
    const s = typeof chunk === "string" ? chunk : "";

    // Detect successful startup — set suppression flag early.
    if (s.includes("listening on ws://")) {
      gatewayRunning = true;
    }

    // Once running, suppress noise from the losing concurrent attempt.
    if (gatewayRunning && CONFLICT_PATTERNS.some((p) => s.includes(p))) {
      return true; // swallow the message
    }

    return orig(chunk, ...args);
  };
}

process.exit = (code) => {
  if (code !== 0 && gatewayRunning) return;
  realExit(code);
};

runCli(process.argv).catch((err) => {
  if (!gatewayRunning) realExit(1);
});
