// One headless pippin session, driven through its control FIFO (README,
// Development). The password goes to pippin on stdin only.

import { closeSync, constants, existsSync, openSync, rmSync, writeSync } from "node:fs";
import { capture, spawn } from "./proc";
import type { Login } from "./login";

const ROOT = `${import.meta.dir}/..`;
// An explicit target dir, so that a CARGO_TARGET_DIR cannot move the binary.
const TARGET = `${ROOT}/target`;
const BINARY = `${TARGET}/release/pippin`;
export const SIZE = "1920x1080";

/** Builds pippin at the lowest priority with 8 jobs: asrock is Dominic's desktop. */
export async function build(log: string): Promise<void> {
  const out = await spawn(["nice", "-n", "19", "cargo", "build", "--release", "--locked", "--target-dir", TARGET], {
    cwd: ROOT,
    env: { CARGO_BUILD_JOBS: "8" },
  });
  await Bun.write(log, out.stderr);
  if (out.code !== 0) throw new Error(`cargo build failed; see ${log}`);
}

/** pippin binds fixed UDP ports, so only one session runs on a machine. */
export async function running(): Promise<boolean> {
  return (await spawn(["pgrep", "-x", "pippin"])).code === 0;
}

/** The pictures that pippin decoded from `fromS` to `toS`, in Unix seconds, from --stats-json. */
export async function decoded(out: string, fromS: number, toS: number): Promise<number> {
  const lines = (await Bun.file(`${out}/stats.jsonl`).text()).trim().split("\n");
  return lines
    .map((l) => JSON.parse(l) as { t: number; decode_aus: number })
    .filter((l) => l.t > fromS && l.t <= toS)
    .reduce((sum, l) => sum + l.decode_aus, 0);
}

export class Session {
  private closed = false;

  private constructor(
    private readonly child: Bun.Subprocess,
    private readonly fifo: string,
    private readonly fd: number,
    readonly out: string,
  ) {}

  /**
   * Starts pippin against `ip`. It writes its log and stats to `out`. With
   * `macDisplay`, pippin streams the Mac's own display (--no-virtual-display);
   * else the Mac creates a virtual display of SIZE for it.
   */
  static async start(ip: string, login: Login, out: string, macDisplay = false): Promise<Session> {
    const fifo = `${process.env.XDG_RUNTIME_DIR}/pippin-e2e-${process.pid}.ctl`;
    rmSync(fifo, { force: true });
    await capture(["mkfifo", "-m", "600", fifo]);
    // Read-write, like pippin: the open does not wait for a reader, and a
    // write waits in the pipe until pippin reads it.
    const fd = openSync(fifo, constants.O_RDWR | constants.O_NONBLOCK);
    const log = openSync(`${out}/pippin.log`, "a");
    const argv = [BINARY, "--headless", ip, "--user", login.username, "--password-stdin"];
    argv.push("--session", "share", "-v", "--stats-json", `${out}/stats.jsonl`);
    argv.push(...(macDisplay ? ["--no-virtual-display"] : ["--size", SIZE]));
    // PIPPIN_PASSWORD would win over stdin.
    const { PIPPIN_PASSWORD: _, ...env } = process.env;
    const child = Bun.spawn(argv, {
      stdin: "pipe",
      stdout: log,
      stderr: log,
      env: { ...env, PIPPIN_CONTROL_FIFO: fifo },
    });
    closeSync(log);
    child.stdin.write(`${login.password}\n`);
    await child.stdin.end();
    return new Session(child, fifo, fd, out);
  }

  send(command: string): void {
    writeSync(this.fd, `${command}\n`);
  }

  /**
   * Asks for a snapshot, and waits for its file. A snapshot before the first
   * picture waits for it, so the first one also proves that video arrived.
   * Returns the path, or undefined when no file came in time.
   */
  async snapshot(name: string, timeoutMs = 30_000): Promise<string | undefined> {
    const path = `${this.out}/${name}.png`;
    this.send(`snapshot ${path}`);
    const end = Date.now() + timeoutMs;
    while (Date.now() < end && this.child.exitCode === null) {
      if (existsSync(path)) return path;
      await Bun.sleep(250);
    }
    return existsSync(path) ? path : undefined;
  }

  /** Sends `quit`, and returns the exit code of pippin. */
  async quit(): Promise<number> {
    if (this.child.exitCode === null) this.send("quit");
    const code = await Promise.race([this.child.exited, Bun.sleep(15_000).then(() => undefined)]);
    this.close();
    return code ?? -1;
  }

  /** Ends pippin, if it still runs, and removes the FIFO. */
  close(): void {
    if (this.child.exitCode === null) this.child.kill();
    if (this.closed) return;
    this.closed = true;
    closeSync(this.fd);
    rmSync(this.fifo, { force: true });
  }
}
