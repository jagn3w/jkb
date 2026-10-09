//! The Container tab's data (D53.8), minus the window: the kit's `run.sh` flags the tab's buttons
//! run, and what `run.sh --status` answers about the container.
//
// The shell side is the source of truth: `.container/run.sh` (the flags, `print_status`, and the
// drift rules `args_drift`/`image_drift` the start path refuses on) and `.container/lib.sh` (the kit's
// location, `DC_KIT_DIR`, and the commit and branch it records). Nothing here re-decides any of it:
// the drift words are run.sh's, read rather than recomputed, so the tab calls a container stale
// exactly when a start would refuse it.

/** A button on the Container tab: one of the kit's `run.sh` flags. */
export type ContainerAction = "build" | "verify" | "stop" | "remove" | "install-extensions";

export interface ContainerActionSpec {
  readonly id: ContainerAction;
  readonly label: string;
  /** The `run.sh` flag it runs. */
  readonly flag: string;
  /** What it does, for the button's title. */
  readonly summary: string;
  /** Whether it ends the container (so the tab asks first). */
  readonly ends: boolean;
}

/** The buttons, in the order the tab draws them. Each is exactly one `run.sh` flag, never two. */
export const CONTAINER_ACTIONS: readonly ContainerActionSpec[] = [
  {
    id: "build",
    label: "Build",
    flag: "--build",
    summary: "Rebuild the image from the kit, then start the container on it (run.sh --build).",
    ends: false,
  },
  {
    id: "verify",
    label: "Verify",
    flag: "--verify",
    summary: "Check the running container again with verify.sh; builds and starts nothing (run.sh --verify).",
    ends: false,
  },
  {
    id: "install-extensions",
    label: "Install extensions",
    flag: "--install-extensions",
    summary: "Install the VS Code extensions into the running container (run.sh --install-extensions).",
    ends: false,
  },
  {
    id: "stop",
    label: "Stop",
    flag: "--stop",
    summary: "Stop the container; its volumes and image survive (run.sh --stop).",
    ends: true,
  },
  {
    id: "remove",
    label: "Remove",
    flag: "--rm",
    summary: "Stop and remove the container, so the next start redoes setup; volumes and image survive (run.sh --rm).",
    ends: true,
  },
];

export function isContainerAction(value: unknown): value is ContainerAction {
  return CONTAINER_ACTIONS.some((a) => a.id === value);
}

/** The flag `action` runs. */
export function flagFor(action: ContainerAction): string {
  const spec = CONTAINER_ACTIONS.find((a) => a.id === action);
  if (spec === undefined) throw new Error(`not a container action: ${String(action)}`);
  return spec.flag;
}

/**
 * Where the kit lives under the account's home: lib.sh's `DC_KIT_DIR`, which is deliberately not
 * overridable from the environment. The app asks the kit's own `run.sh --kit-path` to confirm it.
 */
export const KIT_DIR_IN_HOME = ".local/share/jkb-container-kit/kit";

/** The kit's `run.sh`, relative to the kit. */
export const KIT_RUN_SH = ".container/run.sh";

/** What an image says about itself: its id and the labels the kit's build stamps. */
export interface ImageStamp {
  readonly id: string;
  readonly created: string | null;
  readonly builtAt: string | null;
  readonly sourceCommit: string | null;
  readonly sourceBranch: string | null;
}

/** How a container differs from what a start would make it; `null` when there is no container. */
export type ArgsDrift = "same" | "differs" | "unrecorded";
export type ImageDrift = "same" | "differs" | "unknown";

export interface ContainerState {
  /** docker's `.State.Status`: running, exited, created, paused, … */
  readonly state: string;
  readonly imageId: string;
  readonly argsHash: string | null;
  /** The image the container was created from, which may be older than the tag's. */
  readonly image: ImageStamp | null;
}

/** `run.sh --status`, schema 1. */
export interface ContainerStatus {
  readonly docker: "reachable" | "unreachable";
  readonly name: string;
  readonly image: string;
  readonly kit: string | null;
  readonly checkout: string | null;
  /** The kit's paths that differ from its checkout: what `--install-kit` would take. */
  readonly kitChanged: readonly string[];
  /**
   * The command that refreshes the kit, as run.sh writes it (`KIT_REFRESH`: the KIT's run.sh, never
   * the checkout's, which the agent can rewrite). Shown verbatim; `null` when there is no kit.
   */
  readonly kitRefresh: string | null;
  readonly wantArgsHash: string;
  /** What the image tag holds now. */
  readonly imageOnDisk: ImageStamp | null;
  readonly container: ContainerState | null;
  readonly drift: { readonly args: ArgsDrift | null; readonly image: ImageDrift | null };
}

export type ContainerResult<T> = { readonly ok: true; readonly value: T } | { readonly ok: false; readonly error: string };

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}
const isString = (v: unknown): v is string => typeof v === "string";
const optString = (v: unknown): string | null | undefined => (v === null || v === undefined ? null : isString(v) ? v : undefined);

function stamp(v: unknown): ImageStamp | null | undefined {
  if (v === null || v === undefined) return null;
  if (!isObject(v) || !isString(v["id"])) return undefined;
  const fields = [v["created"], v["built_at"], v["source_commit"], v["source_branch"]].map(optString);
  if (fields.some((f) => f === undefined)) return undefined;
  const [created, builtAt, sourceCommit, sourceBranch] = fields as (string | null)[];
  return {
    id: v["id"],
    created: created ?? null,
    builtAt: builtAt ?? null,
    sourceCommit: sourceCommit ?? null,
    sourceBranch: sourceBranch ?? null,
  };
}

const ARGS_DRIFT: readonly string[] = ["same", "differs", "unrecorded"];
const IMAGE_DRIFT: readonly string[] = ["same", "differs", "unknown"];

/**
 * `run.sh --status`'s stdout as a `ContainerStatus`, or why it is not one. A schema this build does
 * not know is refused rather than read as if it were schema 1.
 */
export function parseContainerStatus(text: string): ContainerResult<ContainerStatus> {
  const bad = (why: string): ContainerResult<ContainerStatus> => ({ ok: false, error: `run.sh --status: ${why}` });
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    return bad("not JSON");
  }
  if (!isObject(v)) return bad("not an object");
  if (v["schema"] !== 1) return bad(`schema ${JSON.stringify(v["schema"])}, this app reads schema 1`);
  const docker = v["docker"];
  if (docker !== "reachable" && docker !== "unreachable") return bad("docker is neither reachable nor unreachable");
  const { name, image, want_args_hash: want } = v;
  if (!isString(name) || !isString(image) || !isString(want)) return bad("name, image or want_args_hash is missing");
  const kit = optString(v["kit"]);
  const checkout = optString(v["checkout"]);
  const kitRefresh = optString(v["kit_refresh"]);
  if (kit === undefined || checkout === undefined) return bad("kit or checkout is not a path");
  if (kitRefresh === undefined) return bad("kit_refresh is not a command");
  const changed = v["kit_changed"];
  if (!Array.isArray(changed) || !changed.every(isString)) return bad("kit_changed is not a list of paths");
  const onDisk = stamp(v["image_on_disk"]);
  if (onDisk === undefined) return bad("image_on_disk is malformed");

  let container: ContainerState | null = null;
  const c = v["container"];
  if (c !== null && c !== undefined) {
    if (!isObject(c) || !isString(c["state"]) || !isString(c["image_id"])) return bad("container is malformed");
    const argsHash = optString(c["args_hash"]);
    const cImage = stamp(c["image"]);
    if (argsHash === undefined || cImage === undefined) return bad("container is malformed");
    container = { state: c["state"], imageId: c["image_id"], argsHash, image: cImage };
  }

  const drift = v["drift"];
  if (!isObject(drift)) return bad("drift is missing");
  const args = drift["args"] ?? null;
  const img = drift["image"] ?? null;
  if (args !== null && !(isString(args) && ARGS_DRIFT.includes(args))) return bad(`args drift ${JSON.stringify(args)}`);
  if (img !== null && !(isString(img) && IMAGE_DRIFT.includes(img))) return bad(`image drift ${JSON.stringify(img)}`);

  return {
    ok: true,
    value: {
      docker,
      name,
      image,
      kit,
      checkout,
      kitChanged: [...changed],
      kitRefresh,
      wantArgsHash: want,
      imageOnDisk: onDisk,
      container,
      drift: { args: args as ArgsDrift | null, image: img as ImageDrift | null },
    },
  };
}

/** One line of what the tab says about the container's standing, and how loudly. */
export interface Finding {
  readonly level: "ok" | "note" | "stale";
  readonly text: string;
}

/** The states run.sh's start path takes up again with `docker start` (its `running|exited|created` arm). */
const RESTARTABLE: readonly string[] = ["exited", "created"];

/** The states `run.sh --stop` acts on: `docker stop` ends a paused or restarting container too. */
const STOPPABLE: readonly string[] = ["running", "paused", "restarting"];

/**
 * What the tab says about `s`, in order: the container's state, then each way it differs from what a
 * start would make it, each with the remedy `run.sh` itself prints. Pure, so every case is a test row.
 *
 * What Build does to a stopped container is said with its condition, because `--status` cannot know
 * it: Build REBUILDS the image first, and a build that changes it (a reinstalled kit's new source
 * labels, a changed Dockerfile) makes the start path refuse the old container as running an older
 * build (review s8 round 2: "Build starts it again" was said, and Build then refused). So a stopped
 * container with no drift is told both outcomes. A stale one says its state and lets the stale lines
 * carry the remedy; one in a state the start path has no arm for (paused, restarting, dead) cannot be
 * started by Build at all -- its `docker run` would collide with the name -- so it is stale in itself.
 */
export function findings(s: ContainerStatus): Finding[] {
  if (s.docker === "unreachable") {
    return [{ level: "stale", text: "The docker daemon is not reachable. Start Docker, then refresh." }];
  }
  const out: Finding[] = [];
  if (s.container === null) {
    out.push({ level: "note", text: `There is no container named ${s.name}. Build creates and starts it.` });
  } else if (s.container.state === "running") {
    out.push({ level: "ok", text: `${s.name} is running.` });
  } else if (RESTARTABLE.includes(s.container.state)) {
    const startable = s.drift.args === "same" && s.drift.image !== "differs";
    out.push({
      level: "note",
      text: `${s.name} is ${s.container.state}.${startable ? " Build rebuilds the image, then starts it again; if the build changed the image, run.sh refuses the old container: Remove, then Build." : ""}`,
    });
  } else {
    out.push({
      level: "stale",
      text: `${s.name} is ${s.container.state}, a state run.sh does not start a container from. Remove it, then Build.`,
    });
  }
  if (s.imageOnDisk === null) out.push({ level: "note", text: `There is no ${s.image} image yet. Build makes one.` });
  if (s.drift.args === "differs") {
    out.push({
      level: "stale",
      text: "The container was created from a different container.json or seccomp profile. Remove it, then Build: a start reuses the old flags.",
    });
  } else if (s.drift.args === "unrecorded") {
    out.push({
      level: "stale",
      text: "The container carries no record of what it was created from. Remove it, then Build.",
    });
  }
  if (s.drift.image === "differs") {
    out.push({
      level: "stale",
      text: `The container runs an older build of ${s.image} than the one on disk. Remove it, then Build.`,
    });
  }
  if (s.kitChanged.length > 0) {
    out.push({
      level: "note",
      // run.sh's own command, never one composed here: the remedy is a program the operator runs on
      // the host by hand, and the checkout's run.sh is the agent's to rewrite (review s8 round 1).
      text: `The checkout has changed since the kit was installed (${s.kitChanged.join(", ")}). Review the changes, then reinstall the kit${s.kitRefresh === null ? " with the kit's own run.sh --install-kit." : `: ${s.kitRefresh}`}`,
    });
  }
  return out;
}

/**
 * Whether a button can do anything for the container `s` describes, and if not, why. With no status
 * (it could not be read) every button is offered: `run.sh` decides, and says why when it refuses.
 */
export function availability(action: ContainerAction, s: ContainerStatus | undefined): { readonly enabled: boolean; readonly why?: string } {
  if (s === undefined) return { enabled: true };
  if (s.docker === "unreachable") return { enabled: false, why: "the docker daemon is not reachable" };
  const state = s.container?.state;
  const running = state === "running";
  switch (action) {
    case "build":
      return { enabled: true };
    case "verify":
    case "install-extensions":
      return running ? { enabled: true } : { enabled: false, why: `${s.name} is not running` };
    case "stop":
      return state !== undefined && STOPPABLE.includes(state) ? { enabled: true } : { enabled: false, why: `${s.name} is not running` };
    case "remove":
      return s.container !== null ? { enabled: true } : { enabled: false, why: `there is no container named ${s.name}` };
  }
}

/** A commit as the tab shows it: the first 12 hex digits, or the value as it came when it is not hex. */
export function shortCommit(commit: string | null): string {
  if (commit === null) return "unknown";
  return /^[0-9a-f]{40}([0-9a-f]{24})?$/.test(commit) ? commit.slice(0, 12) : commit;
}
