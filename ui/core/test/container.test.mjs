//! The Container tab's data as `@jkb/core` reads it (D53.8): the buttons' flags, `run.sh --status`
//! parsed, and what the tab says about each standing. Runs against `dist/`.

import assert from "node:assert/strict";
import test from "node:test";

import { CONTAINER_ACTIONS, availability, findings, flagFor, isContainerAction, parseContainerStatus, shortCommit } from "../dist/index.js";

const SHA = "0123456789abcdef0123456789abcdef01234567";

/** `run.sh --status` as print_status writes it, with `over` merged in. */
const status = (over = {}) =>
  JSON.stringify({
    schema: 1,
    docker: "reachable",
    name: "jkb-dev",
    image: "jkb-dev",
    kit: "/home/me/.local/share/jkb-container-kit/kit",
    checkout: "/home/me/repos/jkb",
    kit_changed: [],
    want_args_hash: "h1",
    image_on_disk: { id: "sha256:a", created: "2026-10-08T10:00:00Z", built_at: "2026-10-08T10:00:01Z", source_commit: SHA, source_branch: "main" },
    container: {
      state: "running",
      image_id: "sha256:a",
      args_hash: "h1",
      image: { id: "sha256:a", created: null, built_at: null, source_commit: null, source_branch: null },
    },
    drift: { args: "same", image: "same" },
    ...over,
  });

test("each button is exactly one of the kit's run.sh flags", () => {
  assert.deepEqual(
    CONTAINER_ACTIONS.map((a) => [a.id, a.flag]),
    [
      ["build", "--build"],
      ["verify", "--verify"],
      ["install-extensions", "--install-extensions"],
      ["stop", "--stop"],
      ["remove", "--rm"],
    ],
  );
  assert.deepEqual(
    CONTAINER_ACTIONS.filter((a) => a.ends).map((a) => a.id),
    ["stop", "remove"],
    "the two that end the container are the two the tab asks about first",
  );
  assert.equal(flagFor("remove"), "--rm");
  assert.equal(isContainerAction("verify"), true);
  assert.equal(isContainerAction("--build"), false, "a flag is not an action: the renderer names actions, main picks flags");
  assert.equal(isContainerAction("shell"), false);
  assert.throws(() => flagFor("shell"));
});

test("run.sh --status parses into the tab's shape, labels and drift included", () => {
  const r = parseContainerStatus(status());
  assert.equal(r.ok, true, r.ok ? "" : r.error);
  const s = r.value;
  assert.equal(s.imageOnDisk.builtAt, "2026-10-08T10:00:01Z");
  assert.equal(s.imageOnDisk.sourceCommit, SHA);
  assert.equal(s.imageOnDisk.sourceBranch, "main");
  assert.equal(s.container.state, "running");
  assert.equal(s.container.image.builtAt, null);
  assert.deepEqual(s.drift, { args: "same", image: "same" });
});

test("no container and the daemon down are answers, not errors", () => {
  const none = parseContainerStatus(status({ container: null, drift: { args: null, image: null } }));
  assert.equal(none.ok, true);
  assert.equal(none.value.container, null);
  const down = parseContainerStatus(
    JSON.stringify({ schema: 1, docker: "unreachable", name: "jkb-dev", image: "jkb-dev", kit: null, checkout: null, kit_changed: [], want_args_hash: "h", image_on_disk: null, container: null, drift: { args: null, image: null } }),
  );
  assert.equal(down.ok, true, down.ok ? "" : down.error);
  assert.equal(down.value.docker, "unreachable");
});

test("anything else is refused with the reason, never read as a status", () => {
  const refused = (text, why) => {
    const r = parseContainerStatus(text);
    assert.equal(r.ok, false, `${text} should be refused`);
    assert.match(r.error, why);
  };
  refused("error: no /x/container.json", /not JSON/);
  refused("[]", /not an object/);
  refused(status({ schema: 2 }), /schema 2/);
  refused(status({ docker: "maybe" }), /docker/);
  refused(status({ name: 7 }), /name/);
  refused(status({ kit_changed: "a" }), /kit_changed/);
  refused(status({ image_on_disk: { created: null } }), /image_on_disk/);
  refused(status({ container: { state: "running" } }), /container/);
  refused(status({ drift: { args: "stale", image: "same" } }), /args drift/);
  refused(status({ drift: { args: "same", image: "older" } }), /image drift/);
});

const say = (over) => {
  const r = parseContainerStatus(status(over));
  assert.equal(r.ok, true, r.ok ? "" : r.error);
  return findings(r.value).map((f) => [f.level, f.text]);
};

test("a running container on this declaration and the tag's image is simply running", () => {
  assert.deepEqual(say({}), [["ok", "jkb-dev is running."]]);
});

test("drift reads as stale, with run.sh's own remedy", () => {
  const args = say({ drift: { args: "differs", image: "same" } });
  assert.equal(args[1][0], "stale");
  assert.match(args[1][1], /different container\.json or seccomp profile.*Remove it, then Build/);
  const unrecorded = say({ drift: { args: "unrecorded", image: "same" } });
  assert.match(unrecorded[1][1], /no record of what it was created from/);
  const image = say({ drift: { args: "same", image: "differs" } });
  assert.equal(image[1][0], "stale");
  assert.match(image[1][1], /older build of jkb-dev/);
  assert.equal(say({ drift: { args: "same", image: "unknown" } }).length, 1, "an unknown image id is not called stale");
});

test("no container, a stopped one, no image, a changed checkout and a down daemon each say so", () => {
  assert.match(say({ container: null, drift: { args: null, image: null } })[0][1], /no container named jkb-dev/);
  assert.match(say({ container: { state: "exited", image_id: "sha256:a", args_hash: "h1", image: null } })[0][1], /is exited/);
  assert.ok(say({ image_on_disk: null }).some(([, t]) => /no jkb-dev image yet/.test(t)));
  const kit = say({ kit_changed: [".container", "scripts/lib.sh"] });
  assert.match(kit[1][1], /\.container, scripts\/lib\.sh.*\/home\/me\/repos\/jkb\/\.container\/run\.sh --install-kit/);
  const down = parseContainerStatus(status({ docker: "unreachable" }));
  assert.deepEqual(
    findings(down.value).map((f) => f.level),
    ["stale"],
    "with the daemon down, nothing else is claimed about the container",
  );
});

test("a button is offered only where it can act, and with no status every one is", () => {
  const of = (over) => {
    const r = parseContainerStatus(status(over));
    assert.equal(r.ok, true, r.ok ? "" : r.error);
    return Object.fromEntries(CONTAINER_ACTIONS.map((a) => [a.id, availability(a.id, r.value).enabled]));
  };
  assert.deepEqual(of({}), { build: true, verify: true, "install-extensions": true, stop: true, remove: true });
  assert.deepEqual(of({ container: { state: "exited", image_id: "sha256:a", args_hash: "h1", image: null } }), {
    build: true,
    verify: false,
    "install-extensions": false,
    stop: false,
    remove: true,
  });
  assert.deepEqual(of({ container: null, drift: { args: null, image: null } }), {
    build: true,
    verify: false,
    "install-extensions": false,
    stop: false,
    remove: false,
  });
  assert.deepEqual(Object.values(of({ docker: "unreachable" })), [false, false, false, false, false]);
  assert.match(availability("verify", parseContainerStatus(status({ container: null })).value).why, /not running/);
  assert.ok(CONTAINER_ACTIONS.every((a) => availability(a.id, undefined).enabled), "run.sh decides when the status could not be read");
});

test("a commit is shown short when it is hex, as it came otherwise", () => {
  assert.equal(shortCommit(SHA), "0123456789ab");
  assert.equal(shortCommit("unknown"), "unknown");
  assert.equal(shortCommit(null), "unknown");
});
