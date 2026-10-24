// Runs a child process and collects its output. Data for the child goes on
// stdin, never into an argument, so that a password stays out of `ps`.

export type Output = { code: number; stdout: Uint8Array; stderr: string };

export type Options = {
  stdin?: string | Uint8Array;
  env?: Record<string, string>;
  cwd?: string;
};

export async function spawn(argv: string[], opts: Options = {}): Promise<Output> {
  const child = Bun.spawn(argv, {
    stdin: opts.stdin === undefined ? "ignore" : "pipe",
    stdout: "pipe",
    stderr: "pipe",
    cwd: opts.cwd,
    env: opts.env && { ...process.env, ...opts.env },
  });
  if (opts.stdin !== undefined && child.stdin) {
    child.stdin.write(opts.stdin);
    await child.stdin.end();
  }
  const [stdout, stderr, code] = await Promise.all([
    new Response(child.stdout).bytes(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  return { code, stdout, stderr };
}

/** Runs the command, and returns its stdout as text. Throws when it fails. */
export async function capture(argv: string[], opts: Options = {}): Promise<string> {
  return new TextDecoder().decode(await captureBytes(argv, opts));
}

export async function captureBytes(argv: string[], opts: Options = {}): Promise<Uint8Array> {
  const out = await spawn(argv, opts);
  if (out.code !== 0) {
    const what = argv.slice(0, 2).join(" ");
    throw new Error(`${what} exited with ${out.code}: ${out.stderr.trim()}`);
  }
  return out.stdout;
}

export const sleep = (ms: number) => Bun.sleep(ms);

/** Calls `probe` until it returns a value, or throws after `timeoutMs`. */
export async function waitFor<T>(
  what: string,
  timeoutMs: number,
  probe: () => Promise<T | undefined>,
): Promise<T> {
  const end = Date.now() + timeoutMs;
  for (;;) {
    const value = await probe();
    if (value !== undefined) return value;
    if (Date.now() > end) throw new Error(`timed out after ${timeoutMs / 1000} s: ${what}`);
    await sleep(1000);
  }
}
