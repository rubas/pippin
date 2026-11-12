import { afterAll, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { held, Session } from "./pippin";

// A private directory for the FIFOs: a CI runner sets no XDG_RUNTIME_DIR.
const tmp = mkdtempSync(`${tmpdir()}/pippin-e2e-test-`);
const runtime = `${tmp}/run`;
mkdirSync(runtime, { mode: 0o700 });
process.env.XDG_RUNTIME_DIR = runtime;
afterAll(() => rmSync(tmp, { recursive: true, force: true }));

function outDir(name: string): string {
  const dir = `${tmp}/${name}`;
  mkdirSync(dir);
  return dir;
}

test("the child gets only PIPPIN_CONTROL_FIFO, not the PIPPIN_DUMP_HEVC of the runner's shell, and the password on stdin", async () => {
  process.env.PIPPIN_DUMP_HEVC = `${tmp}/capture.hevc`;
  try {
    const env = outDir("env");
    expect(await Session.launch(["/usr/bin/env"], "", env).quit()).toBe(0);
    expect(readFileSync(`${env}/pippin.log`, "utf8")).toBe(`PIPPIN_CONTROL_FIFO=${runtime}/pippin-e2e-${process.pid}.ctl\n`);
    const stdin = outDir("stdin");
    expect(await Session.launch(["/usr/bin/cat"], "correct horse\n", stdin).quit()).toBe(0);
    expect(readFileSync(`${stdin}/pippin.log`, "utf8")).toBe("correct horse\n");
  } finally {
    delete process.env.PIPPIN_DUMP_HEVC;
  }
});

test("reports a UDP port that another socket holds, and not a free one", async () => {
  const other = await Bun.udpSocket({ port: 0 });
  const port = other.port;
  expect(await held([port])).toEqual([port]);
  other.close();
  expect(await held([port])).toEqual([]);
});

function alive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

test("a SIGTERM that arrives while a session starts ends its child, removes the FIFO, and exits with 130", async () => {
  const out = outDir("signal");
  const run = `${tmp}/signal-run`;
  mkdirSync(run, { mode: 0o700 });
  const pidFile = `${out}/pid`;
  // The signal is on its way when the session starts. The child writes its
  // pid, when it lives long enough for that, and then sleeps.
  const script = `
    import { Hold } from ${JSON.stringify(`${import.meta.dir}/hold.ts`)};
    import { Session } from ${JSON.stringify(`${import.meta.dir}/pippin.ts`)};
    const hold = new Hold();
    process.kill(process.pid, "SIGTERM");
    const session = Session.launch(["/bin/sh", "-c", 'echo $$ >"$0"; exec sleep 60', ${JSON.stringify(pidFile)}], "", ${JSON.stringify(out)});
    hold.onTeardown(() => session.close());
    await Bun.sleep(10_000);
    process.exit(1);
  `;
  const runner = Bun.spawn([process.execPath, "-e", script], {
    env: { ...process.env, XDG_RUNTIME_DIR: run },
    stdout: "ignore",
    stderr: "pipe",
  });
  const [code, stderr] = await Promise.all([runner.exited, new Response(runner.stderr).text()]);
  expect(stderr).toBe("");
  expect(code).toBe(130);
  expect(readdirSync(run)).toEqual([]);
  // A child that outlived the runner writes its pid within this time.
  await Bun.sleep(500);
  // The shell creates the file before it writes the pid, so a child that
  // ends in between leaves it empty. That is no pid 0: kill(0) would signal
  // the whole process group, the test runner too.
  const text = existsSync(pidFile) ? readFileSync(pidFile, "utf8") : "";
  const pid = text === "" ? undefined : Number(text);
  try {
    for (let i = 0; pid !== undefined && alive(pid) && i < 20; i++) await Bun.sleep(100);
    expect(pid !== undefined && alive(pid)).toBe(false);
  } finally {
    if (pid !== undefined && alive(pid)) process.kill(pid);
  }
});
