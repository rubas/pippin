// The macOS guest, over ssh on the LAN. It takes the machine key that also
// reaches the Mac mini (`~/.ssh/macmini`): `setup` installs the mini's
// `authorized_keys` in the guest, and turns ssh password logins off.

import { mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { capture, spawn, waitFor, type Options, type Output } from "./proc";

export const USER = "admin";

const STATE = `${homedir()}/.local/state/pippin-e2e`;
// Every clone of one base has the same host key. `setup` forgets it when it
// builds a new base.
export const KNOWN_HOSTS = `${STATE}/known_hosts`;
export const HOST_ALIAS = "pippin-vm";

export function sshOptions(): string[] {
  mkdirSync(STATE, { recursive: true });
  return [
    "-o", `HostKeyAlias=${HOST_ALIAS}`,
    "-o", `UserKnownHostsFile=${KNOWN_HOSTS}`,
    "-o", "StrictHostKeyChecking=accept-new",
    "-o", "ConnectTimeout=5",
    "-o", "ServerAliveInterval=5",
    "-o", "LogLevel=ERROR",
  ];
}

export class Guest {
  constructor(readonly ip: string) {}

  private argv(command: string): string[] {
    return [
      "ssh", ...sshOptions(),
      "-o", "BatchMode=yes",
      "-o", "IdentitiesOnly=yes",
      "-o", "IdentityAgent=none",
      "-i", `${homedir()}/.ssh/macmini`,
      `${USER}@${this.ip}`, command,
    ];
  }

  /** Runs a shell command in the guest as `admin`. */
  sh(command: string, opts: Options = {}): Promise<string> {
    return capture(this.argv(command), opts);
  }

  try(command: string, opts: Options = {}): Promise<Output> {
    return spawn(this.argv(command), opts);
  }

  /** Writes `data` to `path` in the guest, with the given mode. */
  async put(path: string, data: string | Uint8Array, mode = "644"): Promise<void> {
    await this.sh(`umask 077 && cat >'${path}' && chmod ${mode} '${path}'`, { stdin: data });
  }

  async waitSsh(timeoutMs: number): Promise<void> {
    await waitFor("ssh to the VM", timeoutMs, async () => ((await this.try("true")).code === 0 ? true : undefined));
  }

  /** Waits until `admin` is logged in at the console, with the Dock and the Finder up. */
  async waitDesktop(timeoutMs: number): Promise<void> {
    const ready = `[ "$(stat -f %Su /dev/console)" = ${USER} ] && pgrep -qx Dock && pgrep -qx Finder`;
    await waitFor("admin at the console", timeoutMs, async () => ((await this.try(ready)).code === 0 ? true : undefined));
  }

  /** The time of the guest, in the format of `log show --start`. */
  async now(): Promise<string> {
    return (await this.sh("date '+%Y-%m-%d %H:%M:%S'")).trim();
  }

  /** The system log since `start`, of the processes named. */
  async log(start: string, processes: string[]): Promise<string> {
    const predicate = processes.map((p) => `process == "${p}"`).join(" OR ");
    return this.sh(`/usr/bin/log show --style compact --info --start '${start}' --predicate '${predicate}'`);
  }

  /** The screens that AppKit lists: id, size, origin, and name. */
  async displays(): Promise<string> {
    const script = await Bun.file(`${import.meta.dir}/guest/displays.js`).text();
    await this.put("/tmp/pippin-e2e-displays.js", script);
    return this.sh("osascript -l JavaScript /tmp/pippin-e2e-displays.js");
  }
}
