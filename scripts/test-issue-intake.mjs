#!/usr/bin/env node
// Offline validation & fixture test suite for:
// 1. .github/ISSUE_TEMPLATE/*.yml and config.yml schema + label alignment with scripts/setup-github-labels.sh
// 2. Byte-for-byte logic sync between scripts/issue-intake.mjs and .github/workflows/issue-intake.yml
// 3. Representative event fixtures and mocked Octokit API tests across all intake scenarios

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import {
  BOT_COMMENT_MARKER,
  evaluateIssueIntake,
  isVerifiedBotComment,
  runIssueIntake,
} from "./issue-intake.mjs";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(__dirname, "..");

function createMockGitHub({
  initialLabels = [],
  initialComments = [],
  initialEvents = [],
} = {}) {
  const state = {
    labels: new Set(initialLabels),
    comments: initialComments.map((c) => ({ ...c })),
    events: initialEvents.map((e) => ({ ...e })),
    calls: {
      listComments: 0,
      listEventsForIssue: 0,
      addLabels: [],
      removeLabel: [],
      createComment: [],
      updateComment: [],
    },
    nextCommentId: 1000,
  };

  const paginateSlice = (arr, page = 1, per_page = 100) => {
    const start = (page - 1) * per_page;
    return arr.slice(start, start + per_page);
  };

  const github = {
    rest: {
      issues: {
        async listComments({ page = 1, per_page = 100 }) {
          state.calls.listComments += 1;
          return { data: paginateSlice(state.comments, page, per_page) };
        },
        async listEventsForIssue({ page = 1, per_page = 100 }) {
          state.calls.listEventsForIssue += 1;
          return { data: paginateSlice(state.events, page, per_page) };
        },
        async addLabels({ labels }) {
          state.calls.addLabels.push([...labels]);
          for (const l of labels) {
            state.labels.add(l);
            state.events.push({
              event: "labeled",
              label: { name: l },
              actor: { login: "github-actions[bot]", type: "Bot" },
            });
          }
          return { data: [...state.labels].map((name) => ({ name })) };
        },
        async removeLabel({ name }) {
          state.calls.removeLabel.push(name);
          if (!state.labels.has(name)) {
            const err = new Error("Label not found");
            err.status = 404;
            throw err;
          }
          state.labels.delete(name);
          state.events.push({
            event: "unlabeled",
            label: { name },
            actor: { login: "github-actions[bot]", type: "Bot" },
          });
          return { data: {} };
        },
        async createComment({ body }) {
          const comment = {
            id: (state.nextCommentId += 1),
            body,
            user: { login: "github-actions[bot]", type: "Bot" },
          };
          state.comments.push(comment);
          state.calls.createComment.push(comment);
          return { data: comment };
        },
        async updateComment({ comment_id, body }) {
          const target = state.comments.find((c) => c.id === comment_id);
          assert.ok(target, `updateComment target #${comment_id} must exist`);
          target.body = body;
          state.calls.updateComment.push({ comment_id, body });
          return { data: target };
        },
      },
    },
  };

  const makeContext = (action, issueOverrides = {}) => ({
    repo: { owner: "Hoggormino", repo: "awcc-rs" },
    payload: {
      action,
      issue: {
        number: 42,
        state: "open",
        title: "[Bug]: Sample issue",
        body: "",
        labels: [...state.labels].map((name) => ({ name })),
        user: { login: "linux-user", type: "User" },
        ...issueOverrides,
      },
    },
  });

  return { state, github, makeContext };
}

function testWorkflowSyncAndPermissions() {
  const moduleSrc = fs.readFileSync(
    path.join(REPO_ROOT, "scripts/issue-intake.mjs"),
    "utf8"
  );
  const workflowSrc = fs.readFileSync(
    path.join(REPO_ROOT, ".github/workflows/issue-intake.yml"),
    "utf8"
  );

  // Verify minimal permissions and absence of checkout in issue-intake.yml:
  assert.match(workflowSrc, /^permissions:\s*\{\}/m);
  assert.match(workflowSrc, /permissions:\s*\n\s*issues:\s*write/);
  assert.ok(
    !workflowSrc.includes("actions/checkout"),
    "issue-intake.yml must not check out contributor code"
  );
  assert.ok(
    !workflowSrc.includes("contents: write") &&
      !workflowSrc.includes("contents: read"),
    "issue-intake.yml must not request contents permissions"
  );
  assert.match(workflowSrc, /uses:\s*actions\/github-script@v9\b/);
  assert.match(
    workflowSrc,
    /group:\s*issue-intake-\$\{\{\s*github\.repository\s*\}\}-\$\{\{\s*github\.event\.issue\.number\s*\}\}/
  );

  // Verify embedded core in issue-intake.yml matches scripts/issue-intake.mjs:
  const beginMarker =
    "// BEGIN AWCC_ISSUE_INTAKE_CORE (synced with scripts/issue-intake.mjs)";
  const endMarker = "// END AWCC_ISSUE_INTAKE_CORE";
  const startIdx = workflowSrc.indexOf(beginMarker);
  const endIdx = workflowSrc.indexOf(endMarker);
  assert.ok(
    startIdx !== -1 && endIdx !== -1 && endIdx > startIdx,
    "issue-intake.yml must contain AWCC_ISSUE_INTAKE_CORE markers"
  );

  const embeddedLines = workflowSrc
    .slice(startIdx + beginMarker.length, endIdx)
    .split("\n")
    .map((line) => (line.startsWith("            ") ? line.slice(12) : line))
    .join("\n")
    .trim();

  const expectedCore = moduleSrc
    .split("\n")
    .filter((line) => !line.startsWith("//"))
    .join("\n")
    .replace(/^export /gm, "")
    .trim();

  assert.equal(
    embeddedLines,
    expectedCore,
    "Embedded script in .github/workflows/issue-intake.yml must stay in sync with scripts/issue-intake.mjs"
  );
}

function testIssueTemplatesAndLabelCatalog() {
  const rawCatalog = execFileSync(
    path.join(REPO_ROOT, "scripts/setup-github-labels.sh"),
    ["--list-json"],
    { encoding: "utf8" }
  );
  const labelCatalog = JSON.parse(rawCatalog);
  const validLabelNames = new Set(labelCatalog.map((l) => l.name));

  for (const required of [
    "bug",
    "enhancement",
    "needs-triage",
    "needs-info",
    "confirmed",
    "needs-hardware-test",
    "area: packaging",
    "area: gui",
    "area: thermal",
    "area: lighting",
    "area: hardware",
    "priority: high",
  ]) {
    assert.ok(
      validLabelNames.has(required),
      `setup-github-labels.sh must include '${required}'`
    );
  }

  const templateDir = path.join(REPO_ROOT, ".github/ISSUE_TEMPLATE");
  const configYml = fs.readFileSync(
    path.join(templateDir, "config.yml"),
    "utf8"
  );
  assert.match(configYml, /^blank_issues_enabled:\s*true$/m);
  assert.ok(configYml.includes("SUPPORT.md"));
  assert.ok(configYml.includes("SECURITY.md"));

  const formFiles = [
    "01-bug-report.yml",
    "02-hardware-compatibility.yml",
    "03-feature-request.yml",
  ];

  for (const file of formFiles) {
    const content = fs.readFileSync(path.join(templateDir, file), "utf8");
    assert.match(content, /^name:\s+\S+/m, `${file} missing name`);
    assert.match(
      content,
      /^description:\s+\S+/m,
      `${file} missing description`
    );
    const labelsMatch = content.match(/^labels:\s*\[([^\]]+)\]/m);
    assert.ok(labelsMatch, `${file} must declare labels: [...]`);
    const formLabels = labelsMatch[1]
      .split(",")
      .map((s) => s.trim().replace(/^["']|["']$/g, ""))
      .filter(Boolean);
    for (const lbl of formLabels) {
      assert.ok(
        validLabelNames.has(lbl),
        `${file} references label '${lbl}' which is missing from scripts/setup-github-labels.sh`
      );
    }
  }

  // Verify bug and hardware templates explicitly warn against serial numbers / service tags:
  const bugYml = fs.readFileSync(
    path.join(templateDir, "01-bug-report.yml"),
    "utf8"
  );
  const hwYml = fs.readFileSync(
    path.join(templateDir, "02-hardware-compatibility.yml"),
    "utf8"
  );
  assert.match(bugYml, /Never include your machine serial number/i);
  assert.match(hwYml, /Do \*\*not\*\* include your machine serial number/i);
}

async function testCompleteReportsReceiveNoNeedlessQuestions() {
  const completeBugBody = `### Affected area

Thermal / Fans / G-Mode / TCC (area: thermal)

### Installation format

Debian / Ubuntu package (.deb)

### Application version (\`awcc-cli --version\` or \`awcc-gui --version\`)

awcc-cli 0.1.0 (d4c4012, clean)
package-format: deb

### Expected vs. actual behavior

Expected: \`awcc-cli g\` engages G-Mode.
Actual: Returns \`RolledBack\` when WMAX 0x15 returns 0xFFFFFFFF.

### Steps to reproduce

1. Run \`awcc-cli g\`
2. Observe stderr rollback notice.

### Regression / last working version

No / first install

### Diagnostic evidence availability

Attached awcc-support.zip (or pasted \`awcc-cli diagnostics summary\` below)

### Support report (\`awcc-support.zip\`), diagnostic summary, or terminal output

[awcc-support.zip](https://github.com/user-attachments/files/12345/awcc-support.zip)`;

  const { state, github, makeContext } = createMockGitHub({
    initialLabels: ["bug"],
  });
  const decision = await runIssueIntake({
    github,
    context: makeContext("opened", { body: completeBugBody }),
  });

  assert.equal(decision.formType, "bug");
  assert.deepEqual(decision.missingFields, []);
  assert.ok(state.labels.has("bug"));
  assert.ok(state.labels.has("needs-triage"));
  assert.ok(state.labels.has("area: thermal"));
  assert.ok(!state.labels.has("needs-info"));
  assert.equal(
    state.comments.length,
    0,
    "Complete bug report must receive zero bot comments"
  );

  // Complete hardware compatibility report:
  const completeHwBody = `### Device model name (\`product_name\`)

Alienware m16 R1

### Installation format & version

Arch Linux / CachyOS package (.pkg.tar.zst)

### Which features work or fail?

- Profile match (\`awcc-cli device-info\`): works
- Thermal mode queries & switching: works
- G-Mode / GameShift toggle (\`awcc-cli g\`): works

### \`awcc-cli device-info\` output or \`awcc-support.zip\` attachment

Device Name     : Alienware m16 R1
ACPI Prefix     : AMWW (\\_SB.AMWW.WMAX)
Matched in DB   : true

### BIOS version & kernel/ACPI context (optional — include only if relevant)

_No response_`;

  const hwMock = createMockGitHub();
  const hwDecision = await runIssueIntake({
    github: hwMock.github,
    context: hwMock.makeContext("opened", { body: completeHwBody }),
  });
  assert.equal(hwDecision.formType, "hardware");
  assert.deepEqual(hwDecision.missingFields, []);
  assert.ok(hwMock.state.labels.has("area: hardware"));
  assert.ok(hwMock.state.labels.has("needs-triage"));
  assert.equal(hwMock.state.comments.length, 0);

  // Complete feature request:
  const completeFeatureBody = `### Related area

GUI (awcc-gui)

### Problem or use case

I want a keyboard shortcut in awcc-gui to copy the diagnostic summary.

### Desired behavior

Pressing Ctrl+Shift+C on the Log tab copies the diagnostic summary.`;

  const featMock = createMockGitHub();
  const featDecision = await runIssueIntake({
    github: featMock.github,
    context: featMock.makeContext("opened", { body: completeFeatureBody }),
  });
  assert.equal(featDecision.formType, "feature");
  assert.deepEqual(featDecision.missingFields, []);
  assert.ok(featMock.state.labels.has("enhancement"));
  assert.ok(featMock.state.labels.has("area: gui"));
  assert.ok(featMock.state.labels.has("needs-triage"));
  assert.equal(featMock.state.comments.length, 0);
}

async function testStartupFailureWithoutSupportExportIsValid() {
  const startupFailureBody = `### Affected area

GUI (awcc-gui)

### Installation format

AppImage (.AppImage)

### Application version (\`awcc-cli --version\` or \`awcc-gui --version\`)

awcc-gui 0.1.0 (d4c4012, clean)
package-format: appimage

### Expected vs. actual behavior

Expected: awcc-gui opens on Wayland compositor.
Actual: Process exits before the window opens.

### Steps to reproduce

1. Run \`./awcc-rs-0.1.0-x86_64.AppImage -v\`

### Diagnostic evidence availability

Application startup / launch failure — diagnostic export unavailable (terminal output pasted below)

### Support report (\`awcc-support.zip\`), diagnostic summary, or terminal output

\`awcc-support.zip\` could not be exported because \`awcc-gui\` exited immediately on startup:
\`\`\`
[INFO] awcc::startup [startup]: starting awcc-gui 0.1.0
error: failed to initialize Wayland display surface
\`\`\``;

  const { state, github, makeContext } = createMockGitHub();
  const decision = await runIssueIntake({
    github,
    context: makeContext("opened", { body: startupFailureBody }),
  });

  assert.equal(decision.formType, "bug");
  assert.deepEqual(
    decision.missingFields,
    [],
    "Startup failure with terminal output must be accepted as complete without requiring awcc-support.zip"
  );
  assert.ok(state.labels.has("bug"));
  assert.ok(state.labels.has("area: gui"));
  assert.ok(state.labels.has("needs-triage"));
  assert.ok(!state.labels.has("needs-info"));
  assert.equal(state.comments.length, 0);
}

async function testMissingInformationFollowedByEditAndRepeatedEvents() {
  const incompleteBugBody = `### Affected area

Lighting / USB ELC / Stealth (area: lighting)

### Installation format

Fedora / RPM package (.rpm)

### Application version (\`awcc-cli --version\` or \`awcc-gui --version\`)

_No response_

### Expected vs. actual behavior

Expected: ...
Actual: ...

### Steps to reproduce

1. Run \`...\`
2. Click \`...\`
3. Observe \`...\`

### Diagnostic evidence availability

Attached awcc-support.zip (or pasted \`awcc-cli diagnostics summary\` below)

### Support report (\`awcc-support.zip\`), diagnostic summary, or terminal output

_No response_`;

  const { state, github, makeContext } = createMockGitHub();

  // 1. Initial open with missing version, expected/actual, steps, and diagnostics:
  const d1 = await runIssueIntake({
    github,
    context: makeContext("opened", { body: incompleteBugBody }),
  });
  assert.equal(d1.missingFields.length, 4);
  assert.ok(state.labels.has("bug"));
  assert.ok(state.labels.has("area: lighting"));
  assert.ok(state.labels.has("needs-triage"));
  assert.ok(state.labels.has("needs-info"));
  assert.equal(state.comments.length, 1);
  assert.equal(state.calls.createComment.length, 1);
  assert.ok(state.comments[0].body.includes(BOT_COMMENT_MARKER));
  assert.ok(state.comments[0].body.includes("**Application version**"));
  assert.ok(state.comments[0].body.includes("**Diagnostic evidence**"));

  // 2. Repeated/duplicate webhook delivery with identical payload must be a no-op:
  const d2 = await runIssueIntake({
    github,
    context: makeContext("opened", { body: incompleteBugBody }),
  });
  assert.equal(d2.commentAction, null);
  assert.equal(
    state.comments.length,
    1,
    "Repeated event must not create a duplicate comment"
  );
  assert.equal(
    state.calls.updateComment.length,
    0,
    "Repeated event with unchanged missing fields must not issue a redundant updateComment"
  );

  // 3. Reporter edits the issue to supply all missing details:
  const completedBugBody = `### Affected area

Lighting / USB ELC / Stealth (area: lighting)

### Installation format

Fedora / RPM package (.rpm)

### Application version (\`awcc-cli --version\` or \`awcc-gui --version\`)

awcc-cli 0.1.0 (d4c4012, clean)
package-format: rpm

### Expected vs. actual behavior

Expected: Setting keyboard morph applies both colors.
Actual: Status shows ACCEPTED with 4 USB reports sent.

### Steps to reproduce

1. Run \`awcc-cli --sim lb 50\`

### Diagnostic evidence availability

Attached awcc-support.zip (or pasted \`awcc-cli diagnostics summary\` below)

### Support report (\`awcc-support.zip\`), diagnostic summary, or terminal output

AWCC Diagnostic Summary (schema_version: 1, session=840c22b6aa98)`;

  const d3 = await runIssueIntake({
    github,
    context: makeContext("edited", { body: completedBugBody }),
  });
  assert.deepEqual(d3.missingFields, []);
  assert.ok(
    !state.labels.has("needs-info"),
    "needs-info must be removed once missing details are supplied"
  );
  assert.ok(state.labels.has("needs-triage"));
  assert.equal(
    state.comments.length,
    1,
    "Existing bot comment must be updated in place, not duplicated"
  );
  assert.equal(state.calls.updateComment.length, 1);
  assert.ok(
    state.comments[0].body.includes("`needs-info` has been removed"),
    "Updated bot comment must acknowledge completion"
  );
}

async function testMaintainerTriagedAndClosedIssuesAreRespected() {
  const incompleteBugBody = `### Affected area

Thermal / Fans / G-Mode / TCC (area: thermal)

### Installation format

_No response_

### Application version (\`awcc-cli --version\` or \`awcc-gui --version\`)

_No response_

### Expected vs. actual behavior

_No response_

### Steps to reproduce

_No response_

### Diagnostic evidence availability

Attached awcc-support.zip (or pasted \`awcc-cli diagnostics summary\` below)

### Support report (\`awcc-support.zip\`), diagnostic summary, or terminal output

_No response_`;

  // Case A: Issue already has `confirmed` from a maintainer and maintainer removed `needs-triage`
  // and `area: thermal` (reclassifying to `area: hardware`).
  const { state, github, makeContext } = createMockGitHub({
    initialLabels: ["bug", "confirmed", "area: hardware"],
    initialEvents: [
      {
        event: "unlabeled",
        label: { name: "needs-triage" },
        actor: { login: "Hoggormino", type: "User" },
      },
      {
        event: "unlabeled",
        label: { name: "area: thermal" },
        actor: { login: "Hoggormino", type: "User" },
      },
    ],
  });

  const decision = await runIssueIntake({
    github,
    context: makeContext("edited", { body: incompleteBugBody }),
  });

  assert.equal(decision.maintainerTriaged, true);
  assert.deepEqual(decision.labelsToAdd, []);
  assert.deepEqual(decision.labelsToRemove, []);
  assert.equal(decision.commentAction, null);
  assert.ok(!state.labels.has("needs-triage"));
  assert.ok(!state.labels.has("needs-info"));
  assert.ok(!state.labels.has("area: thermal"));
  assert.ok(state.labels.has("confirmed"));
  assert.ok(state.labels.has("area: hardware"));
  assert.equal(state.comments.length, 0);

  // Case B: Closed issue is ignored completely:
  const closedMock = createMockGitHub();
  const closedDecision = await runIssueIntake({
    github: closedMock.github,
    context: closedMock.makeContext("edited", {
      state: "closed",
      body: incompleteBugBody,
    }),
  });
  assert.equal(closedDecision.skipped, true);
  assert.equal(closedMock.state.calls.addLabels.length, 0);
  assert.equal(closedMock.state.comments.length, 0);
}

async function testUserForgedBotMarkerIsNeverTrustedOrEdited() {
  const forgedComment = {
    id: 555,
    body: `${BOT_COMMENT_MARKER}\nI am a regular user pretending to be the intake bot!`,
    user: { login: "clever-reporter", type: "User" },
  };
  assert.equal(
    isVerifiedBotComment(forgedComment),
    false,
    "Forged marker from a User account must not be treated as a verified bot comment"
  );

  const incompleteFeatureBody = `### Related area

GUI (awcc-gui)

### Problem or use case

_No response_

### Desired behavior

_No response_`;

  const { state, github, makeContext } = createMockGitHub({
    initialComments: [forgedComment],
  });

  await runIssueIntake({
    github,
    context: makeContext("opened", { body: incompleteFeatureBody }),
  });

  assert.equal(
    state.comments.length,
    2,
    "Bot must leave the user's comment untouched and create its own verified bot comment"
  );
  assert.equal(
    state.comments[0].body,
    forgedComment.body,
    "User comment with forged marker must never be modified"
  );
  assert.equal(state.comments[1].user.login, "github-actions[bot]");
  assert.equal(state.calls.updateComment.length, 0);
  assert.equal(state.calls.createComment.length, 1);
}

async function testAdversarialShellSyntaxQuotesAndNewlinesAreTreatedAsData() {
  const sentinelPath = `/tmp/awcc-intake-shell-injection-sentinel-${process.pid}`;
  try {
    fs.unlinkSync(sentinelPath);
  } catch {
    // ignore
  }

  const adversarialBody = [
    "### Affected area\r\n",
    "Thermal / Fans / G-Mode / TCC (area: thermal)\r\n\r\n",
    "### Installation format\r\n",
    "Binary archive (.tar.gz)\r\n\r\n",
    "### Application version (`awcc-cli --version` or `awcc-gui --version`)\r\n",
    `awcc-cli 0.1.0 "; $(touch ${sentinelPath}) \`touch ${sentinelPath}\` '"\r\n\r\n`,
    "### Expected vs. actual behavior\r\n",
    `Expected: safe handling of "$HOME", '\\n', \`id\`, $(uname -a), and \\_SB.AMWW.WMAX\r\nActual: safe!\r\n\r\n`,
    "### Steps to reproduce\r\n",
    "1. Run `echo \"hello 'world' $(false)\"`\r\n\r\n",
    "### Diagnostic evidence availability\r\n",
    "Attached awcc-support.zip (or pasted `awcc-cli diagnostics summary` below)\r\n\r\n",
    "### Support report (`awcc-support.zip`), diagnostic summary, or terminal output\r\n",
    "AWCC Diagnostic Summary (`schema_version: 1`)",
  ].join("");

  const { state, github, makeContext } = createMockGitHub();
  const decision = await runIssueIntake({
    github,
    context: makeContext("opened", {
      title: `[Bug]: "$(touch ${sentinelPath})" 'single' \`backticks\``,
      body: adversarialBody,
    }),
  });

  assert.equal(
    fs.existsSync(sentinelPath),
    false,
    "Adversarial shell syntax in issue title/body must never be executed"
  );
  assert.equal(decision.formType, "bug");
  assert.deepEqual(decision.missingFields, []);
  assert.ok(state.labels.has("area: thermal"));
  assert.ok(state.labels.has("needs-triage"));
  assert.equal(state.comments.length, 0);
}

async function testPaginatedBotCommentDiscovery() {
  const userComments = Array.from({ length: 105 }, (_, i) => ({
    id: i + 1,
    body: `User discussion comment #${i + 1}`,
    user: { login: `user-${i + 1}`, type: "User" },
  }));
  const botCommentOnPage2 = {
    id: 9001,
    body: `${BOT_COMMENT_MARKER}\nOutdated missing-info comment`,
    user: { login: "github-actions[bot]", type: "Bot" },
  };

  const incompleteFeatureBody = `### Related area

GUI (awcc-gui)

### Problem or use case

_No response_

### Desired behavior

_No response_`;

  const { state, github, makeContext } = createMockGitHub({
    initialComments: [...userComments, botCommentOnPage2],
  });

  await runIssueIntake({
    github,
    context: makeContext("edited", { body: incompleteFeatureBody }),
  });

  assert.equal(
    state.calls.listComments,
    2,
    "Must paginate across comment pages to locate existing bot comment"
  );
  assert.equal(
    state.calls.createComment.length,
    0,
    "Must not create a duplicate bot comment when one exists on page 2"
  );
  assert.equal(
    state.calls.updateComment.length,
    1,
    "Must update the existing bot comment on page 2"
  );
  assert.equal(state.calls.updateComment[0].comment_id, 9001);
}

function testDocumentationAndInvestigationRunbook() {
  for (const relPath of [
    "SUPPORT.md",
    "SECURITY.md",
    "CONTRIBUTING.md",
    ".github/pull_request_template.md",
    "docs/agent_issue_investigation_workflow.md",
  ]) {
    const full = path.join(REPO_ROOT, relPath);
    assert.ok(fs.existsSync(full), `Missing required document: ${relPath}`);
    const text = fs.readFileSync(full, "utf8");
    assert.ok(
      !text.includes("/home/hoggormino"),
      `${relPath} must not contain private local filesystem paths`
    );
    assert.ok(
      !text.includes("src-rust-priv"),
      `${relPath} must not reference private sibling repositories`
    );
  }

  const securityMd = fs.readFileSync(
    path.join(REPO_ROOT, "SECURITY.md"),
    "utf8"
  );
  assert.ok(
    securityMd.includes(
      "https://github.com/Hoggormino/awcc-rs/security/advisories/new"
    )
  );
  assert.ok(
    securityMd.includes("private-vulnerability-reporting"),
    "SECURITY.md must document the required repository setting"
  );

  const supportMd = fs.readFileSync(path.join(REPO_ROOT, "SUPPORT.md"), "utf8");
  assert.ok(
    supportMd.includes("awcc-cli diagnostics export --output awcc-support.zip")
  );

  const runbookMd = fs.readFileSync(
    path.join(REPO_ROOT, "docs/agent_issue_investigation_workflow.md"),
    "utf8"
  );
  assert.ok(runbookMd.includes("Hoggormino/awcc-rs#<ISSUE_NUMBER>"));
  assert.ok(runbookMd.includes("needs-hardware-test"));
}

async function main() {
  testWorkflowSyncAndPermissions();
  testIssueTemplatesAndLabelCatalog();
  testDocumentationAndInvestigationRunbook();
  await testCompleteReportsReceiveNoNeedlessQuestions();
  await testStartupFailureWithoutSupportExportIsValid();
  await testMissingInformationFollowedByEditAndRepeatedEvents();
  await testMaintainerTriagedAndClosedIssuesAreRespected();
  await testUserForgedBotMarkerIsNeverTrustedOrEdited();
  await testAdversarialShellSyntaxQuotesAndNewlinesAreTreatedAsData();
  await testPaginatedBotCommentDiscovery();

  // Also sanity-check evaluateIssueIntake directly on a pull_request payload:
  const prDecision = evaluateIssueIntake({
    payload: {
      action: "opened",
      issue: { number: 99, state: "open", pull_request: {} },
    },
  });
  assert.equal(prDecision.skip, true);
  assert.equal(prDecision.reason, "pull_request");

  console.log(
    "All issue-intake, template, label-catalog, pagination, and security-isolation checks passed!"
  );
}

await main();
