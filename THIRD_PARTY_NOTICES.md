# Third-Party License Notices

`awcc-rs` (`awcc-cli` and `awcc-gui`) is licensed under the **BSD 2-Clause License** (see [`LICENSE`](LICENSE)).

Binary distributions and packages of `awcc-rs` incorporate or link against the following third-party components:

---

## 1. Vendored Slint Material 3 Component Library (`@material` 1.1.0)

- **Location**: `ui-libraries/material-1.1.0/`
- **License**: MIT License

```text
MIT License

Copyright (c) 2026 SixtyFPS GmbH

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

---

## 2. Rust Crate Dependencies

Direct workspace dependencies compiled into `awcc-cli` and/or `awcc-gui`:

| Crate | Version | License |
| :--- | :--- | :--- |
| `anyhow` | `1.0` | `MIT OR Apache-2.0` |
| `i-slint-backend-testing` | `1.18.1` | `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0` |
| `png` | `0.17` | `MIT OR Apache-2.0` |
| `rusb` | `0.9.4` | `MIT` |
| `serde` | `1.0` | `MIT OR Apache-2.0` |
| `serde_json` | `1.0` | `MIT OR Apache-2.0` |
| `slint` | `1.18.1` | `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0` |
| `slint-build` | `1.18.1` | `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0` |
| `thiserror` | `2.0` | `MIT OR Apache-2.0` |

Full transitive crate metadata and versions are recorded in [`Cargo.lock`](Cargo.lock).

---

## 3. Dynamically Linked System Libraries

- **`libusb-1.0` (`libusb-1.0.so.0`)**: Licensed under `LGPL-2.1-or-later`. Dynamically linked by both `awcc-cli` and `awcc-gui`; bundled as an unmodified shared object inside the AppImage (`usr/lib/libusb-1.0.so.0`) and resolved from distribution packages in `.deb`, `.rpm`, and Arch `.pkg.tar.zst` installations.
- **`fontconfig` (`libfontconfig.so.1`)**: Licensed under the Fontconfig MIT-style license. Dynamically linked by `awcc-gui` from the host system.
- **Windowing / Input Libraries (`libxkbcommon`, `libX11`, `libxcb`, `libXcursor`, `libXi`, `libXrender`, `libwayland-client`, `libwayland-egl`, `libEGL`, `libGL`)**: MIT / X11 licenses. Loaded dynamically at runtime when launching interactive GUI sessions on Wayland or X11.
