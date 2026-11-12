import { expect, test } from "bun:test";
import { Hold } from "./hold";

test("a teardown that registers while the release frees the VM runs at once, as for a session that starts after Ctrl-C", async () => {
  const hold = new Hold();
  const ran: string[] = [];
  hold.onTeardown(() => ran.push("before the release"));
  await hold.release();
  hold.onTeardown(() => ran.push("after the release began"));
  await Bun.sleep(0);
  expect(ran).toEqual(["before the release", "after the release began"]);
});
