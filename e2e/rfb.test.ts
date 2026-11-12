import { expect, test } from "bun:test";
import { ask, serves } from "./rfb";

const LIMIT = 120_000;

test("a banner that comes 47 s after the first try, after two silent connections, counts then and not before", () => {
  expect(serves({ kind: "silent" }, 10_000, LIMIT)).toBe(false);
  expect(serves({ kind: "silent" }, 21_000, LIMIT)).toBe(false);
  expect(serves({ kind: "banner", text: "RFB 003.889\n" }, 47_000, LIMIT)).toBe(true);
});

test("a connection that is still silent after 121 s fails as a VM that is not ready", () => {
  expect(() => serves({ kind: "silent" }, 121_000, LIMIT)).toThrow(
    "the VM is not ready: Screen Sharing took a connection on TCP 5900, and sent no RFB banner for 121 s",
  );
});

test("a port that is closed is tried again, and fails after 120 s with the reason", () => {
  expect(serves({ kind: "closed", why: "ECONNREFUSED" }, 3_000, LIMIT)).toBe(false);
  expect(() => serves({ kind: "closed", why: "ECONNREFUSED" }, 120_000, LIMIT)).toThrow(
    "the VM is not ready: TCP 5900 took no connection for 120 s (ECONNREFUSED)",
  );
});

test("the banner of an ssh server on the port fails at once", () => {
  expect(() => serves({ kind: "banner", text: "SSH-2.0-OpenS" }, 0, LIMIT)).toThrow(
    'the VM is not ready: TCP 5900 answered "SSH-2.0-OpenS", which is no RFB banner',
  );
});

/** A local server that sends `banner` after `delayMs`, or nothing. */
function server(banner?: string, delayMs = 0) {
  return Bun.listen({
    hostname: "127.0.0.1",
    port: 0,
    socket: {
      open(socket) {
        if (banner !== undefined) setTimeout(() => socket.write(banner), delayMs);
      },
      data() {},
    },
  });
}

test("one connection reads a banner that comes after 200 ms, and gives up on a silent server after 300 ms", async () => {
  const late = server("RFB 003.889\n", 200);
  const silent = server();
  try {
    expect(await ask("127.0.0.1", late.port, 5_000)).toEqual({ kind: "banner", text: "RFB 003.889\n" });
    expect(await ask("127.0.0.1", silent.port, 300)).toEqual({ kind: "silent" });
  } finally {
    late.stop(true);
    silent.stop(true);
  }
});

test("one connection to a closed port reports that the port refused it", async () => {
  const once = server();
  const port = once.port;
  once.stop(true);
  expect(await ask("127.0.0.1", port, 5_000)).toEqual({ kind: "closed", why: "ECONNREFUSED" });
});
