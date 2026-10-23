# System power profiles

`awccd` is the system PowerProfiles provider. KDE/GNOME remain responsible for
session policy, AC/battery profile choices, suspend, displays and notifications.
There is no TuneD dependency. This replaces the provider, rather than running a
second independent firmware-profile controller next to it.

## Desktop interface and authorization

The system bus exports `org.freedesktop.UPower.PowerProfiles` at
`/org/freedesktop/UPower/PowerProfiles`, and the compatibility name/interface
`net.hadess.PowerProfiles` at `/net/hadess/PowerProfiles`. Both share one state.
See the [public interface contract](https://upower.pages.freedesktop.org/power-profiles-daemon/gdbus-org.freedesktop.UPower.PowerProfiles.html).

The provider claims both names without queueing or replacing an existing owner,
before constructing the hardware owner. It refuses startup when ownership is
unavailable. D-Bus writes authenticate the bus sender through polkit using
AWCC-specific action IDs; a Unix-socket grant is not a D-Bus grant. Active local
users may select/hold profiles; other users require administrator authorization.
Hold release checks the original connection owner, including across endpoints.

Power-saving holds take precedence over performance holds. Releasing the last
hold restores the saved user selection. A different explicit system selection
cancels holds and emits targeted `ProfileReleased` signals. A same-value desktop
selection is a no-op. Owner-disconnect cleanup is checked every 100 ms on the bus
worker. Cookies are process-local and never restored after daemon restart.
Reasons/application IDs and total holds are bounded. Optional hardware actions
are not advertised: `Actions`/`ActionsInfo` are empty, action configuration returns
NotSupported, and BatteryAware is false (automatic selection belongs to the desktop).

The interface surface was checked against KDE PowerDevil 5.27.12 and Plasma 6.8,
GNOME Control Center/Settings Daemon 46, and the current upstream API. Older
clients use the compatibility endpoint; both endpoint/hold paths are exercised on
an isolated real D-Bus daemon. This is protocol verification, not certification
of complete desktop sessions on every distribution.

## Detailed counterparts

| System profile | Allowed saved AWCC counterpart |
| --- | --- |
| Power Save | Quiet, Cool, Manual (and Battery Saver on devices that support it) |
| Balanced | Balanced, Manual |
| Performance | Performance, Full Speed, G-Mode, Manual |

Selecting an AWCC preset selects its associated public profile and remembers the
preset. Manual belongs to the saved user selection, independent of temporary
holds, and stores fixed CPU/GPU fan boosts or the complete fan curve. Each slot
has its own configuration. Changing a counterpart applies even when the system
profile is already active. The Settings pane shows all three counterparts and
separate requested/active profile, CPU outcome and thermal outcome. Thermal
Control edits manual settings and curves. Observing state never submits a change.

Fresh counterparts default to Quiet, Balanced and Performance; a profile is only
advertised when its CPU plan and thermal counterpart are supported. Invalid
mappings are rejected rather than clamped or silently translated. Settings schema
3 migrates schema 1/2 without losing saved curves. An old normal preset seeds its
matching slot. An old active curve seeds Balanced (or the prior preset's slot)
only when the previous startup preference allowed restoration. Curves with
restoration disabled remain saved for explicit application. New mapped curves
are part of the selected policy and resume with that policy; the old standalone
curve-startup switch is hidden while the provider is active.

## CPU ownership and turbo

AWCC owns CPUFreq `scaling_governor` and, for detected active Intel P-State and AMD
P-State EPP drivers, `energy_performance_preference`. Every policy directory is
discovered and planned before writes. Controls/values must exist and be advertised.

* Active EPP drivers use `powersave` to allow EPP control. Power Save chooses
  `power` or `balance_power`; Balanced chooses `balance_performance`,
  `balance_power` or `default`; Performance requires `performance`.
* Passive Intel/AMD and generic CPUFreq drivers use available governors:
  Power Save prefers `powersave`, `conservative`, `schedutil`, then `ondemand`;
  Balanced prefers `schedutil`, `ondemand`, `conservative`, then `powersave`;
  Performance requires `performance`.
* Writes are skipped when the current value matches, and verified by readback.
  Missing interfaces, rejected values and readback failures remain failures.
* Profiles never implicitly enable/disable turbo. An accepted explicit turbo
  toggle is saved and reapplied through the existing Intel/AMD turbo backend on
  startup/resume. If that override cannot be restored the failure is exposed.

There are no P-State mode switches, kernel command-line edits, package power
limits, automatic GPU actions, or hidden ownership of thermald's controls.

## Ordered transitions and failures

Desktop and socket selections run on the existing serialized control worker.
Queued user selections carry desired generations; obsolete queued selections are
rejected and a superseded CPU transition does not start its thermal phase. A
running hardware call completes on its owner thread. There is no rollback across
a newer request. Redundant effective policies are coalesced; reconciliation after
resume deliberately revalidates CPU and thermal state. G-Mode uses an idempotent
ensure-enabled operation and retains the existing latch rollback path.

CPU and thermal results are separate. `ActiveProfile` advances only after CPU
readback succeeds and thermal activation is accepted or confirmed. Missing thermal
readback remains Accepted, never fabricated confirmation. If thermal activation
fails after CPU changes, the previous public ActiveProfile remains published,
the actual thermal readings remain visible, and PerformanceDegraded/status describe
the partial failure. Saved user mappings are retained. The provider does not
attempt to restore old CPU settings over a newer intent. Temporary holds and
reconciliation never persist derived selections over the user's settings.

## Installation, recovery and removal

Native packages provide `power-profiles-daemon` compatibility and conflict with
other desktop providers. They ship a `powerprofilesctl` wrapper supporting get,
set, list, list-holds and launch; no Python GObject dependency is needed for it.
Package-manager dependency resolution governs removal/reinstallation of packages.
AWCC does not reinstall an old provider package automatically on removal.

All formats use the same root-side provider handover journal in
`/var/lib/awcc/provider-handover.json`. Known competing providers/CPU writers are
inspected before takeover. Administrator unit overrides cause an explicit refusal.
Previously unmasked competing units are masked/stopped; masks persist across
reboot and block their D-Bus activation. A known provider whose package unit file
has been removed is still stopped if systemd reports it running or transitioning.
The original executable and enablement are retained for AppImage/manual handover.
Native packages ship vendor activation files; setup adds higher-priority
`/usr/local/share/dbus-1/system-services`
registration pointing to `awccd.service`, plus bus/polkit policy as necessary.
The unit's pre-start check also covers deferred/offline package installations.

The journal is written before mutations. Failed handover attempts restore recorded
changes. Installation health failure stops the new daemon before provider recovery.
Removal restores only recorded masks and starts only providers that were running
before handover, when the recorded state is still applicable and the provider's
unit file is still installed. Removing a mask does not reinstall a removed package.
Hash checks preserve changed registration files and require administrator review
instead of overwriting them. An interrupted journal is recovered on the next setup
attempt. Administrators
can retry `awcc-cli --provider-handover install` or `remove` as root after resolving
reported conflicts. Never delete the journal to bypass recovery checks.

Native setup still authorizes the installing account and starts the service; an
AppImage/tarball uses the existing verified payload and elevated installer. Failed
handover is not reported as a ready provider. Hardware startup requires the matching
`acpi_call` module and usable CPUFreq controls. The live laptop is not automatically
modified by development tests.

## Diagnosing desktop integration

The Log screen's temporary DEBUG recording covers both GUI and daemon for five minutes.
Reproduce the profile change, then use Export Support Report. Default service events already
retain failures, so an authorization or startup error can be reported without enabling DEBUG.
CLI users can use `awcc-cli diagnostics record --seconds 60`, reproduce the problem, and run
`awcc-cli diagnostics export --output awcc-support.zip`; `diagnostics stop-recording` ends
recording early. These diagnostic operations do not increase hardware polling or select profiles.

The report includes separate daemon events and integration context, with request IDs linking
authorization, CPU/thermal policy and publication. Missing daemon/journal access and export
limits are stated explicitly. See [diagnostic retention and export](structured_logging_and_support_export.md).

## Verification limits

Tests use simulated thermal transports, temporary CPUFreq trees, a private real
D-Bus daemon with a mock authorization authority, and isolated installer runners.
They cover hold priority/ownership/disconnect, both endpoints, migration,
idempotent G-Mode, same-profile edits, partial failure and handover recovery.
Real Intel/AMD hardware, full KDE/GNOME sessions, suspend cycles and distribution
package-manager replacement transactions still require deployment testing.
