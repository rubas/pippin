// The file /etc/kcpassword holds the password of the automatic login, XORed
// with a fixed key and padded to a multiple of 12 bytes. sysadminctl cannot
// write it in this guest (`SACSetAutoLoginPassword error:22`), so `setup`
// writes it.

const KEY = [0x7d, 0x89, 0x52, 0x23, 0xd2, 0xbc, 0xdd, 0xea, 0xa3, 0xb9, 0x1f];

export function kcpassword(password: string): Uint8Array {
  const plain = new TextEncoder().encode(password);
  // At least one NUL ends the password.
  const out = new Uint8Array(Math.ceil((plain.length + 1) / 12) * 12);
  for (let i = 0; i < out.length; i++) out[i] = (plain[i] ?? 0) ^ KEY[i % KEY.length]!;
  return out;
}
