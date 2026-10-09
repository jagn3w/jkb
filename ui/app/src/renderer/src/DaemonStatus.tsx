import { useCallback, useEffect, useState } from "react";

import type { ApiError, Hello } from "@jkb/core";

type Status =
  | { readonly kind: "checking" }
  | { readonly kind: "connected"; readonly hello: Hello }
  | { readonly kind: "failed"; readonly error: ApiError };

/**
 * Whether `jkb serve` answers, from `GET /v1/hello` through the bridge. Checked when the window
 * opens and when clicked — never on a timer: live state arrives by subscription (D53.1).
 */
export function DaemonStatus(): React.JSX.Element {
  const [status, setStatus] = useState<Status>({ kind: "checking" });
  const [url, setUrl] = useState<string>("");

  const check = useCallback(async () => {
    setStatus({ kind: "checking" });
    try {
      const outcome = await window.jkb.hello();
      setStatus(outcome.ok ? { kind: "connected", hello: outcome.value } : { kind: "failed", error: outcome.error });
    } catch (e) {
      setStatus({ kind: "failed", error: { code: "internal", message: e instanceof Error ? e.message : String(e) } });
    }
  }, []);

  useEffect(() => {
    void check();
    window.jkb.info().then(
      (info) => setUrl(info.daemonUrl),
      () => setUrl(""),
    );
  }, [check]);

  const label =
    status.kind === "checking"
      ? "Connecting…"
      : status.kind === "connected"
        ? `jkb · schema ${status.hello.schema_version}`
        : status.error.code === "unavailable"
          ? "jkb serve unreachable"
          : status.error.code === "token_refused"
            ? // Not "unreachable": restarting jkb serve would rewrite the token and erase the
              // evidence of whatever was planted in ~/.jkb/daemon.
              "jkb token refused"
            : `jkb · ${status.error.code}`;
  const detail =
    status.kind === "failed"
      ? status.error.message
      : status.kind === "connected"
        ? `${url} · ${status.hello.ops.length} ops`
        : url;

  return (
    <button
      type="button"
      className="daemon-status"
      data-state={status.kind}
      title={`${detail}\nClick to check again.`}
      onClick={() => void check()}
      disabled={status.kind === "checking"}
    >
      <span className="dot" aria-hidden="true" />
      <span>{label}</span>
    </button>
  );
}
