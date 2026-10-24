// The end-to-end scenarios. Each one drives a headless pippin session against
// a fresh clone of the test VM, and reports its checks through `ctx.check`.

import { differingPixels, litShare, size } from "./frame";
import type { Guest } from "./guest";
import type { Login } from "./login";
import { readFile } from "./mini";
import { decoded, Session, SIZE } from "./pippin";
import { waitFor } from "./proc";

export type Context = {
  guest: Guest;
  login: Login;
  out: string;
  check(name: string, pass: boolean, detail: string): void;
  /** Runs `f` at the end of the run, also after a failure or a signal. */
  onTeardown(f: () => unknown): void;
};

type Scenario = { about: string; run(ctx: Context): Promise<void> };

/** A frame passes when more than half of it is brighter than black. */
const LIT = 0.5;

async function checkLit(ctx: Context, name: string, png: string | undefined): Promise<void> {
  if (!png) return ctx.check(name, false, "no snapshot");
  const share = await litShare(png);
  ctx.check(name, share > LIT, `${Math.round(share * 100)} % not black, ${png}`);
}

/**
 * Connects pippin, waits for the first picture, runs `body`, and quits. The
 * system log of `processes` in the guest goes to vm.log.
 */
async function connected(ctx: Context, processes: string[], body: (s: Session) => Promise<void>): Promise<void> {
  const start = await ctx.guest.now();
  const session = await Session.start(ctx.guest.ip, ctx.login, ctx.out);
  ctx.onTeardown(() => session.close());
  try {
    const first = await session.snapshot("0-connected", 90_000);
    ctx.check("video", first !== undefined, first ? `first picture, ${first}` : "no picture within 90 s");
    if (first) await body(session);
  } finally {
    const code = await session.quit();
    ctx.check("pippin quits", code === 0, `exit ${code}`);
    await Bun.write(`${ctx.out}/vm.log`, await ctx.guest.log(start, processes));
  }
}

const smoke: Scenario = {
  about: "connect, take a snapshot of the desktop, and quit",
  async run(ctx) {
    await connected(ctx, ["WindowServer", "screensharingd"], async (session) => {
      await Bun.sleep(5000);
      const png = await session.snapshot("1-desktop");
      const got = png ? await size(png) : "none";
      ctx.check("frame size", got === SIZE, got);
      await checkLit(ctx, "desktop frame", png);
    });
  },
};

// The Mac mini runs its own virtual display, so that it has a screen without
// a monitor (launchd agent dev.rubas.vdisplay, same arguments).
const VDISPLAY = ".local/bin/vdisplay";
const VDISPLAY_ARGS = "-w 2560 -h 1440";

async function startVdisplay(ctx: Context): Promise<void> {
  await ctx.guest.put("vdisplay", await readFile(VDISPLAY), "755");
  await ctx.guest.sh(
    `launchctl submit -l dev.rubas.vdisplay -o /tmp/vdisplay.log -e /tmp/vdisplay.log -- "$HOME/vdisplay" ${VDISPLAY_ARGS}`,
  );
  ctx.onTeardown(async () => Bun.write(`${ctx.out}/vdisplay.log`, await ctx.guest.sh("cat /tmp/vdisplay.log")));
  const screens = await waitFor("a second screen from vdisplay", 20_000, async (left) => {
    const lines = (await ctx.guest.displays(left)).split("\n").filter((l) => l.startsWith("screen "));
    return lines.length >= 2 ? lines : undefined;
  });
  ctx.check("vdisplay", true, screens.join("; "));
}

const APP = "TextEdit";

function fullscreen(vdisplay: boolean): Scenario {
  return {
    about: `put a ${APP} window into native fullscreen, ${vdisplay ? "with" : "without"} the mini's vdisplay`,
    async run(ctx) {
      let window: string | undefined, full: string | undefined;
      let windowAt = 0, fullAt = 0;
      if (vdisplay) await startVdisplay(ctx);
      await Bun.write(`${ctx.out}/displays-before.txt`, await ctx.guest.displays());
      await connected(ctx, ["WindowServer", "screensharingd", APP], async (session) => {
        await Bun.write(`${ctx.out}/displays-connected.txt`, await ctx.guest.displays());
        await ctx.guest.put("/tmp/pippin-e2e.txt", "pippin e2e: this window goes into fullscreen.\n");
        await ctx.guest.sh(`open -a ${APP} /tmp/pippin-e2e.txt`);
        await Bun.sleep(3000);
        ctx.check("app runs", await ctx.guest.ok(`pgrep -qx ${APP}`), APP);
        window = await session.snapshot("1-window");
        windowAt = Date.now() / 1000;
        await Bun.write(`${ctx.out}/windows-window.txt`, await ctx.guest.jxa("windows.js", [APP]));
        session.send("key ctrl+cmd+f");
        await Bun.sleep(10_000);
        full = await session.snapshot("2-fullscreen");
        fullAt = Date.now() / 1000;
        await Bun.write(`${ctx.out}/displays-fullscreen.txt`, await ctx.guest.displays());
        const windows = (await ctx.guest.jxa("windows.js", [APP])).trim().split("\n");
        ctx.check("app is in fullscreen", windows.includes(`${SIZE}+0+0`), `windows of ${APP}: ${windows.join(", ")}`);
        await checkLit(ctx, "fullscreen frame", full);
      });
      // A frozen stream would show an old picture: the fullscreen snapshot
      // must differ from the window, and pippin must have decoded new
      // pictures in between. The stats are complete once pippin quit.
      if (window && full) {
        const pixels = await differingPixels(window, full);
        const pictures = await decoded(ctx.out, windowAt, fullAt);
        ctx.check("fullscreen picture is new", pixels > 0 && pictures > 0, `${pixels} pixels differ, ${pictures} pictures decoded`);
      }
      // WindowServer also logs "Invalid display" for the new display of each
      // Screen Sharing session, before the display exists. Only the app's
      // lines show the bug.
      const log = await Bun.file(`${ctx.out}/vm.log`).text();
      const invalid = log.match(new RegExp(` ${APP}\\[.*Invalid display`, "g"))?.length ?? 0;
      ctx.check("app finds its display", invalid === 0, `${invalid} lines of ${APP} with "Invalid display" in vm.log`);
    },
  };
}

export const SCENARIOS: Record<string, Scenario> = {
  smoke,
  fullscreen: fullscreen(true),
  "fullscreen-no-vdisplay": fullscreen(false),
};
