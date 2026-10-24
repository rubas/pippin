// The test VM on the Mac mini: a Tart clone, bridged on en0, so that pippin
// on asrock reaches its Screen Sharing over the LAN. Every call goes to the
// mini over ssh, as a zsh script on stdin.

import { capture, captureBytes } from "./proc";

/** The image of the base VM: macOS 27, user `admin`, automatic login. */
export const IMAGE = "ghcr.io/cirruslabs/macos-golden-gate-vanilla:27.0";
/** `setup` builds this VM once. Each `up` boots a fresh clone of it. */
export const BASE = "pippin-vm-base";
/** `setup` builds the base under this name, and renames it when it is done. */
export const BUILD = "pippin-vm-new";
/** The clone that `up` boots and `down` deletes. */
export const RUN = "pippin-vm";

// One launchd job in the GUI session of `rubas` runs `tart run` for the VM
// of the moment. The job removes itself when the VM stops.
const JOB = "dev.rubas.pippin-vm";
const LOG = "~/Library/Logs/pippin-vm.log";
// The owner file names the session that holds the VM. mkdir is atomic, so
// only one session at a time takes it, from any machine.
const LOCK = "~/.cache/pippin-vm.lock";
// Tart fails at once with this message while two VMs run. The CI runners
// and the Safari VM share the two slots of the Mac.
const SLOTS_FULL = "exceeds the system limit";
const SLOT_WAIT_MIN = 10;

const PRELUDE = "set -eu\nPATH=/opt/homebrew/bin:$PATH\n";

function mini(script: string): Promise<string> {
  return capture(["ssh", "-o", "ConnectTimeout=10", "macmini", "zsh", "-l"], { stdin: PRELUDE + script });
}

export function readFile(path: string): Promise<Uint8Array> {
  return captureBytes(["ssh", "-o", "ConnectTimeout=10", "macmini", "cat", path]);
}

/** Takes the lock for `owner`. Fails when another session holds it. */
export async function lock(owner: string): Promise<void> {
  await mini(`mkdir -p ~/.cache
if ! mkdir ${LOCK} 2>/dev/null; then
  echo "$(cat ${LOCK}/owner 2>/dev/null) holds the pippin VM" >&2
  exit 1
fi
echo '${owner}' >${LOCK}/owner
`);
}

/** Frees the lock, only when `owner` holds it. Returns false otherwise. */
export async function unlock(owner: string): Promise<boolean> {
  const out = await mini(`if [ "$(cat ${LOCK}/owner 2>/dev/null)" = '${owner}' ]; then
  rm -r ${LOCK}; echo freed
fi
`);
  return out.trim() === "freed";
}

export async function holder(): Promise<string | undefined> {
  const out = await mini(`cat ${LOCK}/owner 2>/dev/null || true`);
  return out.trim() || undefined;
}

export async function exists(vm: string): Promise<boolean> {
  return (await mini(`[ -d ~/.tart/vms/${vm} ] && echo yes || true`)).trim() === "yes";
}

/**
 * Clones `source` as `vm`. A clone of a local VM gets a new MAC address, so
 * `keepMac` takes the MAC of the source back: every run then keeps one DHCP
 * lease and one address on the LAN.
 */
export async function clone(source: string, vm: string, keepMac = false): Promise<void> {
  const mac = keepMac
    ? `cfg=~/.tart/vms/${vm}/config.json
jq --arg mac "$(jq -r .macAddress ~/.tart/vms/${source}/config.json)" '.macAddress = $mac' "$cfg" >"$cfg.new"
mv "$cfg.new" "$cfg"
`
    : "";
  await mini(`tart delete ${vm} 2>/dev/null || true
tart clone ${source} ${vm}
${mac}`);
}

/**
 * Starts `vm` headless in the launchd job. While both VM slots are busy, it
 * tries again every 30 s, for up to 10 minutes.
 */
export async function start(vm: string): Promise<void> {
  await mini(`touch ${LOG}
end=$((SECONDS + ${SLOT_WAIT_MIN * 60})) said=
while [ $SECONDS -lt $end ]; do
  from=$(($(wc -l <${LOG}) + 1))
  launchctl submit -l ${JOB} -o ${LOG} -e ${LOG} -- /bin/zsh -lc \\
    'echo "${vm}: start $(date -u +%FT%TZ)"; tart run --no-graphics --net-bridged=en0 ${vm}; echo "${vm}: exit $? $(date -u +%FT%TZ)"; launchctl remove ${JOB}'
  sleep 5
  launchctl list ${JOB} >/dev/null 2>&1 && exit 0
  if ! tail -n +$from ${LOG} | grep -q "${SLOTS_FULL}"; then
    tail -n +$from ${LOG} >&2
    exit 1
  fi
  if [ -z "$said" ]; then
    echo "both VM slots of the Mac are busy; waiting up to ${SLOT_WAIT_MIN} min for one" >&2
    said=1
  fi
  sleep 25
done
echo "both VM slots stayed busy for ${SLOT_WAIT_MIN} min" >&2
exit 1
`);
}

/** The LAN address of `vm`, from the ARP table of the mini. */
export async function ip(vm: string): Promise<string> {
  return (await mini(`tart ip ${vm} --resolver arp --wait 120`)).trim();
}

/** Waits until the launchd job ends, after the guest shut itself down. */
export async function waitStopped(timeoutS: number): Promise<void> {
  await mini(`for _ in $(seq ${timeoutS}); do
  launchctl list ${JOB} >/dev/null 2>&1 || exit 0
  sleep 1
done
echo "the VM still runs after ${timeoutS} s" >&2
exit 1
`);
}

/** Stops every VM of this tool that runs, and deletes the clones. */
export async function stopAndDelete(): Promise<void> {
  await mini(`for vm in ${RUN} ${BUILD}; do tart stop $vm --timeout 5 >/dev/null 2>&1 || true; done
for _ in $(seq 60); do launchctl list ${JOB} >/dev/null 2>&1 || break; sleep 1; done
if launchctl list ${JOB} >/dev/null 2>&1; then
  echo "the VM did not stop; see ${LOG} on the mini" >&2
  exit 1
fi
for vm in ${RUN} ${BUILD}; do tart delete $vm 2>/dev/null || true; done
`);
}

export async function remove(vm: string): Promise<void> {
  await mini(`tart delete ${vm} 2>/dev/null || true`);
}

/** Makes the VM that `setup` built the base. */
export async function promote(): Promise<void> {
  await mini(`tart rename ${BUILD} ${BASE}`);
}

/** The lines of the mini's own `authorized_keys`, without their `from=` option. */
export async function authorizedKeys(): Promise<string> {
  return mini(`sed -E 's/^from="[^"]*",?//' ~/.ssh/authorized_keys`);
}
