// The macOS guest, over ssh on the LAN. It takes the machine key that also
// reaches the Mac mini (`~/.ssh/macmini`): `setup` installs the mini's
// `authorized_keys` in the guest, and turns ssh password logins off.

import { mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { capture, spawn, waitFor, type Options } from "./proc";

export const USER = "admin";

/** Files of this tool that outlive a reboot of this machine. */
export const STATE = `${homedir()}/.local/state/pippin-e2e`;
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

  /** Runs a shell command in the guest, and tells whether it exited with 0. */
  async ok(command: string, timeoutMs?: number): Promise<boolean> {
    return (await spawn(this.argv(command), { timeoutMs })).code === 0;
  }

  /** Writes `data` to `path` in the guest, with the given mode. */
  async put(path: string, data: string | Uint8Array, mode = "644"): Promise<void> {
    await this.sh(`umask 077 && cat >'${path}' && chmod ${mode} '${path}'`, { stdin: data });
  }

  async waitSsh(timeoutMs: number): Promise<void> {
    await waitFor("ssh to the VM", timeoutMs, async (left) => (await this.ok("true", left)) || undefined);
  }

  /** Waits until `admin` is logged in at the console, with the Dock and the Finder up. */
  async waitDesktop(timeoutMs: number): Promise<void> {
    const ready = `[ "$(stat -f %Su /dev/console)" = ${USER} ] && pgrep -qx Dock && pgrep -qx Finder`;
    await waitFor("admin at the console", timeoutMs, async (left) => (await this.ok(ready, left)) || undefined);
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

  /** Runs a JavaScript for Automation script of e2e/guest/ with `args`. */
  async jxa(name: string, args: string[] = [], timeoutMs?: number): Promise<string> {
    const script = await Bun.file(`${import.meta.dir}/guest/${name}`).text();
    return this.sh(["osascript -l JavaScript -", ...args].join(" "), { stdin: script, timeoutMs });
  }

  /** The displays of CoreGraphics and the screens of AppKit (guest/displays.js). */
  displays(timeoutMs?: number): Promise<string> {
    return this.jxa("displays.js", [], timeoutMs);
  }
}
