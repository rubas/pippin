import { expect, test } from "bun:test";
import { dockerRun, onLink } from "./container";

test("runs pippin at 192.168.4.10 with the host's /usr, the binary, and only the given directories", () => {
  const argv = dockerRun(
    "pippin-e2e-42",
    "1000:1000",
    ["/w/target/release/pippin", "--headless", "192.168.6.20", "--password-stdin"],
    ["/run/user/1000/pippin-e2e-42", "/home/rubas/.cache/tasks/pippin-e2e/out"],
    { PIPPIN_CONTROL_FIFO: "/run/user/1000/pippin-e2e-42/pippin.ctl" },
  );
  expect(argv).toEqual([
    ...["docker", "run", "--rm", "--interactive", "--init", "--name", "pippin-e2e-42", "--user", "1000:1000"],
    ...["--network", "name=pippin-e2e,ip=192.168.4.10,mac-address=02:70:69:70:00:01"],
    ...["--device", "/dev/dri"],
    ...["--volume", "/usr:/usr:ro"],
    ...["--volume", "/w/target/release/pippin:/w/target/release/pippin:ro"],
    ...["--volume", "/run/user/1000/pippin-e2e-42:/run/user/1000/pippin-e2e-42:rw"],
    ...["--volume", "/home/rubas/.cache/tasks/pippin-e2e/out:/home/rubas/.cache/tasks/pippin-e2e/out:rw"],
    ...["--env", "PIPPIN_CONTROL_FIFO=/run/user/1000/pippin-e2e-42/pippin.ctl"],
    "archlinux@sha256:52dfbd5afb23a220d5ee92dafcb4c788d96c026ed04179890c23adef4c897fa0",
    ...["/w/target/release/pippin", "--headless", "192.168.6.20", "--password-stdin"],
  ]);
});

test("takes eno2 from the route to the mini on the LAN", () => {
  expect(onLink("192.168.5.239 dev eno2 src 192.168.7.109 uid 1000 \n    cache \n")).toBe("eno2");
});

test("refuses a route through a router, as away from home", () => {
  expect(onLink("192.168.5.239 via 10.0.0.1 dev wlan0 src 10.0.0.23 uid 1000 \n    cache \n")).toBeUndefined();
});
