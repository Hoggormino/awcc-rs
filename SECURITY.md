# Security Policy

## Supported Versions

Security fixes are applied to the latest release series on `main`. Please verify against the newest `vX.Y.Z` release before reporting.

| Version | Supported |
| :--- | :--- |
| Latest `0.1.x` / `main` | Yes |
| Older snapshots | No |

---

## Threat Surface in `awcc-rs`

Because `awcc-rs` interacts with privileged hardware interfaces on Linux, the following areas are treated as security-sensitive:

1. **Privileged Daemon IPC (`/run/awcc/awcc.sock`, `crates/awcc-service/src/daemon.rs`)**:
   - The daemon validates incoming requests against a strict command allowlist (`WMAX` ACPI calls, Intel/AMD turbo sysfs paths, and `hwmon` PWM/boost paths) before execution. Any bypass of the allowlist or shell/argument injection is a critical vulnerability.
   - The socket is mode `0666` (any local user may send requests), the same access model as the legacy daemon; only typed, validated requests are accepted and no shell string ever crosses it. Clients without the service never fall back to a per-call `pkexec` unless `AWCC_ALLOW_PKEXEC=1` is set.
2. **State File Persistence (`/etc/awcc/prev-mode`, `/etc/awcc/brightness`)**:
   - `PosixStateStorage` refuses to read or write through symlinks (`O_NOFOLLOW`) to prevent local symlink-race file overwrite attacks.
3. **Diagnostic Support Bundles & Redaction (`crates/awcc-diagnostics`)**:
   - Support exports (`awcc-support.zip`) must never execute external commands or include unredacted local credentials, tokens, or machine serial numbers.
4. **GitHub Actions & Issue Intake Workflows (`.github/workflows/`)**:
   - Issue intake workflows must never check out untrusted code or interpolate issue titles, bodies, or comments into shell scripts.

---

## Reporting a Vulnerability Privately

**Do not report security vulnerabilities in public GitHub issues, pull requests, or discussions.**

### Primary Route: GitHub Private Vulnerability Reporting

Report vulnerabilities privately through GitHub Security Advisories for this repository:

- **URL**: <https://github.com/Hoggormino/awcc-rs/security/advisories/new>

When submitting a report, please include:
- The affected component (`awcc-service` daemon, `awcc-platform`, `awcc-cli`, `awcc-gui`, packaging, or CI workflow).
- Output of `awcc-cli --version` (or commit SHA).
- Minimal, deterministic reproduction steps or proof-of-concept input.
- Impact assessment (e.g., local privilege escalation, allowlist bypass, symlink write, or data exposure).

### Repository Setting Requirement & Fallback Route

> [!IMPORTANT]
> Adding `SECURITY.md` to the repository does **not** automatically enable GitHub Private Vulnerability Reporting. A repository maintainer must explicitly enable **Private vulnerability reporting** on `Hoggormino/awcc-rs`:
> - **Via GitHub UI**: **Settings → Code security → Private vulnerability reporting → Enable**
> - **Via GitHub CLI**: `gh api -X PUT repos/Hoggormino/awcc-rs/private-vulnerability-reporting`
> - **Verify status**: `gh api repos/Hoggormino/awcc-rs/private-vulnerability-reporting` (must return `{"enabled": true}`)

**If the link above returns a 404 or indicates that private reporting is not yet enabled:**
Open a minimal public issue titled `[Security]: Request to enable Private Vulnerability Reporting` containing **zero vulnerability details or exploit steps** and ask the maintainer to enable the repository setting so you can submit the advisory privately.
