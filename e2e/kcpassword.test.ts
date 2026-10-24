import { expect, test } from "bun:test";
import { kcpassword } from "./kcpassword";

const hex = (b: Uint8Array) => Buffer.from(b).toString("hex");

test("encodes 'admin' like the kcpassword of the Tart image, up to the end of the password", () => {
  // The image's file: 1ced3f4abcbc3b160181bc0f. macOS fills the bytes after
  // the NUL with other values; the login reads only up to the NUL.
  expect(hex(kcpassword("admin")).slice(0, 12)).toBe("1ced3f4abcbc");
  expect(kcpassword("admin").length).toBe(12);
});

test("pads a password of 12 characters to 24 bytes, so that a NUL ends it", () => {
  expect(kcpassword("abcdefghijkl").length).toBe(24);
  expect(kcpassword("abcdefghijkl")[12]).toBe(0x00 ^ 0x89);
});
