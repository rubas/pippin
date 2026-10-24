// Builds the base VM once: a clone of the Tart image with a new random
// password for `admin`, because the image's password is public and the VM
// sits on our LAN while it runs. The password goes to Infisical, and from
// there only to pippin, on stdin.

import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { Guest, HOST_ALIAS, KNOWN_HOSTS, sshOptions, USER } from "./guest";
import { kcpassword } from "./kcpassword";
import { writeLogin } from "./login";
import * as mini from "./mini";
import { capture, spawn, waitFor } from "./proc";

/** The password of `admin` in the Tart image. It is public. */
const IMAGE_PASSWORD = "admin";

function randomPassword(length = 24): string {
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  let out = "";
  while (out.length < length) {
    for (const b of crypto.getRandomValues(new Uint8Array(length))) {
      // 248 = 4 * 62: a larger byte would favour the first letters.
      if (b < 248 && out.length < length) out += alphabet[b % 62];
    }
  }
  return out;
}

/**
 * Logs in once with the image's password: installs the mini's machine keys,
 * and turns ssh password logins off.
 */
async function installKeys(ip: string): Promise<void> {
  const dir = mkdtempSync(`${tmpdir()}/pippin-e2e-`);
  try {
    const askpass = `${dir}/askpass`;
    writeFileSync(askpass, `#!/bin/sh\necho ${IMAGE_PASSWORD}\n`, { mode: 0o700 });
    const script = [
      "umask 077 && mkdir -p .ssh && cat >.ssh/authorized_keys",
      'printf "PasswordAuthentication no\\nKbdInteractiveAuthentication no\\n" | sudo -n tee /etc/ssh/sshd_config.d/10-keys-only.conf >/dev/null',
      'sudo -n sshd -T | grep -qx "passwordauthentication no"',
      'sudo -n sshd -T | grep -qx "kbdinteractiveauthentication no"',
    ].join(" && ");
    const argv = ["ssh", ...sshOptions(), "-o", "PubkeyAuthentication=no", "-o", "ControlPath=none"];
    argv.push(`${USER}@${ip}`, script);
    const keys = await mini.authorizedKeys();
    const env = { SSH_ASKPASS: askpass, SSH_ASKPASS_REQUIRE: "force" };
    await waitFor("the first ssh login to the VM", 120_000, async () => {
      const out = await spawn(argv, { stdin: keys, env });
      return out.code === 0 ? true : undefined;
    });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

async function setPassword(guest: Guest, password: string): Promise<void> {
  const script = await Bun.file(`${import.meta.dir}/guest/password.exp`).text();
  await guest.put("/tmp/pippin-e2e-password.exp", script);
  await guest.sh("expect /tmp/pippin-e2e-password.exp", { stdin: `${IMAGE_PASSWORD}\n${password}\n` });
  // The automatic login reads the new password from /etc/kcpassword.
  await guest.sh("sudo -n sh -c 'umask 077 && cat >/etc/kcpassword.new && mv /etc/kcpassword.new /etc/kcpassword'", {
    stdin: kcpassword(password),
  });
}

/** Builds the base VM as `BUILD`, and renames it to `BASE` when it is done. */
export async function buildBase(): Promise<void> {
  // A failed build leaves no base, so the next setup builds it again.
  await mini.remove(mini.BASE);
  if (existsSync(KNOWN_HOSTS)) await capture(["ssh-keygen", "-R", HOST_ALIAS, "-f", KNOWN_HOSTS]);
  await mini.clone(mini.IMAGE, mini.BUILD);
  await mini.start(mini.BUILD);
  const guest = new Guest(await mini.ip(mini.BUILD));
  await installKeys(guest.ip);
  await guest.waitSsh(30_000);
  const password = randomPassword();
  await setPassword(guest, password);
  await writeLogin({ username: USER, password });
  // The guest shuts itself down after ssh returns, so every clone boots from
  // a clean disk. `tart stop` would pull the plug.
  await guest.sh('{ sudo -n sh -c "sleep 1; shutdown -h now" >/dev/null 2>&1 & }');
  await mini.waitStopped(120);
  await mini.promote();
}
