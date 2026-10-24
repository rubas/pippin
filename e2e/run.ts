// End-to-end tests of pippin against a throwaway macOS VM on the Mac mini.
// README, section End-to-end tests.
//
//   bun e2e/run.ts setup            build the base VM once (idempotent)
//   bun e2e/run.ts up               boot a fresh clone, and wait for the desktop
//   bun e2e/run.ts down             stop and delete the clone that this machine started
//   bun e2e/run.ts run <scenario>   build pippin, then up, the scenario, and down

import { mkdirSync, rmSync } from "node:fs";
import { homedir, hostname } from "node:os";
import { prepare } from "./container";
import { Guest, STATE } from "./guest";
import { readLogin, type Login } from "./login";
import * as mini from "./mini";
import { build } from "./pippin";
import { SCENARIOS } from "./scenarios";
import { buildBase } from "./setup";

// The lock owner of the VM that this machine started, so that `down` stops
// only that VM. It outlives a reboot, like the VM and the lock on the mini.
const SESSION = `${STATE}/session`;
// pippin must never connect to the mini itself: it shares Dominic's screen.
const MINI_LAN = "192.168.5.239";

/** Stops and deletes the VM of `owner`, and frees its lock, only while `owner` holds it. */
async function release(owner: string): Promise<void> {
  await mini.cancel(owner);
  if ((await mini.holder()) === owner) {
    await mini.stopAndDelete();
    await mini.unlock(owner);
  }
  const file = Bun.file(SESSION);
  if ((await file.exists()) && (await file.text()).trim() === owner) rmSync(SESSION);
}

/**
 * The hold of one invocation on the VM. It releases only what it took, once:
 * at the end, after a failure, or on a signal.
 */
class Hold {
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

  /** Runs `f` before the release, also after a failure or a signal. */
  onTeardown(f: () => unknown): void {
    this.teardown.push(f);
  }

  release(): Promise<void> {
    return (this.releasing ??= (async () => {
      for (const f of this.teardown.reverse()) {
        await Promise.resolve()
          .then(f)
          .catch((e) => console.error(`teardown: ${e.message}`));
      }
      if (this.owner) await release(this.owner);
    })());
  }
}

/** Takes the VM, and boots a fresh clone of the base. */
async function up(hold: Hold): Promise<{ guest: Guest; login: Login }> {
  await hold.take();
  const login = await readLogin();
  if (!login || !(await mini.exists(mini.BASE))) throw new Error(`no ${mini.BASE} with a login; run setup first`);
  await mini.clone(mini.BASE, mini.RUN, true);
  await mini.start(mini.RUN);
  const guest = new Guest(await mini.ip(mini.RUN));
  if (guest.ip === MINI_LAN) throw new Error("the VM address is the Mac mini's own");
  await guest.waitSsh(120_000);
  await guest.waitDesktop(120_000);
  return { guest, login };
}

async function setup(): Promise<void> {
  const hold = new Hold();
  try {
    await hold.take();
    if ((await readLogin()) && (await mini.exists(mini.BASE))) {
      console.log(`${mini.BASE} is ready`);
      return;
    }
    await buildBase();
    console.log(`${mini.BASE} is ready; its login is in Infisical r-dotfiles dev:/pippin-vm`);
  } finally {
    await hold.release();
  }
}

async function down(): Promise<void> {
  const file = Bun.file(SESSION);
  if (!(await file.exists())) {
    console.log("this machine holds no VM");
    return;
  }
  await release((await file.text()).trim());
}

function stamp(): string {
  const d = new Date();
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}-${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`;
}

async function run(name: string): Promise<boolean> {
  const scenario = SCENARIOS[name];
  if (!scenario) throw new Error(`no scenario ${name}; the scenarios are ${Object.keys(SCENARIOS).join(", ")}`);
  await prepare(MINI_LAN);
  const out = `${homedir()}/.cache/tasks/pippin-e2e/${stamp()}-${name}`;
  mkdirSync(out, { recursive: true });
  console.log(`${name}: ${scenario.about}\noutput: ${out}`);
  await build(`${out}/build.log`);

  let failed = false;
  const check = (check: string, pass: boolean, detail: string) => {
    failed ||= !pass;
    console.log(`${pass ? "PASS" : "FAIL"} ${check}: ${detail}`);
  };
  const hold = new Hold();
  try {
    const { guest, login } = await up(hold);
    await scenario.run({ guest, login, out, check, onTeardown: (f) => hold.onTeardown(f) });
  } catch (e) {
    check("run", false, (e as Error).message);
  } finally {
    await hold.release();
  }
  console.log(`output: ${out}`);
  return !failed;
}

async function main(args: string[]): Promise<boolean> {
  switch (args[0]) {
    case "setup":
      await setup();
      return true;
    case "up": {
      const hold = new Hold();
      const { guest } = await up(hold).catch(async (e) => {
        await hold.release();
        throw e;
      });
      console.log(`the VM runs at ${guest.ip}; run down when you are done`);
      return true;
    }
    case "down":
      await down();
      return true;
    case "run":
      if (args[1]) return run(args[1]);
  }
  console.error(`usage: bun e2e/run.ts setup | up | down | run <${Object.keys(SCENARIOS).join("|")}>`);
  return false;
}

try {
  process.exit((await main(Bun.argv.slice(2))) ? 0 : 1);
} catch (e) {
  console.error((e as Error).message);
  process.exit(1);
}
