import { expect, test } from "bun:test";
import { ask, serves, waitServes, type Answer } from "./rfb";

const LIMIT = 120_000;

test("a silent connection means try again, and an RFB banner 47 s after the first try means ready", () => {
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

/** Answers one connection after the other from `answers`, and notes the time that each got. */
function scripted(answers: Answer[], times: number[]) {
  return async (timeoutMs: number) => {
    times.push(timeoutMs);
    return answers.shift()!;
  };
}

test("a banner on the third connection, after a closed port and a silent one, ends the wait, and each connection gets at most what is left of 2.5 s", async () => {
  const times: number[] = [];
  const answers: Answer[] = [{ kind: "closed", why: "ECONNREFUSED" }, { kind: "silent" }, { kind: "banner", text: "RFB 003.889\n" }];
  const waited = await waitServes(scripted(answers, times), 2_500);
  expect(times.length).toBe(3);
  expect(times[0]).toBeLessThanOrEqual(2_500);
  expect(times[1]).toBeLessThanOrEqual(1_500);
  expect(times[2]).toBe(1_000);
  expect(waited).toBeGreaterThanOrEqual(2_000);
});

test("connections that stay silent past a limit of 1.5 s end the wait with the error, after one last connection of 1 s", async () => {
  const times: number[] = [];
  const silent: Answer[] = Array.from({ length: 5 }, () => ({ kind: "silent" }));
  await expect(waitServes(scripted(silent, times), 1_500)).rejects.toThrow("sent no RFB banner for");
  expect(times.length).toBe(3);
  expect(times[2]).toBe(1_000);
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
