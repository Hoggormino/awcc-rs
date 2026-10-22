//! Rendering of the installer-managed `awccd.service`.

/// Template shipped in the repository (`packaging/linux/awccd-appimage.service.in`).
const TEMPLATE: &str = include_str!("../../../../packaging/linux/awccd-appimage.service.in");

/// Fixed service root of installer-managed deployments.
pub const SERVICE_ROOT: &str = "/opt/awcc-rs/service";

/// Render the unit text. `has_lib` adds `LD_LIBRARY_PATH` for the release's private `lib/`.
///
/// Production callers always pass [`SERVICE_ROOT`]; other roots exist for `systemd-analyze`
/// verification in tests.
#[must_use]
pub fn render_unit(service_root: &str, has_lib: bool) -> String {
    let library = if has_lib {
        format!("Environment=LD_LIBRARY_PATH={service_root}/current/lib")
    } else {
        String::new()
    };
    let rendered = TEMPLATE
        .replace("@SERVICE_ROOT@", service_root)
        .replace("@LIBRARY_ENVIRONMENT@", &library);
    // Drop the empty line left by an absent library environment.
    let mut out = String::with_capacity(rendered.len());
    for line in rendered.lines() {
        if line.is_empty() && !has_lib && out.ends_with("-d\n") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}
