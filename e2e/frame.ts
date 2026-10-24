// Reads a snapshot with ImageMagick.

import { capture } from "./proc";

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
