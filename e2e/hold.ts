// The hold of one invocation on the test VM: the lock on the mini, the
// session file of this machine, and what must end before both go.

import { mkdirSync, rmSync } from "node:fs";
import { hostname } from "node:os";
import { STATE } from "./guest";
import * as mini from "./mini";

// The lock owner of the VM that this machine started, so that `down` stops
// only that VM. It outlives a reboot, like the VM and the lock on the mini.
export const SESSION = `${STATE}/session`;

/** Stops and deletes the VM of `owner`, and frees its lock, only while `owner` holds it. */
export async function release(owner: string): Promise<void> {
  await mini.cancel(owner);
  if ((await mini.holder()) === owner) {
    await mini.stopAndDelete();
    await mini.unlock(owner);
  }
  const file = Bun.file(SESSION);
  if ((await file.exists()) && (await file.text()).trim() === owner) rmSync(SESSION);
}

async function run(f: () => unknown): Promise<void> {
  await Promise.resolve()
    .then(f)
    .catch((e) => console.error(`teardown: ${e.message}`));
}

/**
 * It releases only what it took, once: at the end, after a failure, or on a
 * signal.
 */
export class Hold {
  private owner: string | undefined;
  private readonly teardown: (() => unknown)[] = [];
  private releasing: Promise<void> | undefined;

  constructor() {
    for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) {
      process.on(signal, () => void this.release().then(() => process.exit(130)));
    }
  }

  async take(): Promise<void> {
    if (await Bun.file(SESSION).exists()) {
      throw new Error(`this machine holds the VM already (${SESSION}); run down first`);
    }
    // The owner is set before the lock call, so that a signal during it
    // still frees the lock once the call ends.
    this.owner = `${hostname()} ${process.pid} ${Date.now()}`;
    mkdirSync(STATE, { recursive: true });
    await Bun.write(SESSION, this.owner);
    await mini.lock(this.owner);
  }

  /**
   * Runs `f` before the release, also after a failure or a signal. The
   * scenario goes on while a release frees the VM, so what it starts then
   * ends at once.
   */
  onTeardown(f: () => unknown): void {
    if (this.releasing) void run(f);
    else this.teardown.push(f);
  }

  release(): Promise<void> {
    return (this.releasing ??= (async () => {
      for (const f of this.teardown.reverse()) await run(f);
      if (this.owner) await release(this.owner);
    })());
  }
}
