// Tells when Screen Sharing in the VM serves a viewer: it sends its RFB
// banner on TCP 5900. launchd holds the port and starts screensharingd on
// the first connection, so an open port alone proves nothing.

import { connect } from "node:net";

/** The control port of Screen Sharing, where pippin connects. */
const PORT = 5900;
/** "RFB 003.889\n": pippin reads these 12 bytes first (src/proto/handshake.rs). */
const BANNER_BYTES = 12;
// One connection waits this long for the banner. Then a new one takes its
// place, because a connection that the VM lost never gets one.
const TRY_MS = 10_000;
const PAUSE_MS = 1000;

/** What one connection to the port got in its time. */
export type Answer =
  | { kind: "closed"; why: string } // no connection
  | { kind: "silent" } // a connection, and no banner
  | { kind: "banner"; text: string };

/**
 * Whether `answer` shows that Screen Sharing serves, `waitedMs` after the
 * first try. False means: try again. It throws when the VM is not ready
 * within `limitMs`, or when another program answers on the port.
 */
export function serves(answer: Answer, waitedMs: number, limitMs: number): boolean {
  if (answer.kind === "banner") {
    if (answer.text.startsWith("RFB ")) return true;
    throw new Error(`the VM is not ready: TCP ${PORT} answered ${JSON.stringify(answer.text)}, which is no RFB banner`);
  }
  if (waitedMs < limitMs) return false;
  const waited = `${Math.round(waitedMs / 1000)} s`;
  throw new Error(
    answer.kind === "closed"
      ? `the VM is not ready: TCP ${PORT} took no connection for ${waited} (${answer.why})`
      : `the VM is not ready: Screen Sharing took a connection on TCP ${PORT}, and sent no RFB banner for ${waited}`,
  );
}

/** Connects once, and reads the banner. It ends after `timeoutMs` at the latest. */
export function ask(host: string, port: number, timeoutMs: number): Promise<Answer> {
  return new Promise((resolve) => {
    const socket = connect({ host, port });
    let open = false;
    let got = Buffer.alloc(0);
    const end = (answer: Answer) => {
      clearTimeout(timer);
      socket.destroy();
      resolve(answer);
    };
    const none = (why: string): Answer => (open ? { kind: "silent" } : { kind: "closed", why });
    const timer = setTimeout(() => end(none("no answer")), timeoutMs);
    socket.on("connect", () => (open = true));
    socket.on("data", (chunk: Buffer) => {
      got = Buffer.concat([got, chunk]);
      if (got.length >= BANNER_BYTES) end({ kind: "banner", text: got.toString("latin1", 0, BANNER_BYTES) });
    });
    socket.on("error", (e: NodeJS.ErrnoException) => end(none(e.code ?? e.message)));
    socket.on("close", () => end(none("closed")));
  });
}

/**
 * Calls `once` for one connection after the other until its answer serves,
 * and returns the time that it took. Each call gets its time: what is left
 * of `limitMs`, at most TRY_MS, and at least 1 s, like waitFor.
 */
export async function waitServes(once: (timeoutMs: number) => Promise<Answer>, limitMs: number): Promise<number> {
  const begin = Date.now();
  const waited = () => Date.now() - begin;
  while (!serves(await once(Math.min(TRY_MS, Math.max(limitMs - waited(), 1000))), waited(), limitMs)) {
    await Bun.sleep(PAUSE_MS);
  }
  return waited();
}

/**
 * Waits until Screen Sharing at `host` sends its RFB banner, and returns the
 * time that it took. Throws when it sends none within `limitMs`.
 */
export function waitBanner(host: string, limitMs: number): Promise<number> {
  return waitServes((timeoutMs) => ask(host, PORT, timeoutMs), limitMs);
}
