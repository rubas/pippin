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

// `cancel` writes this file, and the start loop ends when it sees it. A
// local kill of ssh would leave the remote loop running.
const CANCEL = `${LOCK}/cancel`;

const PRELUDE = "set -eu\nPATH=/opt/homebrew/bin:$PATH\n";

// The scripts that run on the mini now, so that `cancel` can wait for them.
const inflight = new Set<Promise<string>>();

function mini(script: string): Promise<string> {
  const call = capture(["ssh", "-o", "ConnectTimeout=10", "macmini", "zsh", "-l"], { stdin: PRELUDE + script });
  inflight.add(call);
  void call.catch(() => undefined).finally(() => inflight.delete(call));
  return call;
}

/**
 * Ends the start loop of `owner`, and waits until every script of this
 * process on the mini has ended. Then no VM starts after a stop.
 */
export async function cancel(owner: string): Promise<void> {
  const running = [...inflight];
  await mini(`if [ "$(cat ${LOCK}/owner 2>/dev/null)" = '${owner}' ]; then touch ${CANCEL}; fi`);
  await Promise.allSettled(running);
}

export function readFile(path: string): Promise<Uint8Array> {
  return captureBytes(["ssh", "-o", "ConnectTimeout=10", "macmini", "cat", path]);
}

// The guest has no compiler, so the mini builds the Swift tools of e2e/guest/.
const TOOLS = ".cache/pippin-e2e";

/**
 * Builds a Swift tool of e2e/guest/ on the mini, once for each version of its
 * source, and returns the binary.
 */
export async function swiftTool(name: string): Promise<Uint8Array> {
  const source = await Bun.file(`${import.meta.dir}/guest/${name}.swift`).text();
  const bin = `${TOOLS}/${name}-${new Bun.CryptoHasher("sha256").update(source).digest("hex").slice(0, 16)}`;
  await mini(`cd ~
if [ ! -x ${bin} ]; then
  mkdir -p ${TOOLS}
  rm -f ${TOOLS}/${name}-*(N)
  src=$(mktemp -d)
  trap 'rm -rf -- "$src"' EXIT
  cat >$src/${name}.swift <<'SWIFT'
${source}
SWIFT
  swiftc -O -o $src/${name} $src/${name}.swift
  mv $src/${name} ${bin}
fi
`);
  return readFile(bin);
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

/** Frees the lock, only when `owner` holds it. */
export async function unlock(owner: string): Promise<void> {
  await mini(`if [ "$(cat ${LOCK}/owner 2>/dev/null)" = '${owner}' ]; then rm -r ${LOCK}; fi`);
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
 * tries again every 30 s, for up to 10 minutes, until `cancel`.
 */
export async function start(vm: string): Promise<void> {
  await mini(`touch ${LOG}
cancelled() { [ -e ${CANCEL} ] && echo "cancelled" >&2; }
end=$((SECONDS + ${SLOT_WAIT_MIN * 60})) said=
while [ $SECONDS -lt $end ]; do
  cancelled && exit 1
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
  for _ in $(seq 25); do cancelled && exit 1; sleep 1; done
done
echo "both VM slots stayed busy for ${SLOT_WAIT_MIN} min" >&2
exit 1
`);
}

/** The LAN address of `vm`, from the ARP table of the mini. */
export async function ip(vm: string): Promise<string> {
  return (await mini(`tart ip ${vm} --resolver arp --wait 120`)).trim();
}

// A zsh step that waits until the launchd job ends, and fails after `timeoutS`.
const jobEnded = (timeoutS: number) => `for _ in $(seq ${timeoutS}); do launchctl list ${JOB} >/dev/null 2>&1 || break; sleep 1; done
if launchctl list ${JOB} >/dev/null 2>&1; then
  echo "the VM still runs after ${timeoutS} s; see ${LOG} on the mini" >&2
  exit 1
fi
`;

/** Waits until the launchd job ends, after the guest shut itself down. */
export async function waitStopped(timeoutS: number): Promise<void> {
  await mini(jobEnded(timeoutS));
}

/** Stops every VM of this tool that runs, and deletes the clones. */
export async function stopAndDelete(): Promise<void> {
  await mini(`for vm in ${RUN} ${BUILD}; do tart stop $vm --timeout 5 >/dev/null 2>&1 || true; done
${jobEnded(60)}for vm in ${RUN} ${BUILD}; do tart delete $vm 2>/dev/null || true; done
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
