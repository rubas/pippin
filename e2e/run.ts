// End-to-end tests of pippin against a throwaway macOS VM on the Mac mini.
// README, section End-to-end tests.
//
//   bun e2e/run.ts setup            build the base VM once (idempotent)
//   bun e2e/run.ts up               boot a fresh clone, and wait for the desktop
//   bun e2e/run.ts down             stop and delete the clone that this machine started
//   bun e2e/run.ts run <scenario>   build pippin, then up, the scenario, and down

import { mkdirSync, rmSync } from "node:fs";
import { homedir, hostname } from "node:os";
import { Guest } from "./guest";
import { readLogin } from "./login";
import * as mini from "./mini";
import { build, running } from "./pippin";
import { SCENARIOS } from "./scenarios";
import { buildBase } from "./setup";

// The lock owner of the VM that this machine started, so that `down` stops
// only that VM.
const SESSION = `${process.env.XDG_RUNTIME_DIR}/pippin-e2e.session`;
// pippin must never connect to the mini itself: it shares Dominic's screen.
const MINI_LAN = "192.168.5.239";

async function take(): Promise<void> {
  if (await Bun.file(SESSION).exists()) {
    throw new Error(`this machine holds the VM already (${SESSION}); run down first`);
  }
  const owner = `${hostname()} ${process.pid} ${Date.now()}`;
  await mini.lock(owner);
  await Bun.write(SESSION, owner);
}

async function up(): Promise<Guest> {
  if (!(await mini.exists(mini.BASE))) throw new Error(`the mini has no ${mini.BASE}; run setup first`);
  await take();
  await mini.clone(mini.BASE, mini.RUN, true);
  await mini.start(mini.RUN);
  const guest = new Guest(await mini.ip(mini.RUN));
  if (guest.ip === MINI_LAN) throw new Error("the VM address is the Mac mini's own");
  await guest.waitSsh(120_000);
  await guest.waitDesktop(120_000);
  return guest;
}

async function down(): Promise<void> {
  const file = Bun.file(SESSION);
  if (!(await file.exists())) return;
  const owner = (await file.text()).trim();
  if ((await mini.holder()) === owner) {
    await mini.stopAndDelete();
    await mini.unlock(owner);
  } else {
    console.error("another session holds the VM now; this one stops nothing");
  }
  rmSync(SESSION, { force: true });
}

async function setup(): Promise<void> {
  if ((await readLogin()) && (await mini.exists(mini.BASE))) {
    console.log(`${mini.BASE} is ready`);
    return;
  }
  await take();
  try {
    await buildBase();
  } finally {
    await down();
  }
  console.log(`${mini.BASE} is ready; its login is in Infisical r-dotfiles dev:/pippin-vm`);
}

function stamp(): string {
  const d = new Date();
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}-${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`;
}

async function run(name: string): Promise<boolean> {
  const scenario = SCENARIOS[name];
  if (!scenario) throw new Error(`no scenario ${name}; the scenarios are ${Object.keys(SCENARIOS).join(", ")}`);
  if (await running()) throw new Error("another pippin runs on this machine, and it holds the fixed UDP ports");
  const login = await readLogin();
  if (!login) throw new Error("Infisical has no login for the VM; run setup first");
  const out = `${homedir()}/.cache/tasks/pippin-e2e/${stamp()}-${name}`;
  mkdirSync(out, { recursive: true });
  console.log(`${name}: ${scenario.about}\noutput: ${out}`);
  await build(`${out}/build.log`);

  let failed = false;
  const check = (check: string, pass: boolean, detail: string) => {
    failed ||= !pass;
    console.log(`${pass ? "PASS" : "FAIL"} ${check}: ${detail}`);
  };
  const teardown: (() => unknown)[] = [];
  let finishing: Promise<void> | undefined;
  // Runs once: at the end, or on a signal.
  const finish = () =>
    (finishing ??= (async () => {
      for (const f of teardown.reverse()) {
        await Promise.resolve()
          .then(f)
          .catch((e) => console.error(`teardown: ${e.message}`));
      }
      await down();
    })());
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) {
    process.on(signal, () => void finish().then(() => process.exit(130)));
  }
  try {
    const guest = await up();
    await scenario.run({ guest, login, out, check, onTeardown: (f) => teardown.push(f) });
  } catch (e) {
    check("run", false, (e as Error).message);
  } finally {
    await finish();
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
      const guest = await up().catch(async (e) => {
        await down();
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
