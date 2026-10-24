// The end-to-end scenarios. Each one drives a headless pippin session against
// a fresh clone of the test VM, and reports its checks through `ctx.check`.

import { differingPixels, litShare, size } from "./frame";
import type { Guest } from "./guest";
import type { Login } from "./login";
import { readFile, swiftTool } from "./mini";
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
 * system log of `processes` in the guest goes to vm.log. With `macDisplay`,
 * pippin streams the Mac's own display.
 */
async function connected(
  ctx: Context,
  processes: string[],
  macDisplay: boolean,
  body: (s: Session) => Promise<void>,
): Promise<void> {
  const start = await ctx.guest.now();
  const session = await Session.start(ctx.guest.ip, ctx.login, ctx.out, macDisplay);
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
    await connected(ctx, ["WindowServer", "screensharingd"], false, async (session) => {
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

// vdisplay is the only display of the mini, but the VM keeps its own display
// next to it. displaycfg switches the VM's display off, and vdisplay becomes
// main at 0,0, as on the mini.
async function onlyVdisplay(ctx: Context): Promise<void> {
  await ctx.guest.put("displaycfg", await swiftTool("displaycfg"), "755");
  await ctx.guest.sh("./displaycfg");
  const only = (cg: string[]) => cg.length === 1 && /^cg \S+ \d+x\d+\+0\+0 vendor 0x1ab2 active main$/.test(cg[0]!);
  let cg: string[] = [];
  await waitFor("vdisplay as the only display", 10_000, async (left) => {
    cg = (await ctx.guest.displays(left)).split("\n").filter((l) => l.startsWith("cg "));
    return only(cg) || undefined;
  }).catch(() => undefined);
  ctx.check("only vdisplay", only(cg), cg.join("; "));
}

/**
 * The main display of CoreGraphics, with its size in points and in pixels
 * ("2560x1440" and "5120x2880") when AppKit gives it to apps as a screen.
 */
type Main = { id: string; online: boolean; points?: string; pixels?: string };

function mainDisplay(displays: string): Main | undefined {
  const main = displays.match(/^main (\S+) (online|offline)$/m);
  if (!main) return undefined;
  const screen = displays.match(new RegExp(`^screen ${main[1]} ((\\d+)x(\\d+))\\+\\S+ (\\d+)x `, "m"));
  const pixels = screen && `${Number(screen[2]) * Number(screen[4])}x${Number(screen[3]) * Number(screen[4])}`;
  return { id: main[1]!, online: main[2] === "online", points: screen?.[1], pixels: pixels ?? undefined };
}

function describe(main: Main | undefined): string {
  return main ? `${main.id} ${main.online ? "online" : "offline"}` : "none";
}

// While pippin is connected, the display that it streams must be the main
// display, and online. pippin names its own display in its display request
// (src/proto/messages.rs). With --no-virtual-display, there is no pippin
// display, and the main display stays the one from before the connect.
function checkMain(ctx: Context, macDisplay: boolean, before: string, connected: string): void {
  const main = mainDisplay(connected);
  const pippin = connected.match(/^screen (\S+) .* pippin Virtual Display$/m)?.[1];
  if (!macDisplay) {
    const pass = main?.online === true && main.id === pippin;
    return ctx.check("main display", pass, `main ${describe(main)}, pippin display ${pippin ?? "none"}`);
  }
  const was = mainDisplay(before);
  const pass = main?.online === true && main.id === was?.id && pippin === undefined;
  ctx.check("main display", pass, `main ${describe(main)}, before the connect ${describe(was)}, pippin display ${pippin ?? "none"}`);
}

const APP = "TextEdit";
// The fullscreen transition takes about 1 s, and the snapshot comes 10 s
// after the key. pippin must still decode pictures in the last seconds
// before it.
const SETTLED_S = 3;

/**
 * Puts a window into native fullscreen. With `vdisplay`, the mini's virtual
 * display is the only display of the VM. With `macDisplay`, pippin streams
 * that display (--no-virtual-display) instead of its own: the window must
 * then cover the main display, and the frame must show all of it.
 */
function fullscreen(vdisplay: boolean, macDisplay = false): Scenario {
  const streams = macDisplay ? ", and pippin streams the Mac's own display" : "";
  return {
    about: `put a ${APP} window into native fullscreen, ${vdisplay ? "with" : "without"} the mini's vdisplay${streams}`,
    async run(ctx) {
      let window: string | undefined, full: string | undefined;
      let fullAt = 0;
      if (vdisplay) {
        await startVdisplay(ctx);
        await onlyVdisplay(ctx);
      }
      const before = await ctx.guest.displays();
      await Bun.write(`${ctx.out}/displays-before.txt`, before);
      await connected(ctx, ["WindowServer", "screensharingd", APP], macDisplay, async (session) => {
        const displays = await ctx.guest.displays();
        await Bun.write(`${ctx.out}/displays-connected.txt`, displays);
        checkMain(ctx, macDisplay, before, displays);
        // The window and the frame must cover the display that pippin streams.
        const main = mainDisplay(displays);
        const points = macDisplay ? main?.points : SIZE;
        const pixels = macDisplay ? main?.pixels : SIZE;
        await ctx.guest.put("/tmp/pippin-e2e.txt", "pippin e2e: this window goes into fullscreen.\n");
        await ctx.guest.sh(`open -a ${APP} /tmp/pippin-e2e.txt`);
        await Bun.sleep(3000);
        ctx.check("app runs", await ctx.guest.ok(`pgrep -qx ${APP}`), APP);
        window = await session.snapshot("1-window");
        await Bun.write(`${ctx.out}/windows-window.txt`, await ctx.guest.jxa("windows.js", [APP]));
        session.send("key ctrl+cmd+f");
        await Bun.sleep(10_000);
        full = await session.snapshot("2-fullscreen");
        fullAt = Date.now() / 1000;
        await Bun.write(`${ctx.out}/displays-fullscreen.txt`, await ctx.guest.displays());
        const windows = (await ctx.guest.jxa("windows.js", [APP])).trim().split("\n");
        const shown = `windows of ${APP}: ${windows.join(", ")}; display ${points ?? "unknown"}`;
        ctx.check("app is in fullscreen", points !== undefined && windows.includes(`${points}+0+0`), shown);
        const got = full ? await size(full) : "none";
        ctx.check("frame size", got === pixels, `${got}, display ${pixels ?? "unknown"} pixels`);
        await checkLit(ctx, "fullscreen frame", full);
      });
      // A frozen stream would show an old picture: the fullscreen snapshot
      // must differ from the window, and pippin must still decode pictures
      // after the transition. The stats are complete once pippin quit.
      if (!window || !full) {
        ctx.check("fullscreen picture is new", false, "no snapshot of the window or of the fullscreen");
      } else {
        const pixels = await differingPixels(window, full);
        const pictures = await decoded(ctx.out, fullAt - SETTLED_S, fullAt);
        ctx.check(
          "fullscreen picture is new",
          pixels > 0 && pictures > 0,
          `${pixels} pixels differ, ${pictures} pictures decoded in the ${SETTLED_S} s before the fullscreen snapshot`,
        );
      }
    },
  };
}

const SCROLL_APP = "/tmp/scrolllog.app";
const SCROLL_LOG = "/tmp/scrolllog.log";
const SCROLL_PLIST = `<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>scrolllog</string>
<key>CFBundleIdentifier</key><string>dev.rubas.scrolllog</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
`;

/** A scroll event that reached scrolllog, with the sign of the device: a positive x is left, a positive y up. */
type Scrolled = { x: number; y: number };

async function scrolled(ctx: Context): Promise<Scrolled[]> {
  const lines = (await ctx.guest.sh(`cat ${SCROLL_LOG}`)).split("\n");
  return lines
    .filter((l) => l.startsWith("scroll "))
    .map((l) => {
      const [, x, y] = l.split(" ");
      return { x: Number(x), y: Number(y) };
    });
}

const scroll: Scenario = {
  about: "scroll down and right with the control FIFO, and see both reach a window on the Mac",
  async run(ctx) {
    await connected(ctx, ["WindowServer", "screensharingd"], false, async (session) => {
      // After the connect, so that the window covers pippin's display.
      await ctx.guest.sh(`mkdir -p ${SCROLL_APP}/Contents/MacOS && : >${SCROLL_LOG}`);
      await ctx.guest.put(`${SCROLL_APP}/Contents/Info.plist`, SCROLL_PLIST);
      await ctx.guest.put(`${SCROLL_APP}/Contents/MacOS/scrolllog`, await swiftTool("scrolllog"), "755");
      await ctx.guest.sh(`open -n ${SCROLL_APP} --args ${SCROLL_LOG}`);
      ctx.onTeardown(async () => Bun.write(`${ctx.out}/scrolllog.log`, await ctx.guest.sh(`cat ${SCROLL_LOG}`)));
      await waitFor("scrolllog to show its window", 20_000, async () =>
        (await ctx.guest.sh(`cat ${SCROLL_LOG}`)).includes("ready") || undefined,
      );
      session.send("pointer 200 200");
      await Bun.sleep(1000);
      session.send("scroll 0 3");
      await Bun.sleep(2000);
      const vertical = await scrolled(ctx);
      session.send("scroll 2 0");
      await Bun.sleep(2000);
      const horizontal = (await scrolled(ctx)).slice(vertical.length);
      await session.snapshot("1-scrolllog");
      const show = (events: Scrolled[]) => events.map((e) => `${e.x},${e.y}`).join(" ") || "none";
      // AppKit can merge scroll events, so the count of events is not the count of steps.
      const all = (events: Scrolled[], ok: (e: Scrolled) => boolean) => events.length > 0 && events.every(ok);
      ctx.check("scroll down", all(vertical, (e) => e.x === 0 && e.y < 0), `events ${show(vertical)}`);
      ctx.check("scroll right", all(horizontal, (e) => e.x < 0 && e.y === 0), `events ${show(horizontal)}`);
    });
  },
};

export const SCENARIOS: Record<string, Scenario> = {
  smoke,
  scroll,
  fullscreen: fullscreen(true),
  "fullscreen-no-vdisplay": fullscreen(false),
  "fullscreen-own-display": fullscreen(true, true),
};
