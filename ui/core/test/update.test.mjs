//! The installed copy's pure half (D53.3): the checkout refusal, the stamp, the commit log, and the
//! words the update's confirmation shows. Runs against `dist/`.

import assert from "node:assert/strict";
import test from "node:test";

import {
  FROM_CHECKOUT_VAR,
  LISTED_COMMITS,
  UPDATE_REF,
  UPDATE_REFSPEC,
  checkoutRefusal,
  installedAppDir,
  installedExecutable,
  isCommitId,
  parseCommitLog,
  parseInstallResult,
  parseInstalledStamp,
  updateSummary,
} from "../dist/index.js";

const A = "a".repeat(40);
const B = "b".repeat(40);

test("anything but the installed copy runs only when JKB_APP_FROM_CHECKOUT=1 says so; the installed copy always runs", () => {
  assert.equal(FROM_CHECKOUT_VAR, "JKB_APP_FROM_CHECKOUT");
  assert.equal(checkoutRefusal(true, {}), undefined);
  assert.equal(checkoutRefusal(false, { JKB_APP_FROM_CHECKOUT: "1" }), undefined);
  for (const env of [{}, { JKB_APP_FROM_CHECKOUT: "" }, { JKB_APP_FROM_CHECKOUT: "0" }, { JKB_APP_FROM_CHECKOUT: "yes" }]) {
    const why = checkoutRefusal(false, env, "/h/.local/share/jkb-app/app/code-factory");
    assert.match(why ?? "", /not running from its installed copy \(\/h\/\.local\/share\/jkb-app\/app\/code-factory\)/);
    assert.match(why ?? "", /JKB_APP_FROM_CHECKOUT=1/);
  }
});

test("the installed copy's place: ~/Applications on macOS, the app home elsewhere", () => {
  assert.equal(installedAppDir("darwin", "/Users/u"), "/Users/u/Applications/Code Factory.app");
  assert.equal(installedExecutable("darwin", "/Users/u"), "/Users/u/Applications/Code Factory.app/Contents/MacOS/Code Factory");
  assert.equal(installedAppDir("linux", "/home/u"), "/home/u/.local/share/jkb-app/app");
  assert.equal(installedExecutable("linux", "/home/u"), "/home/u/.local/share/jkb-app/app/code-factory");
});

test("the update takes main, and only main", () => {
  assert.equal(UPDATE_REF, "refs/remotes/origin/main");
  assert.equal(UPDATE_REFSPEC, "+refs/heads/main:refs/remotes/origin/main");
});

test("the stamp is read for a full commit id and nothing else", () => {
  assert.equal(parseInstalledStamp(`commit=${A}\n`), A);
  assert.equal(parseInstalledStamp(`other=1\ncommit=${A}`), A);
  assert.equal(parseInstalledStamp("commit=abc\n"), undefined);
  assert.equal(parseInstalledStamp(`commit=${A.toUpperCase()}\n`), undefined);
  assert.equal(parseInstalledStamp(""), undefined);
  assert.ok(isCommitId(A));
  assert.ok(!isCommitId(`${A}\n`));
  assert.ok(!isCommitId(42));
});

test("git log lines parse to id and subject; anything else is dropped", () => {
  const log = `${A}\tfirst: a subject\twith a tab\n\nnot a commit\tline\n${B}\tsecond\n`;
  assert.deepEqual(parseCommitLog(log), [
    { id: A, subject: "first: a subject\twith a tab" },
    { id: B, subject: "second" },
  ]);
});

test("nothing to take is no confirmation at all", () => {
  assert.equal(updateSummary({ installed: A, target: A, commits: [], diverged: false }), undefined);
});

test("the confirmation names the range and lists the commits, capped", () => {
  const commits = Array.from({ length: LISTED_COMMITS + 3 }, (_, i) => ({ id: `${i}`.padStart(40, "c"), subject: `change ${i}` }));
  const s = updateSummary({ installed: A, target: B, commits, diverged: false });
  assert.ok(s !== undefined);
  assert.equal(s.message, `Take ${LISTED_COMMITS + 3} commits from main?`);
  assert.match(s.detail, new RegExp(`^${A.slice(0, 12)}\\.\\.${B.slice(0, 12)}`));
  assert.match(s.detail, /change 0/);
  assert.doesNotMatch(s.detail, new RegExp(`change ${LISTED_COMMITS}\\b`));
  assert.match(s.detail, /… and 3 more/);
  assert.match(s.detail, /relaunched/);
  assert.doesNotMatch(s.detail, /rewritten/);
  const one = updateSummary({ installed: A, target: B, commits: [{ id: B, subject: "x" }], diverged: false });
  assert.equal(one?.message, "Take 1 commit from main?");
});

test("a rewritten main and a missing stamp are said, not hidden", () => {
  assert.match(updateSummary({ installed: A, target: B, commits: [], diverged: true })?.detail ?? "", /not an ancestor of main/);
  const fresh = updateSummary({ installed: undefined, target: B, commits: [], diverged: false });
  assert.equal(fresh?.message, "Build Code Factory from main?");
  assert.match(fresh?.detail ?? "", /No installed commit is recorded/);
});

test("the install step's result is read for its status and commit", () => {
  assert.deepEqual(parseInstallResult(`status=76\ncommit=${A}\n`), { status: 76, commit: A });
  assert.deepEqual(parseInstallResult("status=75\ncommit=\n"), { status: 75, commit: undefined });
  assert.equal(parseInstallResult("commit=x\n"), undefined);
  assert.equal(parseInstallResult(""), undefined);
});
