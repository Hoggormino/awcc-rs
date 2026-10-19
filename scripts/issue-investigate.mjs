// Deterministic preflight, evidence collection, output validation, and optional
// comment publication for the GitHub-hosted read-only issue investigation workflow
// (.github/workflows/issue-investigate.yml) in Hoggormino/awcc-rs.
//
// Security properties:
// - Untrusted issue titles, bodies, comments, and model outputs are treated strictly
//   as data strings and never interpolated into shell source or executed.
// - Explicit invocation only (workflow_dispatch on refs/heads/main in Hoggormino/awcc-rs).
// - Verifies BOTH actor and triggering_actor (for reruns) are non-bot maintainers with
//   write/admin permission or explicit maintainer allowlist membership BEFORE any paid
//   model invocation.
// - Does not download attachments or follow reporter URLs; records attachment notices
//   so missing attachment inspection is flagged for the Phase 3 local workflow.

import fs from "node:fs";
import path from "node:path";

export const EXPECTED_REPOSITORY = "Hoggormino/awcc-rs";
export const EXPECTED_REF = "refs/heads/main";
export const EXPECTED_EVENT_NAME = "workflow_dispatch";
export const EXPLICIT_MAINTAINER_ALLOWLIST = new Set(["Hoggormino"]);
export const ALLOWED_COLLABORATOR_PERMISSIONS = new Set(["admin", "write"]);
export const INVESTIGATION_COMMENT_MARKER =
  "<!-- awcc-rs:issue-investigation:v1 -->";

const BOT_LOGINS = new Set(["github-actions[bot]", "app/github-actions"]);

export const MAX_ISSUE_BODY_CHARS = 16000;
export const MAX_COMMENTS_COLLECTED = 30;
export const MAX_COMMENT_BODY_CHARS = 4000;
export const MAX_COMMENT_PAGES = 5;

export const ALLOWED_REPRODUCTION_STATUSES = new Set([
  "not_attempted_static_analysis_only",
  "insufficient_evidence_to_reproduce",
  "needs_local_simulation_reproduction",
  "needs_physical_hardware_reproduction",
]);

export const ALLOWED_TARGET_CRATES = new Set([
  "awcc-core",
  "awcc-build-info",
  "awcc-diagnostics",
  "awcc-platform",
  "awcc-service",
  "awcc-cli",
  "awcc-gui",
  "scripts",
]);

export const ALLOWED_CONFIDENCE_LEVELS = new Set(["low", "medium", "high"]);

const FALSE_REPRODUCTION_PATTERNS = [
  /\bi\s+(?:ran|executed)\s+cargo\s+test\b/i,
  /\ball\s+tests\s+passed\s+in\s+this\s+run\b/i,
  /\bsuccessfully\s+reproduced\s+at\s+runtime\b/i,
  /\breproduced\s+and\s+verified\s+by\s+running\b/i,
];

export function isBotLogin(login, type = "") {
  const cleanLogin = String(login || "").trim();
  const cleanType = String(type || "").trim();
  if (!cleanLogin) return false;
  return (
    BOT_LOGINS.has(cleanLogin) ||
    cleanLogin.endsWith("[bot]") ||
    cleanType.toLowerCase() === "bot"
  );
}

export function isVerifiedInvestigationBotComment(comment) {
  if (!comment || typeof comment.body !== "string") return false;
  if (!comment.body.includes(INVESTIGATION_COMMENT_MARKER)) return false;
  return isBotLogin(comment.user?.login, comment.user?.type);
}

export function validateIssueNumberInput(rawIssueNumber) {
  const str = String(rawIssueNumber ?? "").trim();
  if (!/^[1-9][0-9]{0,8}$/.test(str)) {
    throw new Error(
      `Invalid issue_number '${str}': must be a positive integer (1..999999999).`
    );
  }
  const num = Number.parseInt(str, 10);
  if (!Number.isSafeInteger(num) || num < 1) {
    throw new Error(`Invalid issue_number '${str}'.`);
  }
  return num;
}

export async function verifySingleActorAuthorization({
  github,
  owner,
  repo,
  username,
  roleLabel,
  allowlist = EXPLICIT_MAINTAINER_ALLOWLIST,
}) {
  const login = String(username || "").trim();
  if (!login) {
    throw new Error(`Missing ${roleLabel} identity.`);
  }
  if (isBotLogin(login)) {
    throw new Error(
      `Unauthorized ${roleLabel} '${login}': bot accounts cannot trigger hosted issue investigation.`
    );
  }

  let apiPermission = null;
  let apiUserType = null;
  if (github?.rest?.repos?.getCollaboratorPermissionLevel) {
    try {
      const resp = await github.rest.repos.getCollaboratorPermissionLevel({
        owner,
        repo,
        username: login,
      });
      apiPermission = String(resp?.data?.permission || "");
      apiUserType = String(resp?.data?.user?.type || "");
    } catch (err) {
      if (err?.status !== 404 && err?.status !== 403) {
        throw err;
      }
    }
  }

  if (isBotLogin(login, apiUserType)) {
    throw new Error(
      `Unauthorized ${roleLabel} '${login}': bot accounts cannot trigger hosted issue investigation.`
    );
  }

  if (ALLOWED_COLLABORATOR_PERMISSIONS.has(apiPermission)) {
    return {
      login,
      authorized: true,
      source: `permission:${apiPermission}`,
    };
  }

  if (allowlist.has(login) && apiPermission !== "read" && apiPermission !== "none") {
    return {
      login,
      authorized: true,
      source: "explicit_allowlist",
    };
  }

  throw new Error(
    `Unauthorized ${roleLabel} '${login}' (effective permission: ${
      apiPermission || "none"
    }). Only repository maintainers with write/admin access may run hosted issue investigation.`
  );
}

export async function verifyInvocationGates({
  github,
  repository,
  ref,
  eventName,
  actor,
  triggeringActor,
  openaiApiKeyPresent,
  rawIssueNumber,
  allowlist = EXPLICIT_MAINTAINER_ALLOWLIST,
}) {
  if (repository !== EXPECTED_REPOSITORY) {
    throw new Error(
      `Unsupported repository '${repository}': workflow is locked to '${EXPECTED_REPOSITORY}'.`
    );
  }
  if (eventName !== EXPECTED_EVENT_NAME) {
    throw new Error(
      `Unsupported event '${eventName}': workflow only supports '${EXPECTED_EVENT_NAME}'.`
    );
  }
  if (ref !== EXPECTED_REF) {
    throw new Error(
      `Unsupported ref '${ref}': hosted investigation must run from trusted default branch '${EXPECTED_REF}'.`
    );
  }
  if (!openaiApiKeyPresent) {
    throw new Error(
      "Missing required secret OPENAI_API_KEY. Configure OPENAI_API_KEY in repository Action secrets before running hosted issue investigation."
    );
  }

  const issueNumber = validateIssueNumberInput(rawIssueNumber);
  const [owner, repo] = repository.split("/");

  const actorAuth = await verifySingleActorAuthorization({
    github,
    owner,
    repo,
    username: actor,
    roleLabel: "actor",
    allowlist,
  });

  const effectiveTriggeringActor = String(triggeringActor || actor || "").trim();
  const triggeringAuth = await verifySingleActorAuthorization({
    github,
    owner,
    repo,
    username: effectiveTriggeringActor,
    roleLabel: "triggering_actor",
    allowlist,
  });

  return {
    owner,
    repo,
    issueNumber,
    actorAuth,
    triggeringAuth,
  };
}

function truncateWithNotice(text, maxChars) {
  const normalized = String(text ?? "").replace(/\r\n/g, "\n");
  if (normalized.length <= maxChars) {
    return {
      text: normalized,
      truncated: false,
      original_length: normalized.length,
      retained_length: normalized.length,
    };
  }
  return {
    text: normalized.slice(0, maxChars),
    truncated: true,
    original_length: normalized.length,
    retained_length: maxChars,
  };
}

export function detectUninspectedAttachments(texts) {
  const notices = [];
  const seenUrls = new Set();
  const urlRegex =
    /https?:\/\/[^\s)>\]"']+(?:\.zip|\.tar\.gz|\.tgz|\.log|\.txt|\.json|\/files\/\d+\/[^\s)>\]"']+|\/user-attachments\/(?:files|assets)\/[^\s)>\]"']+)/gi;

  for (const { source, text } of texts) {
    const content = String(text || "");
    let match;
    while ((match = urlRegex.exec(content)) !== null) {
      const url = match[0];
      if (seenUrls.has(url)) continue;
      seenUrls.add(url);
      notices.push({
        source,
        url,
        policy:
          "not_fetched_in_hosted_mode: inspect locally via docs/agent_issue_investigation_workflow.md if needed",
      });
    }
  }
  return notices;
}

export function extractInlineDiagnostics(texts) {
  const snippets = [];
  const patterns = [
    {
      kind: "awcc_diagnostic_summary",
      regex: /AWCC Diagnostic Summary[\s\S]{0,3500}/i,
    },
    {
      kind: "version_identity",
      regex:
        /awcc-(?:cli|gui)\s+[0-9]+\.[0-9]+\.[0-9]+[^\n]*(?:\n\s*(?:commit|dirty|target|rustc|package-format):[^\n]*){1,6}/i,
    },
    {
      kind: "device_info",
      regex:
        /(?:Device Name|product_name|Matched in DB|ACPI Prefix)[\s\S]{0,1500}/i,
    },
  ];

  for (const { source, text } of texts) {
    const content = String(text || "");
    for (const { kind, regex } of patterns) {
      const match = content.match(regex);
      if (match) {
        snippets.push({
          source,
          kind,
          excerpt: match[0].trim().slice(0, 2000),
        });
      }
    }
  }
  return snippets.slice(0, 10);
}

async function listBoundedIssueComments({
  github,
  owner,
  repo,
  issueNumber,
  maxPages = MAX_COMMENT_PAGES,
}) {
  const allComments = [];
  let hasMorePages = false;
  for (let page = 1; page <= maxPages; page += 1) {
    const res = await github.rest.issues.listComments({
      owner,
      repo,
      issue_number: issueNumber,
      per_page: 100,
      page,
    });
    const pageItems = Array.isArray(res?.data) ? res.data : [];
    allComments.push(...pageItems);
    if (pageItems.length < 100) {
      break;
    }
    if (page === maxPages) {
      hasMorePages = true;
    }
  }
  return { allComments, hasMorePages };
}

export async function collectBoundedIssueEvidence({
  github,
  owner,
  repo,
  issueNumber,
  inspectedRevision,
  collectedAt = new Date().toISOString(),
}) {
  let issueResp;
  try {
    issueResp = await github.rest.issues.get({
      owner,
      repo,
      issue_number: issueNumber,
    });
  } catch (err) {
    if (err?.status === 404) {
      throw new Error(
        `Issue #${issueNumber} does not exist in ${owner}/${repo}.`
      );
    }
    throw err;
  }

  const issue = issueResp?.data;
  if (!issue || typeof issue !== "object") {
    throw new Error(`Empty response when fetching issue #${issueNumber}.`);
  }

  if (issue.pull_request) {
    throw new Error(
      `#${issueNumber} is a pull request, not an issue. Hosted issue investigation only accepts issue numbers.`
    );
  }

  const { allComments, hasMorePages } = await listBoundedIssueComments({
    github,
    owner,
    repo,
    issueNumber,
  });

  // Ignore prior automated intake/investigation bot comments when building investigation evidence.
  const humanOrRelevantComments = allComments.filter(
    (c) =>
      !String(c?.body || "").includes(INVESTIGATION_COMMENT_MARKER) &&
      !String(c?.body || "").includes("<!-- awcc-rs:issue-intake:v1 -->")
  );

  const bodyResult = truncateWithNotice(issue.body || "", MAX_ISSUE_BODY_CHARS);
  const selectedComments = humanOrRelevantComments.slice(
    0,
    MAX_COMMENTS_COLLECTED
  );

  let anyCommentTruncated = false;
  const boundedComments = selectedComments.map((c) => {
    const tr = truncateWithNotice(c.body || "", MAX_COMMENT_BODY_CHARS);
    if (tr.truncated) anyCommentTruncated = true;
    return {
      id: c.id,
      html_url:
        c.html_url ||
        `https://github.com/${owner}/${repo}/issues/${issueNumber}#issuecomment-${c.id}`,
      created_at: c.created_at || null,
      updated_at: c.updated_at || null,
      author_login: String(c.user?.login || "unknown"),
      body: tr.text,
      truncated: tr.truncated,
      original_length: tr.original_length,
    };
  });

  const textsForScan = [
    { source: `issue#${issueNumber}`, text: bodyResult.text },
    ...boundedComments.map((c) => ({
      source: `issue#${issueNumber}#issuecomment-${c.id}`,
      text: c.body,
    })),
  ];

  const attachmentNotices = detectUninspectedAttachments(textsForScan);
  const extractedDiagnostics = extractInlineDiagnostics(textsForScan);

  const labels = (issue.labels || [])
    .map((l) => (typeof l === "string" ? l : l?.name))
    .filter(Boolean);

  return {
    schema_version: 1,
    trust_classification: "untrusted_public_issue_data",
    repository: `${owner}/${repo}`,
    inspected_revision: String(inspectedRevision || "unknown"),
    collected_at_utc: collectedAt,
    issue: {
      number: issue.number,
      title: String(issue.title || "").slice(0, 500),
      state: String(issue.state || "open"),
      html_url:
        issue.html_url ||
        `https://github.com/${owner}/${repo}/issues/${issueNumber}`,
      created_at: issue.created_at || null,
      updated_at: issue.updated_at || null,
      author_login: String(issue.user?.login || "unknown"),
      labels,
      body: bodyResult.text,
    },
    comments: boundedComments,
    extracted_inline_diagnostics: extractedDiagnostics,
    attachment_notices: attachmentNotices,
    truncation: {
      issue_body_truncated: bodyResult.truncated,
      issue_body_original_chars: bodyResult.original_length,
      issue_body_retained_chars: bodyResult.retained_length,
      total_comments_seen: humanOrRelevantComments.length,
      comments_retained: boundedComments.length,
      comments_omitted_count: Math.max(
        0,
        humanOrRelevantComments.length - boundedComments.length
      ),
      any_comment_body_truncated: anyCommentTruncated,
      comment_pagination_capped: hasMorePages,
    },
    not_checked_in_hosted_mode: [
      "External attachments (such as uploaded awcc-support.zip archives) are never downloaded or extracted in the hosted workflow.",
      "External URLs referenced by the reporter are never fetched.",
      "No runtime commands, cargo test runs, or hardware operations are executed in the hosted read-only sandbox.",
    ],
  };
}

function assertNonEmptyString(val, fieldName, minLen, maxLen) {
  if (typeof val !== "string") {
    throw new Error(`Field '${fieldName}' must be a string.`);
  }
  const trimmed = val.trim();
  if (trimmed.length < minLen) {
    throw new Error(
      `Field '${fieldName}' must be at least ${minLen} characters (got ${trimmed.length}).`
    );
  }
  if (val.length > maxLen) {
    throw new Error(
      `Field '${fieldName}' exceeds maximum length of ${maxLen} characters (got ${val.length}).`
    );
  }
}

export function validateInvestigationOutput(rawOutput, expectedIssueNumber = null) {
  let parsed = rawOutput;
  if (typeof rawOutput === "string") {
    try {
      parsed = JSON.parse(rawOutput);
    } catch (err) {
      throw new Error(`Model output is not valid JSON: ${err.message}`);
    }
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error("Model output must be a JSON object.");
  }

  const requiredTopKeys = [
    "summary",
    "affected_versions",
    "evidence",
    "hypotheses",
    "missing_information",
    "reproduction_status",
    "suggested_test",
    "recommended_next_step",
    "requires_hardware",
    "candidate_duplicates",
  ];

  const actualKeys = Object.keys(parsed);
  for (const reqKey of requiredTopKeys) {
    if (!Object.prototype.hasOwnProperty.call(parsed, reqKey)) {
      throw new Error(`Missing required top-level field '${reqKey}'.`);
    }
  }
  for (const key of actualKeys) {
    if (!requiredTopKeys.includes(key)) {
      throw new Error(`Unexpected top-level field '${key}' in model output.`);
    }
  }

  assertNonEmptyString(parsed.summary, "summary", 20, 1200);
  assertNonEmptyString(
    parsed.recommended_next_step,
    "recommended_next_step",
    20,
    1200
  );

  for (const pattern of FALSE_REPRODUCTION_PATTERNS) {
    if (
      pattern.test(parsed.summary) ||
      pattern.test(parsed.recommended_next_step)
    ) {
      throw new Error(
        "Model output falsely claims runtime test execution or reproduction in static hosted mode."
      );
    }
  }

  if (
    !Array.isArray(parsed.affected_versions) ||
    parsed.affected_versions.length < 1 ||
    parsed.affected_versions.length > 10
  ) {
    throw new Error(
      "Field 'affected_versions' must be an array of 1..10 strings."
    );
  }
  for (const [i, v] of parsed.affected_versions.entries()) {
    assertNonEmptyString(v, `affected_versions[${i}]`, 1, 160);
  }

  if (
    !Array.isArray(parsed.evidence) ||
    parsed.evidence.length < 1 ||
    parsed.evidence.length > 15
  ) {
    throw new Error("Field 'evidence' must be an array of 1..15 objects.");
  }
  for (const [i, ev] of parsed.evidence.entries()) {
    if (!ev || typeof ev !== "object" || Array.isArray(ev)) {
      throw new Error(`Field 'evidence[${i}]' must be an object.`);
    }
    const evKeys = Object.keys(ev);
    if (
      evKeys.length !== 2 ||
      !evKeys.includes("source") ||
      !evKeys.includes("observation")
    ) {
      throw new Error(
        `Field 'evidence[${i}]' must contain only 'source' and 'observation'.`
      );
    }
    assertNonEmptyString(ev.source, `evidence[${i}].source`, 3, 240);
    assertNonEmptyString(
      ev.observation,
      `evidence[${i}].observation`,
      10,
      600
    );
  }

  if (
    !Array.isArray(parsed.hypotheses) ||
    parsed.hypotheses.length > 10
  ) {
    throw new Error("Field 'hypotheses' must be an array of 0..10 objects.");
  }
  for (const [i, hyp] of parsed.hypotheses.entries()) {
    if (!hyp || typeof hyp !== "object" || Array.isArray(hyp)) {
      throw new Error(`Field 'hypotheses[${i}]' must be an object.`);
    }
    const hypKeys = Object.keys(hyp);
    if (
      hypKeys.length !== 3 ||
      !hypKeys.includes("hypothesis") ||
      !hypKeys.includes("rationale") ||
      !hypKeys.includes("confidence")
    ) {
      throw new Error(
        `Field 'hypotheses[${i}]' must contain only 'hypothesis', 'rationale', and 'confidence'.`
      );
    }
    assertNonEmptyString(
      hyp.hypothesis,
      `hypotheses[${i}].hypothesis`,
      10,
      500
    );
    assertNonEmptyString(hyp.rationale, `hypotheses[${i}].rationale`, 10, 600);
    if (!ALLOWED_CONFIDENCE_LEVELS.has(hyp.confidence)) {
      throw new Error(
        `Field 'hypotheses[${i}].confidence' must be one of low, medium, high.`
      );
    }
  }

  if (
    !Array.isArray(parsed.missing_information) ||
    parsed.missing_information.length > 15
  ) {
    throw new Error(
      "Field 'missing_information' must be an array of 0..15 strings."
    );
  }
  for (const [i, info] of parsed.missing_information.entries()) {
    assertNonEmptyString(info, `missing_information[${i}]`, 5, 400);
  }

  if (!ALLOWED_REPRODUCTION_STATUSES.has(parsed.reproduction_status)) {
    throw new Error(
      `Invalid reproduction_status '${parsed.reproduction_status}'.`
    );
  }

  const st = parsed.suggested_test;
  if (!st || typeof st !== "object" || Array.isArray(st)) {
    throw new Error("Field 'suggested_test' must be an object.");
  }
  const stKeys = Object.keys(st);
  if (
    stKeys.length !== 3 ||
    !stKeys.includes("target_crate") ||
    !stKeys.includes("test_description") ||
    !stKeys.includes("mock_or_fixture")
  ) {
    throw new Error(
      "Field 'suggested_test' must contain only 'target_crate', 'test_description', and 'mock_or_fixture'."
    );
  }
  if (!ALLOWED_TARGET_CRATES.has(st.target_crate)) {
    throw new Error(
      `Invalid suggested_test.target_crate '${st.target_crate}'.`
    );
  }
  assertNonEmptyString(
    st.test_description,
    "suggested_test.test_description",
    15,
    600
  );
  assertNonEmptyString(
    st.mock_or_fixture,
    "suggested_test.mock_or_fixture",
    5,
    400
  );

  if (typeof parsed.requires_hardware !== "boolean") {
    throw new Error("Field 'requires_hardware' must be a boolean.");
  }

  if (
    !Array.isArray(parsed.candidate_duplicates) ||
    parsed.candidate_duplicates.length > 5
  ) {
    throw new Error(
      "Field 'candidate_duplicates' must be an array of 0..5 objects."
    );
  }
  for (const [i, dup] of parsed.candidate_duplicates.entries()) {
    if (!dup || typeof dup !== "object" || Array.isArray(dup)) {
      throw new Error(`Field 'candidate_duplicates[${i}]' must be an object.`);
    }
    const dupKeys = Object.keys(dup);
    if (
      dupKeys.length !== 3 ||
      !dupKeys.includes("issue_number") ||
      !dupKeys.includes("url") ||
      !dupKeys.includes("reason")
    ) {
      throw new Error(
        `Field 'candidate_duplicates[${i}]' must contain only 'issue_number', 'url', and 'reason'.`
      );
    }
    if (!Number.isInteger(dup.issue_number) || dup.issue_number < 1) {
      throw new Error(
        `Field 'candidate_duplicates[${i}].issue_number' must be a positive integer.`
      );
    }
    if (
      expectedIssueNumber !== null &&
      dup.issue_number === expectedIssueNumber
    ) {
      throw new Error(
        `Field 'candidate_duplicates[${i}]' cannot list the inspected issue #${expectedIssueNumber} as a duplicate of itself.`
      );
    }
    const expectedUrl = `https://github.com/${EXPECTED_REPOSITORY}/issues/${dup.issue_number}`;
    if (dup.url !== expectedUrl) {
      throw new Error(
        `Field 'candidate_duplicates[${i}].url' must equal '${expectedUrl}' (got '${dup.url}').`
      );
    }
    assertNonEmptyString(
      dup.reason,
      `candidate_duplicates[${i}].reason`,
      10,
      400
    );
  }

  return parsed;
}

export function sanitizeMarkdownDataText(raw) {
  return String(raw ?? "")
    .replace(/\r\n/g, "\n")
    .replace(/<!--/g, "&lt;!--")
    .replace(/-->/g, "--&gt;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/`/g, "\\`")
    .replace(/\n+/g, " ")
    .trim();
}

export function renderInvestigationMarkdown({
  report,
  evidenceMeta,
  includeBotMarker = false,
}) {
  const issueNum = evidenceMeta?.issue?.number ?? "unknown";
  const issueUrl =
    evidenceMeta?.issue?.html_url ??
    `https://github.com/${EXPECTED_REPOSITORY}/issues/${issueNum}`;
  const rev = sanitizeMarkdownDataText(
    evidenceMeta?.inspected_revision || "unknown"
  );
  const collectedAt = sanitizeMarkdownDataText(
    evidenceMeta?.collected_at_utc || "unknown"
  );

  const lines = [];
  if (includeBotMarker) {
    lines.push(INVESTIGATION_COMMENT_MARKER);
  }

  lines.push(
    `## Automated Read-Only Issue Investigation (\`#${issueNum}\`)`,
    "",
    `> **Scope & Verification Notice**: This report was generated in a read-only sandbox against [\`${EXPECTED_REPOSITORY}\` @ \`${rev.slice(0, 12)}\`](${issueUrl}) at \`${collectedAt}\`. **Static source inspection is not a runtime reproduction or passed test.**`,
    "",
    "### Summary",
    sanitizeMarkdownDataText(report.summary),
    "",
    "### Metadata & Status",
    `- **Affected Version(s)**: ${report.affected_versions
      .map((v) => `\`${sanitizeMarkdownDataText(v)}\``)
      .join(", ")}`,
    `- **Reproduction Status**: \`${sanitizeMarkdownDataText(
      report.reproduction_status
    )}\``,
    `- **Requires Physical Hardware Validation**: \`${
      report.requires_hardware ? "true" : "false"
    }\``,
    "",
    "### Grounded Source & Issue Evidence"
  );

  for (const ev of report.evidence) {
    lines.push(
      `- **\`${sanitizeMarkdownDataText(ev.source)}\`**: ${sanitizeMarkdownDataText(
        ev.observation
      )}`
    );
  }

  lines.push("", "### Hypotheses");
  if (report.hypotheses.length === 0) {
    lines.push("- None (root cause is directly observable in evidence).");
  } else {
    for (const hyp of report.hypotheses) {
      lines.push(
        `- **[${sanitizeMarkdownDataText(
          hyp.confidence.toUpperCase()
        )}]** ${sanitizeMarkdownDataText(
          hyp.hypothesis
        )} — *Rationale*: ${sanitizeMarkdownDataText(hyp.rationale)}`
      );
    }
  }

  lines.push("", "### Missing Information & Uninspected Evidence");
  const missingItems = [...report.missing_information];
  const attachmentNotices = evidenceMeta?.attachment_notices || [];
  for (const att of attachmentNotices) {
    missingItems.push(
      `Attachment referenced in ${att.source} (${att.url}) was not downloaded in hosted mode; inspect locally via docs/agent_issue_investigation_workflow.md.`
    );
  }
  if (missingItems.length === 0) {
    lines.push("- None identified.");
  } else {
    for (const item of missingItems) {
      lines.push(`- ${sanitizeMarkdownDataText(item)}`);
    }
  }

  lines.push(
    "",
    "### Suggested Offline Regression Test",
    `- **Target Crate**: \`${sanitizeMarkdownDataText(
      report.suggested_test.target_crate
    )}\``,
    `- **Mock / Fixture**: \`${sanitizeMarkdownDataText(
      report.suggested_test.mock_or_fixture
    )}\``,
    `- **Test Description**: ${sanitizeMarkdownDataText(
      report.suggested_test.test_description
    )}`,
    "",
    "### Recommended Next Step",
    sanitizeMarkdownDataText(report.recommended_next_step)
  );

  if (report.candidate_duplicates.length > 0) {
    lines.push("", "### Candidate Duplicate Issues");
    for (const dup of report.candidate_duplicates) {
      lines.push(
        `- [#${dup.issue_number}](${dup.url}): ${sanitizeMarkdownDataText(
          dup.reason
        )}`
      );
    }
  }

  const trunc = evidenceMeta?.truncation || {};
  lines.push(
    "",
    "### What Was & Was Not Checked",
    `- **Checked**: Issue \`#${issueNum}\` body (${
      trunc.issue_body_truncated
        ? `truncated to ${trunc.issue_body_retained_chars}/${trunc.issue_body_original_chars} chars`
        : `${trunc.issue_body_retained_chars ?? 0} chars`
    }), ${trunc.comments_retained ?? 0} comment(s) (omitted: ${
      trunc.comments_omitted_count ?? 0
    }), ${
      (evidenceMeta?.extracted_inline_diagnostics || []).length
    } inline diagnostic block(s), and trusted repository checkout at \`${rev}\`.`,
    "- **Not Checked**: Uploaded file attachments (`awcc-support.zip`), external URLs, live ACPI/USB hardware, and runtime test execution (`cargo test` is run in the local-agent workflow and CI pipeline, not inside this read-only investigation job)."
  );

  return lines.join("\n") + "\n";
}

export async function publishOrUpdateInvestigationComment({
  github,
  owner,
  repo,
  issueNumber,
  report,
  evidenceMeta,
}) {
  const validated = validateInvestigationOutput(report, issueNumber);
  const body = renderInvestigationMarkdown({
    report: validated,
    evidenceMeta,
    includeBotMarker: true,
  });

  const { allComments } = await listBoundedIssueComments({
    github,
    owner,
    repo,
    issueNumber,
  });

  const verifiedComments = allComments.filter(
    isVerifiedInvestigationBotComment
  );
  const existing = verifiedComments.length > 0 ? verifiedComments[0] : null;

  if (existing) {
    if (existing.body === body) {
      return {
        action: "noop",
        commentId: existing.id,
        body,
      };
    }
    await github.rest.issues.updateComment({
      owner,
      repo,
      comment_id: existing.id,
      body,
    });
    return {
      action: "updated",
      commentId: existing.id,
      body,
    };
  }

  const created = await github.rest.issues.createComment({
    owner,
    repo,
    issue_number: issueNumber,
    body,
  });
  return {
    action: "created",
    commentId: created?.data?.id ?? null,
    body,
  };
}

export async function runPreflightStep({
  github,
  env = process.env,
  workDir = process.cwd(),
}) {
  const gates = await verifyInvocationGates({
    github,
    repository: env.GITHUB_REPOSITORY || "",
    ref: env.GITHUB_REF || "",
    eventName: env.GITHUB_EVENT_NAME || "",
    actor: env.GITHUB_ACTOR || "",
    triggeringActor: env.GITHUB_TRIGGERING_ACTOR || env.GITHUB_ACTOR || "",
    openaiApiKeyPresent: Boolean(
      String(env.OPENAI_API_KEY_PRESENT || "").trim() === "true" ||
        String(env.OPENAI_API_KEY || "").trim()
    ),
    rawIssueNumber: env.INPUT_ISSUE_NUMBER || "",
  });

  const evidence = await collectBoundedIssueEvidence({
    github,
    owner: gates.owner,
    repo: gates.repo,
    issueNumber: gates.issueNumber,
    inspectedRevision: env.GITHUB_SHA || "unknown",
  });

  const outDir = path.join(workDir, ".codex-investigation");
  fs.mkdirSync(outDir, { recursive: true });

  const evidencePath = path.join(outDir, "evidence.json");
  fs.writeFileSync(evidencePath, JSON.stringify(evidence, null, 2) + "\n", {
    encoding: "utf8",
    mode: 0o600,
  });

  const promptSourcePath = path.join(
    workDir,
    "docs",
    "prompts",
    "hosted_issue_investigation_prompt.md"
  );
  const promptTemplate = fs.readFileSync(promptSourcePath, "utf8");
  const promptPath = path.join(outDir, "prompt.md");
  fs.writeFileSync(promptPath, promptTemplate, {
    encoding: "utf8",
    mode: 0o600,
  });

  return {
    gates,
    evidence,
    evidencePath,
    promptPath,
  };
}

export function runValidateStep({
  env = process.env,
  workDir = process.cwd(),
}) {
  const outDir = path.join(workDir, ".codex-investigation");
  const artifactDir = path.join(outDir, "artifact");
  fs.mkdirSync(artifactDir, { recursive: true });

  const evidencePath = path.join(outDir, "evidence.json");
  const rawOutputPath = path.join(outDir, "raw-output.json");
  const codexOutcome = String(env.CODEX_OUTCOME || "success").trim();

  let evidenceMeta = null;
  if (fs.existsSync(evidencePath)) {
    evidenceMeta = JSON.parse(fs.readFileSync(evidencePath, "utf8"));
    const metaCopy = {
      schema_version: evidenceMeta.schema_version,
      repository: evidenceMeta.repository,
      inspected_revision: evidenceMeta.inspected_revision,
      collected_at_utc: evidenceMeta.collected_at_utc,
      issue: {
        number: evidenceMeta.issue?.number,
        title: evidenceMeta.issue?.title,
        state: evidenceMeta.issue?.state,
        html_url: evidenceMeta.issue?.html_url,
        labels: evidenceMeta.issue?.labels,
      },
      extracted_inline_diagnostics_count: (
        evidenceMeta.extracted_inline_diagnostics || []
      ).length,
      attachment_notices: evidenceMeta.attachment_notices || [],
      truncation: evidenceMeta.truncation || {},
      not_checked_in_hosted_mode: evidenceMeta.not_checked_in_hosted_mode || [],
    };
    fs.writeFileSync(
      path.join(artifactDir, "evidence-metadata.json"),
      JSON.stringify(metaCopy, null, 2) + "\n",
      "utf8"
    );
  }

  const writeFailureDiagnostic = (reason) => {
    const failureSummary = [
      `## Hosted Issue Investigation Failed (\`#${
        evidenceMeta?.issue?.number ?? env.INPUT_ISSUE_NUMBER ?? "unknown"
      }\`)`,
      "",
      `- **Status**: \`failed\``,
      `- **Codex Step Outcome**: \`${sanitizeMarkdownDataText(codexOutcome)}\``,
      `- **Reason**: ${sanitizeMarkdownDataText(reason)}`,
      "- **Note**: No GitHub issue comment was published or updated.",
      "",
    ].join("\n");
    fs.writeFileSync(
      path.join(artifactDir, "investigation-summary.md"),
      failureSummary,
      "utf8"
    );
    if (env.GITHUB_STEP_SUMMARY) {
      fs.appendFileSync(env.GITHUB_STEP_SUMMARY, failureSummary + "\n", "utf8");
    }
  };

  if (codexOutcome !== "success") {
    const msg = `Model execution step did not succeed (outcome: ${codexOutcome}).`;
    writeFailureDiagnostic(msg);
    throw new Error(msg);
  }

  if (!fs.existsSync(rawOutputPath)) {
    const msg = "Missing model output file (.codex-investigation/raw-output.json).";
    writeFailureDiagnostic(msg);
    throw new Error(msg);
  }

  let validatedReport;
  try {
    const rawContent = fs.readFileSync(rawOutputPath, "utf8");
    validatedReport = validateInvestigationOutput(
      rawContent,
      evidenceMeta?.issue?.number ?? null
    );
  } catch (err) {
    writeFailureDiagnostic(err.message);
    throw err;
  }

  const reportJsonPath = path.join(artifactDir, "investigation-report.json");
  fs.writeFileSync(
    reportJsonPath,
    JSON.stringify(validatedReport, null, 2) + "\n",
    "utf8"
  );

  const summaryMd = renderInvestigationMarkdown({
    report: validatedReport,
    evidenceMeta,
    includeBotMarker: false,
  });

  const summaryMdPath = path.join(artifactDir, "investigation-summary.md");
  fs.writeFileSync(summaryMdPath, summaryMd, "utf8");

  if (env.GITHUB_STEP_SUMMARY) {
    fs.appendFileSync(env.GITHUB_STEP_SUMMARY, summaryMd + "\n", "utf8");
  }

  return {
    validatedReport,
    reportJsonPath,
    summaryMdPath,
    summaryMd,
  };
}
