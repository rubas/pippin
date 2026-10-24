// The login of the test VM, in Infisical `r-dotfiles`, env `dev`, path
// `/pippin-vm`. It goes through the API as the identity `agent`, so that the
// password stays in memory: no argument, no file, no log.

const API = "https://secrets.rubas.dev/api";
const PROJECT = "270d0dff-f490-40fa-b44c-74c1953174e4"; // r-dotfiles
const ENV = "dev";
const FOLDER = "pippin-vm";
const SCOPE = { projectId: PROJECT, environment: ENV, secretPath: `/${FOLDER}` };

export type Login = { username: string; password: string };

async function token(): Promise<string> {
  const path = `${process.env.XDG_RUNTIME_DIR}/infisical/agent-token`;
  const file = Bun.file(path);
  if (!(await file.exists())) throw new Error(`${path} is missing; check infisical-agent.service`);
  return (await file.text()).trim();
}

async function call(method: string, path: string, body?: object): Promise<Response> {
  return fetch(`${API}${path}`, {
    method,
    headers: { Authorization: `Bearer ${await token()}`, "Content-Type": "application/json" },
    body: body && JSON.stringify(body),
  });
}

async function fail(what: string, res: Response): Promise<never> {
  const { message } = (await res.json().catch(() => ({}))) as { message?: string };
  throw new Error(`Infisical: ${what}: HTTP ${res.status} ${message ?? ""}`.trim());
}

async function read(key: string): Promise<string | undefined> {
  const q = new URLSearchParams(SCOPE);
  const res = await call("GET", `/v4/secrets/${key}?${q}`);
  if (res.status === 404) return undefined;
  if (!res.ok) return fail(`read ${key}`, res);
  return ((await res.json()) as { secret: { secretValue: string } }).secret.secretValue;
}

/** The login, or undefined when `setup` has not stored one. */
export async function readLogin(): Promise<Login | undefined> {
  const [username, password] = await Promise.all([read("USERNAME"), read("PASSWORD")]);
  return username && password ? { username, password } : undefined;
}

async function write(key: string, value: string): Promise<void> {
  const body = { ...SCOPE, secretValue: value, type: "shared" };
  const created = await call("POST", `/v4/secrets/${key}`, body);
  if (created.ok) return;
  if (created.status !== 400) return fail(`create ${key}`, created);
  const updated = await call("PATCH", `/v4/secrets/${key}`, body);
  if (!updated.ok) return fail(`update ${key}`, updated);
}

export async function writeLogin(login: Login): Promise<void> {
  const folder = await call("POST", "/v2/folders", { projectId: PROJECT, environment: ENV, name: FOLDER, path: "/" });
  // 400: the folder exists.
  if (!folder.ok && folder.status !== 400) await fail("create the folder", folder);
  await write("USERNAME", login.username);
  await write("PASSWORD", login.password);
}
