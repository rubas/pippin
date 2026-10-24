// Reads a snapshot with ImageMagick.

import { capture, spawn } from "./proc";

export async function size(png: string): Promise<string> {
  return (await capture(["magick", "identify", "-format", "%wx%h", png])).trim();
}

/**
 * The share of pixels, from 0 to 1, that are brighter than black, below the
 * top 5 % of the frame. The fullscreen bug leaves the menu bar on black, so
 * the menu bar does not count.
 */
export async function litShare(png: string): Promise<number> {
  const argv = ["magick", png, "-gravity", "south", "-crop", "100%x95%+0+0", "+repage"];
  argv.push("-colorspace", "Gray", "-threshold", "6%", "-format", "%[fx:mean]", "info:");
  return Number(await capture(argv));
}

/** The number of pixels that differ between two snapshots of one size. */
export async function differingPixels(a: string, b: string): Promise<number> {
  // compare exits with 1 when the pictures differ, and with 2 on an error.
  const out = await spawn(["magick", "compare", "-metric", "AE", a, b, "null:"]);
  if (out.code > 1) throw new Error(`magick compare exited with ${out.code}: ${out.stderr.trim()}`);
  return Number(out.stderr.trim().split(/\s/)[0]);
}
