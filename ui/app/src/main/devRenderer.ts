//! Whether to load the renderer from a dev server, and which (D53.1).
//
// `electron-vite dev` sets `ELECTRON_RENDERER_URL` to its dev server, and main then loads that URL
// as the app's own page — the page `isAppPage` trusts with the bridge, and so with every op the root
// token can run. An inherited variable must not be able to make that a remote page, so it is
// honoured only when the dev opt-in is set too and it names an http server on loopback; set any
// other way, the app refuses to start rather than quietly loading its built page instead.
//
// No Electron import: plain logic, tested without launching Electron.

/** The variable `electron-vite dev` sets to its dev server's URL. */
export const RENDERER_URL_VAR = "ELECTRON_RENDERER_URL";

/** The opt-in to load a dev server's page, set by `pnpm run dev` and nowhere else. */
export const DEV_RENDERER_VAR = "JKB_APP_DEV_RENDERER";

const LOOPBACK_HOSTS: ReadonlySet<string> = new Set(["127.0.0.1", "[::1]", "localhost"]);

/** Where the renderer comes from: the built file, a dev server's URL, or nowhere (refused, and why). */
export type RendererSource =
  | { readonly kind: "file" }
  | { readonly kind: "dev"; readonly url: string }
  | { readonly kind: "refused"; readonly reason: string };

/** Read the renderer's source from the environment. A packaged app always loads its built file. */
export function rendererSource(
  isPackaged: boolean,
  env: Readonly<Record<string, string | undefined>>,
): RendererSource {
  const raw = env[RENDERER_URL_VAR]?.trim();
  if (isPackaged || raw === undefined || raw === "") return { kind: "file" };
  if (env[DEV_RENDERER_VAR] !== "1") {
    return {
      kind: "refused",
      reason:
        `${RENDERER_URL_VAR} is set, but ${DEV_RENDERER_VAR}=1 is not: the app loads a dev server's page ` +
        `only from \`pnpm run dev\`. Unset ${RENDERER_URL_VAR} to run the built page.`,
    };
  }
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    return { kind: "refused", reason: `${RENDERER_URL_VAR} is not a URL: ${raw}` };
  }
  if (url.protocol !== "http:" || !LOOPBACK_HOSTS.has(url.hostname) || url.username !== "" || url.password !== "") {
    return {
      kind: "refused",
      reason: `${RENDERER_URL_VAR} must be an http:// URL on loopback (127.0.0.1, [::1] or localhost), not ${raw}`,
    };
  }
  return { kind: "dev", url: url.href };
}
