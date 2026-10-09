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
    kit_refresh: "/home/me/.local/share/jkb-container-kit/kit/.container/run.sh --install-kit",
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
  refused(status({ kit_refresh: ["run.sh"] }), /kit_refresh/);
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

test("no container, a stopped one, no image and a down daemon each say so", () => {
  assert.match(say({ container: null, drift: { args: null, image: null } })[0][1], /no container named jkb-dev/);
  assert.match(say({ container: { state: "exited", image_id: "sha256:a", args_hash: "h1", image: null } })[0][1], /is exited/);
  assert.ok(say({ image_on_disk: null }).some(([, t]) => /no jkb-dev image yet/.test(t)));
  const down = parseContainerStatus(status({ docker: "unreachable" }));
  assert.deepEqual(
    findings(down.value).map((f) => f.level),
    ["stale"],
    "with the daemon down, nothing else is claimed about the container",
  );
});

test("a stale kit's remedy is run.sh's own command, the KIT's run.sh, never the checkout's", () => {
  // The checkout's run.sh is the agent's to rewrite, and this line is a program the operator runs on
  // the host by hand (review s8 round 1: it named the checkout's).
  const kit = say({ kit_changed: [".container", "scripts/lib.sh"] });
  assert.equal(
    kit[1][1],
    "The checkout has changed since the kit was installed (.container, scripts/lib.sh). Review the changes, then reinstall the kit: /home/me/.local/share/jkb-container-kit/kit/.container/run.sh --install-kit",
  );
  assert.doesNotMatch(kit[1][1], /\/home\/me\/repos\/jkb\/\.container/);
  // Whatever run.sh says is what is shown: the app composes no path of its own.
  assert.match(say({ kit_changed: [".container"], kit_refresh: "/k/.container/run.sh --install-kit" })[1][1], /: \/k\/\.container\/run\.sh --install-kit$/);
  const none = say({ kit_changed: [".container"], kit_refresh: null })[1][1];
  assert.doesNotMatch(none, /\/home\/me\/repos/, "with no command from run.sh, no path is guessed");
  assert.match(none, /the kit's own run\.sh --install-kit/);
});

test("Build is said to start a stopped container only where the start path would", () => {
  const ctr = (state) => ({ state, image_id: "sha256:a", args_hash: "h1", image: null });
  assert.deepEqual(say({ container: ctr("exited") }), [["note", "jkb-dev is exited. Build starts it again."]]);
  assert.deepEqual(say({ container: ctr("created"), drift: { args: "same", image: "unknown" } }), [["note", "jkb-dev is created. Build starts it again."]]);
  // Drift the start path refuses: the state is said plainly, and the stale line carries the remedy.
  for (const drift of [
    { args: "differs", image: "same" },
    { args: "unrecorded", image: "same" },
    { args: "same", image: "differs" },
  ]) {
    const said = say({ container: ctr("exited"), drift });
    assert.deepEqual(said[0], ["note", "jkb-dev is exited."], JSON.stringify(drift));
    assert.ok(said.slice(1).some(([level, text]) => level === "stale" && /Remove it, then Build/.test(text)));
    assert.ok(!said.some(([, text]) => /Build starts it again/.test(text)));
  }
  // A state the start path has no arm for: Build would collide with the name.
  for (const state of ["paused", "restarting", "dead"]) {
    const said = say({ container: ctr(state) });
    assert.equal(said[0][0], "stale", state);
    assert.match(said[0][1], new RegExp(`jkb-dev is ${state}, .*Remove it, then Build`));
    assert.doesNotMatch(said[0][1], /starts it again/);
  }
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
  for (const state of ["paused", "restarting"]) {
    assert.deepEqual(
      of({ container: { state, image_id: "sha256:a", args_hash: "h1", image: null } }),
      { build: true, verify: false, "install-extensions": false, stop: true, remove: true },
      `run.sh --stop acts on a ${state} container, so Stop is offered`,
    );
  }
  assert.equal(of({ container: { state: "dead", image_id: "sha256:a", args_hash: "h1", image: null } }).stop, false);
  assert.deepEqual(Object.values(of({ docker: "unreachable" })), [false, false, false, false, false]);
  assert.match(availability("verify", parseContainerStatus(status({ container: null })).value).why, /not running/);
  assert.ok(CONTAINER_ACTIONS.every((a) => availability(a.id, undefined).enabled), "run.sh decides when the status could not be read");
});

test("a commit is shown short when it is hex, as it came otherwise", () => {
  assert.equal(shortCommit(SHA), "0123456789ab");
  assert.equal(shortCommit("unknown"), "unknown");
  assert.equal(shortCommit(null), "unknown");
});
