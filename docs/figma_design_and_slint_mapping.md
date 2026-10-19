# Figma Design Specification & Slint Material 3 (`@material`) Mapping

## 1. Design Philosophy & Explicit Non-Goals

- **Task-Oriented Desktop Material 3 Utility**: Designed around the core tasks of an Alienware Linux user—checking thermal/fan health at a glance, switching thermal profiles or G-Mode safely, staging manual fan boost overrides without accidental hardware writes, configuring RGB zones, and inspecting hardware diagnostics.
- **Explicit Non-Goal**: We do **not** clone sprawling vendor marketing layouts, full-screen 3D chassis renders, or fixed-pixel gamer skins.
- **Compact DesktopErgonomics**:
  - **Normal Desktop Viewport**: `1120 × 720 px` (2-column dashboard grid with persistent `NavigationRail`).
  - **Compact Desktop Viewport**: `880 × 620 px` (responsive cards & controls that reflow cleanly without clipping or horizontal scrollbars).
- **Dark-First Material 3 Palette**: Uses `@material` `MaterialPalette.schemes.dark` (`Palette.color-scheme = ColorScheme.dark`) with high-contrast semantic status chips for `Simulated Hardware`, `G-Mode Active`, `Rolled Back`, and `Manual Unsaved Changes`.

---

## 2. Full Application Screen Inventory

| Nav Index | Screen Name | Purpose | Phase Status |
| :--- | :--- | :--- | :--- |
| **`0`** | **Thermal & Fans** | Active thermal profile selector (`Quiet`, `Cool`, `Balanced`, `Performance`, `Full Speed`, `Manual`), one-click **G-Mode (`GameShift`)** card with two-step rollback status, live CPU/GPU telemetry (`Temp °C`, `Fan RPM`, `Boost %`), staged Manual Fan Boost sliders (`Apply` / `Reset`), CPU Turbo toggle, and Simulation Scenario bar. | **Complete & Interactive in Phase 1 Prototype** (`awcc-1ln.1`) |
| **`1`** | **Lighting (AlienFX)** | USB HID (`187c:0550`/`0551`) 4-zone & per-key lighting selector (`Static`, `Spectrum`, `Breathe`, `Rainbow`, `Wave`, `BackAndForth`), brightness slider (`0–10`), and zone chips (`[0, 1, 2, 3]`). | **Interactive Preview Shell in Phase 1**; full USB HID backend in Phase 3 (`awcc-1ln.3`) |
| **`2`** | **Log** | Live hardware/simulation command log (proving **Zero Unsolicited Hardware Writes** on startup/refresh), resolved `database.json` source & bitmasks, and ACPI prefix (`\_SB.AMWW.WMAX`). | **Complete & Interactive in Phase 1 Prototype** (`awcc-1ln.1`) |

---

## 3. Live Figma Design File Hierarchy & Node IDs (`Page 1` `#0:1`)

Synced into [`https://www.figma.com/design/tdXCkrkcTWuswSRAJVI2xM/AWCC`](https://www.figma.com/design/tdXCkrkcTWuswSRAJVI2xM/AWCC) (`fileKey: "tdXCkrkcTWuswSRAJVI2xM"`) via the official `figma` MCP server (`use_figma` full Plugin API):

### Color Variable Collection: `AWCC Material 3 Dark` (`VariableCollectionId:1:18`, Mode `1:1`)
All 21 semantic Material 3 dark tokens are bound with explicit `scopes` (`FRAME_FILL`, `SHAPE_FILL`, `TEXT_FILL`, `STROKE_COLOR`) and `WEB` code syntax (`var(--md-sys-color-...)`):
- `surface` (`VariableID:1:19`): `#111318`
- `surface-container-low` (`VariableID:1:20`): `#1A1B20`
- `surface-container-high` (`VariableID:1:21`): `#282A2F`
- `surface-container-highest` (`VariableID:1:22`): `#33353A`
- `primary` (`VariableID:1:23`): `#ADC6FF`
- `on-primary` (`VariableID:6:2`): `#112F60`
- `primary-container` (`VariableID:1:24`): `#2B4678`
- `on-primary-container` (`VariableID:6:3`): `#D8E2FF`
- `secondary` (`VariableID:6:4`): `#BFC6DC`
- `secondary-container` (`VariableID:1:25`): `#3F4759`
- `on-secondary-container` (`VariableID:6:5`): `#DBE2F9`
- `tertiary` (`VariableID:6:6`): `#DEBCDF`
- `tertiary-container` (`VariableID:1:26`): `#583E5B`
- `on-tertiary-container` (`VariableID:6:7`): `#FCD7FB`
- `error` (`VariableID:6:8`): `#FFB4AB`
- `error-container` (`VariableID:1:27`): `#93000A`
- `on-error-container` (`VariableID:6:9`): `#FFDAD6`
- `on-surface` (`VariableID:6:10`): `#E2E2E9`
- `on-surface-variant` (`VariableID:6:11`): `#C4C6D0`
- `outline` (`VariableID:6:12`): `#8E9099`
- `outline-variant` (`VariableID:6:13`): `#44474F`

### Reusable Component Bench: `AWCC Material 3 Component Bench` (`#6:14`, `y = -220`)
- **Vector Icon Components** (imported via `figma.createNodeFromSvg()` from `crates/awcc-gui/ui/icons/*.svg`):
  - `Icon / thermals` (`#6:17`, `24×24`)
  - `Icon / lighting` (`#6:20`, `24×24`)
  - `Icon / diagnostics` (`#6:23`, `24×24`)
  - `Icon / refresh` (`#6:26`, `18×18`)
  - `Icon / bolt` (`#6:29`, `18×18`)
- **Material 3 Control Components**:
  - `AWCC / Switch / Off` (`#6:30`, `52×32` pill track + `16×16` circular thumb)
  - `AWCC / Switch / On` (`#6:32`, `52×32` pill track + `24×24` circular thumb)
  - `AWCC / InputChip / Unselected` (`#6:34`, `32px` height, `8px` radius, `HUG` width)
  - `AWCC / InputChip / Selected` (`#6:36`, `32px` height, `8px` radius, `HUG` width, `secondary-container` fill + `primary` outline)

### Desktop Frames on `Page 1` (`#0:1`)
1. **Frame `F-01` (`#1:15`): `F-01 · AWCC Desktop — Normal (1120x720) — Balanced`** (`132` nodes)
   - Exported screenshot: `docs/screenshots/figma-frame-f01-normal.png`
   - Highlights:
     - `NavigationRail + Divider` (`82×720`) with vector icon instances (`#6:17`, `#6:20`, `#6:23`), `56×32` active indicator pill, and `"SOON"` badge on `Lighting`.
     - `Device Header Banner` with `"SIMULATED HW"` and `"Balanced (0xA0)"` semantic badges, `G-Mode Toggle Capsule` containing an instance of `AWCC / Switch / Off` (`#6:30`), and `Refresh` `TonalButton` with `Icon / refresh` (`#6:26`).
     - `Scenario & Lifecycle Bar` with visual `SegmentedButton` pill group (`✓ Normal (Balanced)`) and `"IN SYNC WITH HW"` / `"READY"` badges.
     - `Thermal Profile Card` (`#7:66`–`#7:76`) with 6 `HUG`-sized `AWCC / InputChip` instances (`Quiet (0xA3)`, `✓ Balanced (0xA0)`, `Performance (0xA1)`, `Full Speed (0xA4)`, `G-Mode (0xAB)`, `Custom`).
     - `Telemetry Cards Row` (`#7:78`) expanding (`FILL`) vertically with `CPU Telemetry` (`#7:79`) and `GPU Telemetry` (`#7:101`), each rendering `LinearProgress` bars and visual Material 3 `Slider` tracks (`16px` rounded track + `4×28px` vertical thumb handle).
     - `Apply / Reset Footer Card` anchored flush at the bottom with `Reset` and `Apply Changes` (`Icon / bolt` `#6:29`).
2. **Frame `F-02` (`#1:16`): `F-02 · AWCC Desktop — Compact (880x620) — Manual Boost Staged`** (`132` nodes)
   - Exported screenshot: `docs/screenshots/figma-frame-f02-compact.png`
   - Highlights:
     - Compact `880×620` responsive layout with zero horizontal/vertical overflow.
     - `✓ Staged Boost` scenario selected, `"UNSAVED CHANGES"` + `"STAGED (+35% / +20%)"` badges, `✓ Custom` chip (`#7:208`), `+35% (Staged)` CPU slider and `+20% (Staged)` GPU slider, and active `primary`-filled `Apply Changes` button.
3. **Frame `F-03` (`#1:17`): `F-03 · AWCC Desktop — G-Mode Failure Rollback & Unavailable Sensors (1120x720)`** (`138` nodes)
   - Exported screenshot: `docs/screenshots/figma-frame-f03-rollback.png`
   - Highlights:
     - `Non-Modal Rollback Error Banner` (`error-container` `#93000A` fill, `error` `#FFB4AB` border, `on-error-container` `#FFDAD6` text) explaining that `WMAX 0xAB` was rejected and rolled back to `Balanced (0xA0)` / `G-Mode OFF`.
     - `GPU Telemetry (Sensor 0x06)` (`#7:371`) with `error` border, `"SENSOR UNAVAILABLE"` badge, `"— N/A"` RPM readout (`0xFFFFFFFF (Unavailable)`), and disabled (`opacity: 0.42`) `GPU Fan Boost (0x33)` slider.
4. **Frame `F-04` (`#13:38`): `F-04 · AWCC Desktop — AlienFX 7-Zone Lighting (1120x720)`** (`99` nodes)
   - Exported screenshot: `docs/screenshots/figma-frame-f04-lighting.png`
   - Highlights:
     - `AlienFX 7-Zone Chassis & 4-Zone Keyboard Map Card` (`#13:159`) showing live per-zone color indicators for `Stadium Top (0x00)`, `Alienhead Logo (0x03)`, `Stadium Bottom (0x01)`, `Power LED (0x02 · 0x5B..60)`, and the 4-zone standard keyboard (`KB Left 0x04`, `KB Center-Left 0x05`, `KB Center-Right 0x06`, `KB Right 0x07`).
     - `Lighting Effect, Tempo, Brightness & Persistence Card` (`#13:192`) with `Target Zones` (`All Zones`, `Keyboard`, `Alienhead Logo`, `Stadium / Tron`), `Effect Mode` (`Static`, `Breathe`, `Spectrum`, `Wave`, `Rainbow`, `Back & Forth`, `Default Blue`), `Color Swatch` presets, `Animation Speed / Duration (0x24 AddAction)` slider (`600 ms`), `ELC Brightness (0x26 SetDim)` slider (`100%`), `Stealth (Go Dark)` toggle, `Volatile Session (0x27 / Slot 0)` vs `Boot Slot 0x0061` persistence toggle, `Program Power LED (0x02 · 0x5B..60)` button, and `Apply Lighting` button.

---

## 4. Figma-to-Slint `@material` Component Mapping

| Figma Component / Pattern | Figma Node IDs | Slint `@material` (`1.1.0`) Component | Source in `ui-libraries/material-1.1.0/` | Usage in `awcc-gui` (`ui/main.slint`) |
| :--- | :--- | :--- | :--- | :--- |
| **App Window Surface** | `#1:15`, `#1:16`, `#1:17`, `#13:38` | `MaterialWindow` | `ui/components/material_window.slint` | Root window (`MainWindow inherits MaterialWindow`) with dark scheme |
| **Left Navigation Bar** | `Navigation Rail` + `#6:17`, `#6:20`, `#6:23` | `NavigationRail` + `NavigationItem` | `ui/components/navigation_rail.slint` | Primary 3-destination rail (`Thermals`, `Lighting`, `Audit`) |
| **Header & Rollback Banner** | `Device Header Banner`, `Non-Modal Rollback Error Banner` | `FilledCard` / `OutlinedCard` + `Badge` | `ui/components/card.slint`, `badge.slint` | Top persistent `SIMULATED HW` bar, G-Mode capsule, and rollback error banner |
| **Thermal Mode Selector** | `#6:34`, `#6:36` (`ProfileChipsRow`) | `InputChip` | `ui/components/chip.slint` | Profile selection chips (`Quiet`, `Cool`, `Balanced`, `Performance`, `Full Speed`, `Manual`, `G-Mode`) |
| **Simulation Scenario Switcher** | `SegmentedButton` (`Scenario & Lifecycle Bar`) | `SegmentedButton` + `SegmentedItem` | `ui/components/segmented_button.slint` | 4-mode fault-injection switcher (`Normal`, `Rollback`, `0xFE`, `No Sensors`) |
| **G-Mode (`GameShift`) Toggle & Refresh** | `#6:30`, `#6:32`, `#6:26` | `Switch` + `TonalButton` | `ui/components/switch.slint`, `tonal_button.slint` | One-click G-Mode toggle (`WMAX 0x25` + `0x15 0xAB`) and `Refresh` button |
| **CPU/GPU Telemetry & TCC Offset** | `#7:79`, `#7:101`, `#7:211`, `#7:233`, `#7:349`, `#7:371` | `ElevatedCard` + `LinearProgressIndicator` + `Slider` | `ui/components/card.slint`, `progress_indicator.slint`, `slider.slint` | Live temperature (`°C`), fan speed (`RPM`), boost (`%`), CPU Turbo switch, and staged CPU TCC Offset (`0–15°C` TjMax cap) |
| **Manual Fan Boost & 4-Point Curve** | `Material 3 Slider` + `Apply / Reset Footer Card` (`#6:29`) | `OutlinedCard` + `Slider` + `FilledButton` + `OutlineButton` | `ui/components/slider.slint`, `filled_button.slint`, `outline_button.slint` | Staged CPU/GPU fan boost (`0–100%`) and 4-point custom temperature-to-fan-boost curve editor (`45/60/75/85°C`) with hysteresis |
| **7-Zone ELC Lighting & Chassis Map** | `#13:159`, `#13:192` (`F-04`) | `FilledCard` + `ElevatedCard` + `InputChip` + `Slider` + `Switch` | `ui/components/card.slint`, `chip.slint`, `slider.slint`, `switch.slint` | Interactive 7-zone chassis & 4-zone keyboard map (`0x00..0x07`), zone group targeting, effect & swatch pickers, tempo (`ms`) & brightness (`%`) sliders, Stealth toggle, and volatile vs boot-slot `0x0061` persistence |

