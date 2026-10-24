// The e2e pippin runs in a Docker container with its own address on the LAN.
// pippin binds the fixed UDP ports 5900 and 5901 (README, Single Instance /
// IPv4), so a container with its own network namespace and address lets an
// e2e run share the machine with a pippin session of the host. The test VM
// sends its media to that address. A macvlan child cannot reach its host,
// and pippin does not need to: the runner drives it through files.

import { existsSync } from "node:fs";
import { capture, spawn } from "./proc";

const NETWORK = "pippin-e2e";
// The UniFi network Default. Its DHCP pool starts at .16, so .10 is static.
const SUBNET = "192.168.4.0/22";
const GATEWAY = "192.168.4.1";
const IP = "192.168.4.10";
const MAC = "02:70:69:70:00:01";
// archlinux:base gives the layout of the host, such as /lib64 -> usr/lib, and
// the host's /usr gives the libraries of the binary that the host built.
const IMAGE = "archlinux@sha256:52dfbd5afb23a220d5ee92dafcb4c788d96c026ed04179890c23adef4c897fa0";

/** The interface of the route in `ip route get` output, when the route needs no router. */
export function onLink(route: string): string | undefined {
  if (/\bvia\s/.test(route)) return undefined;
  return /\bdev\s+(\S+)/.exec(route)?.[1];
}

/**
 * The `docker run` call that runs `argv` as `user` ("uid:gid") at IP. The
 * host's /usr and the binary `argv[0]` mount read-only, and each of `dirs`
 * read-write, each at its own path.
 */
export function dockerRun(
  name: string,
  user: string,
  argv: [string, ...string[]],
  dirs: string[],
  env: Record<string, string>,
): string[] {
  const mount = (path: string, mode: "ro" | "rw") => ["--volume", `${path}:${path}:${mode}`];
  return [
    ...["docker", "run", "--rm", "--interactive", "--init", "--name", name, "--user", user],
    ...["--network", `name=${NETWORK},ip=${IP},mac-address=${MAC}`],
    // The VAAPI decoder opens a render node.
    ...["--device", "/dev/dri"],
    ...mount("/usr", "ro"),
    ...mount(argv[0], "ro"),
    ...dirs.flatMap((dir) => mount(dir, "rw")),
    ...Object.entries(env).flatMap(([k, v]) => ["--env", `${k}=${v}`]),
    IMAGE,
    ...argv,
  ];
}

/**
 * Creates the network on the wired interface of the route to `peer`, when it
 * is missing, and pulls the image.
 */
export async function prepare(peer: string): Promise<void> {
  const route = await capture(["ip", "route", "get", peer]);
  const dev = onLink(route);
  // A physical interface has a device; Wi-Fi refuses the second MAC.
  if (!dev || !existsSync(`/sys/class/net/${dev}/device`) || existsSync(`/sys/class/net/${dev}/wireless`)) {
    throw new Error(`e2e needs a wired LAN to ${peer}, and the route is: ${route.trim()}`);
  }
  const docker = (args: string[]) =>
    spawn(["docker", ...args]).catch(() => {
      throw new Error("e2e runs pippin in Docker, and docker is missing");
    });
  const parent = await docker(["network", "inspect", "--format", "{{.Options.parent}}", NETWORK]);
  if (parent.code === 0) {
    const was = new TextDecoder().decode(parent.stdout).trim();
    if (was !== dev) throw new Error(`the Docker network ${NETWORK} is on ${was}, not ${dev}; remove it`);
  } else {
    const net = ["--driver", "macvlan", "--subnet", SUBNET, "--gateway", GATEWAY, "--ip-range", `${IP}/32`];
    const out = await docker(["network", "create", ...net, "--opt", `parent=${dev}`, NETWORK]);
    if (out.code !== 0) throw new Error(`cannot create the Docker network ${NETWORK}: ${out.stderr.trim()}`);
  }
  if ((await docker(["image", "inspect", IMAGE])).code !== 0) {
    const out = await docker(["pull", "--quiet", IMAGE]);
    if (out.code !== 0) throw new Error(`cannot pull ${IMAGE}: ${out.stderr.trim()}`);
  }
}
