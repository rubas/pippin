// One headless pippin session, driven through its control FIFO (README,
// Development). The password goes to pippin on stdin only.

import { closeSync, constants, existsSync, openSync, rmSync, writeSync } from "node:fs";
import { spawn } from "./proc";
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

/** pippin binds these fixed UDP ports (README, Single Instance / IPv4). */
export const PORTS = [5900, 5901];

/** The UDP ports of `ports` that a socket on this machine holds. */
export async function held(ports: number[]): Promise<number[]> {
  const out: number[] = [];
  for (const port of ports) {
    try {
      (await Bun.udpSocket({ port })).close();
    } catch (e) {
      if ((e as { code?: string }).code !== "EADDRINUSE") throw e;
      out.push(port);
    }
  }
  return out;
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
  static start(ip: string, login: Login, out: string, macDisplay = false): Session {
    const argv = [BINARY, "--headless", ip, "--user", login.username, "--password-stdin"];
    argv.push("--session", "share", "-v", "--stats-json", `${out}/stats.jsonl`);
    argv.push(...(macDisplay ? ["--no-virtual-display"] : ["--size", SIZE]));
    return Session.launch(argv, `${login.password}\n`, out);
  }

  /**
   * Runs `argv` with `stdin` and a new control FIFO, and writes its output
   * to pippin.log in `out`. The child gets no variable of the runner's shell:
   * PIPPIN_PASSWORD would win over stdin, and PIPPIN_DUMP_HEVC would overwrite
   * a capture. No step here waits, so no signal handler runs before the
   * caller has registered `close`.
   */
  static launch(argv: string[], stdin: string, out: string): Session {
    const fifo = `${process.env.XDG_RUNTIME_DIR}/pippin-e2e-${process.pid}.ctl`;
    rmSync(fifo, { force: true });
    const made = Bun.spawnSync(["mkfifo", "-m", "600", fifo]);
    if (made.exitCode !== 0) throw new Error(`mkfifo ${fifo}: ${made.stderr.toString().trim()}`);
    // Read-write, like pippin: the open does not wait for a reader, and a
    // write waits in the pipe until pippin reads it.
    const fd = openSync(fifo, constants.O_RDWR | constants.O_NONBLOCK);
    const log = openSync(`${out}/pippin.log`, "a");
    const child = Bun.spawn(argv, {
      stdin: Buffer.from(stdin),
      stdout: log,
      stderr: log,
      env: { PIPPIN_CONTROL_FIFO: fifo },
    });
    closeSync(log);
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
    if (this.closed) return;
    this.closed = true;
    if (this.child.exitCode === null) this.child.kill();
    closeSync(this.fd);
    rmSync(this.fifo, { force: true });
  }
}
