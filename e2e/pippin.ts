// One headless pippin session, driven through its control FIFO (README,
// Development). The password goes to pippin on stdin only. pippin runs in a
// container with its own LAN address (container.ts).

import { closeSync, constants, existsSync, mkdirSync, openSync, rmSync, writeSync } from "node:fs";
import { userInfo } from "node:os";
import { dirname } from "node:path";
import { dockerRun } from "./container";
import { capture, spawn } from "./proc";
import type { Login } from "./login";

// A clean path: the container has the binary, but no e2e directory to walk up from.
const ROOT = dirname(import.meta.dir);
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
    private readonly container: string,
    private readonly dir: string,
    private readonly fd: number,
    readonly out: string,
  ) {}

  /**
   * Starts pippin against `ip`. It writes its log and stats to `out`. With
   * `macDisplay`, pippin streams the Mac's own display (--no-virtual-display);
   * else the Mac creates a virtual display of SIZE for it.
   */
  static async start(ip: string, login: Login, out: string, macDisplay = false): Promise<Session> {
    const name = `pippin-e2e-${process.pid}`;
    // The container sees only this directory of XDG_RUNTIME_DIR, which is
    // private, as pippin wants the directory of its FIFO.
    const dir = `${process.env.XDG_RUNTIME_DIR}/${name}`;
    rmSync(dir, { recursive: true, force: true });
    mkdirSync(dir, { mode: 0o700 });
    const fifo = `${dir}/pippin.ctl`;
    await capture(["mkfifo", "-m", "600", fifo]);
    // Read-write, like pippin: the open does not wait for a reader, and a
    // write waits in the pipe until pippin reads it.
    const fd = openSync(fifo, constants.O_RDWR | constants.O_NONBLOCK);
    const log = openSync(`${out}/pippin.log`, "a");
    const argv: [string, ...string[]] = [BINARY, "--headless", ip, "--user", login.username, "--password-stdin"];
    argv.push("--session", "share", "-v", "--stats-json", `${out}/stats.jsonl`);
    argv.push(...(macDisplay ? ["--no-virtual-display"] : ["--size", SIZE]));
    const { uid, gid } = userInfo();
    const docker = dockerRun(name, `${uid}:${gid}`, argv, [dir, out], { PIPPIN_CONTROL_FIFO: fifo });
    const child = Bun.spawn(docker, { stdin: "pipe", stdout: log, stderr: log });
    closeSync(log);
    child.stdin.write(`${login.password}\n`);
    await child.stdin.end();
    return new Session(child, name, dir, fd, out);
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

  /** Ends pippin, if it still runs, and removes its container and the FIFO. */
  close(): void {
    if (this.closed) return;
    this.closed = true;
    // A kill of the docker client alone can leave the container running.
    Bun.spawnSync(["docker", "rm", "--force", this.container], { stdout: "ignore", stderr: "ignore" });
    closeSync(this.fd);
    rmSync(this.dir, { recursive: true, force: true });
  }
}
