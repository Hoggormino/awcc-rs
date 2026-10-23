//! PowerProfiles service. Both namespace endpoints share the same coordinator and hold owners.
use crate::{
    control::{ControlHandle, ControlOp, ControlRequest},
    power::{PowerStatus, ProfileHold},
};
use awcc_core::PowerProfile;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use zbus::{
    fdo,
    message::Header,
    zvariant::{OwnedValue, Str, Value},
};
pub const MODERN: (&str, &str) = (
    "org.freedesktop.UPower.PowerProfiles",
    "/org/freedesktop/UPower/PowerProfiles",
);
pub const COMPAT: (&str, &str) = ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles");
fn error(e: impl std::fmt::Display) -> fdo::Error {
    fdo::Error::Failed(e.to_string())
}
fn text(value: &str) -> OwnedValue {
    Str::from(value.to_owned()).into()
}
#[derive(Default)]
struct Bridge {
    control: Mutex<Option<ControlHandle>>,
    sequence: AtomicU32,
}
impl Bridge {
    fn status(&self) -> PowerStatus {
        self.control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|h| h.published())
            .map(|(v, _)| v.control.power.clone())
            .unwrap_or_default()
    }
    fn apply(&self, op: ControlOp) -> fdo::Result<()> {
        self.apply_desired(op, None)
    }
    fn reserve_desired(&self) -> fdo::Result<u64> {
        self.control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(ControlHandle::reserve_desired)
            .ok_or_else(|| error("provider initializing"))
    }
    fn apply_desired(&self, op: ControlOp, desired: Option<u64>) -> fdo::Result<()> {
        let id = desired.map_or_else(
            || self.new_id("operation"),
            |generation| format!("dbus-intent-{generation}"),
        );
        self.apply_request(op, desired, id)
    }
    fn new_id(&self, kind: &str) -> String {
        format!(
            "dbus-{kind}-{}",
            self.sequence.fetch_add(1, Ordering::Relaxed)
        )
    }
    fn apply_request(&self, op: ControlOp, desired: Option<u64>, id: String) -> fdo::Result<()> {
        let started = Instant::now();
        tracing::info!(target:"awcc_service::power_bus", code="power_request_submitted", request_id=%id, generation=?desired, operation=match &op { ControlOp::SelectPower(_) => "select", ControlOp::HoldPower(_) => "hold", ControlOp::ReleasePower { .. } => "release", ControlOp::DisconnectPower(_) => "disconnect", _ => "control" }, "profile request submitted to control worker");
        let handle = self
            .control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| error("provider initializing"))?;
        let request = ControlRequest::Op { id: id.clone(), op };
        let submission = if let Some(g) = desired {
            handle.request_desired(request, Duration::from_secs(12), g)
        } else {
            handle.request(request, Duration::from_secs(12))
        };
        let reply = submission.map_err(|e| {
            tracing::error!(target:"awcc_service::power_bus", code="power_queue_rejected", request_id=%id, error=?e, "control queue rejected profile request");
            error(format!("{e:?}"))
        })?.ok_or_else(|| {
            tracing::warn!(target:"awcc_service::power_bus", code="power_request_pending", request_id=%id, elapsed_ms=started.elapsed().as_millis() as u64, "request still pending; outcome must be inspected before retrying");
            error("operation pending; inspect provider state before retrying")
        })?;
        tracing::info!(target:"awcc_service::power_bus", code="power_request_completed", request_id=%id, elapsed_ms=started.elapsed().as_millis() as u64, outcome=?reply.result, "control worker returned profile outcome");
        if !reply
            .result
            .as_ref()
            .is_ok_and(|lifecycle| !lifecycle.is_error_or_rollback())
        {
            tracing::warn!(target:"awcc_service::power_bus", code="power_request_failed", request_id=%id, outcome=?reply.result, "profile request failed or rolled back");
        }
        match reply.result {
            Ok(lifecycle) if !lifecycle.is_error_or_rollback() => Ok(()),
            Ok(lifecycle) => Err(error(lifecycle.detail_message())),
            Err(e) => Err(match e.code {
                "access_denied" => fdo::Error::AccessDenied(e.message),
                "invalid_hold" | "invalid_policy" => fdo::Error::InvalidArgs(e.message),
                _ => error(e.message),
            }),
        }
    }
}
async fn authorize(
    connection: &zbus::Connection,
    header: Option<&Header<'_>>,
    action: &str,
    request_id: &str,
    endpoint: &str,
) -> fdo::Result<String> {
    let started = Instant::now();
    let sender = header.and_then(|header| header.sender()).ok_or_else(|| {
        tracing::error!(target:"awcc_service::power_bus", code="power_credentials_missing", request_id, endpoint, "bus request has no authenticated sender");
        fdo::Error::AccessDenied("missing bus credentials".into())
    })?.to_string();
    tracing::debug!(target:"awcc_service::power_bus", code="power_authorization_started", request_id, endpoint, sender, action, "checking profile authorization");
    let bus = fdo::DBusProxy::new(connection).await.map_err(|e| { tracing::error!(target:"awcc_service::power_bus", code="power_credentials_failed", request_id, endpoint, error=%e, "cannot create bus credentials proxy"); e })?;
    let name = zbus::names::BusName::try_from(sender.as_str()).map_err(error)?;
    let uid = bus.get_connection_unix_user(name).await.map_err(|e| {
        tracing::error!(target:"awcc_service::power_bus", code="power_credentials_failed", request_id, endpoint, error=%e, "cannot obtain bus sender credentials");
        e
    })?;
    if uid == 0 {
        tracing::info!(target:"awcc_service::power_bus", code="power_authorization_allowed", request_id, endpoint, authority="root_credentials", elapsed_ms=started.elapsed().as_millis() as u64, "profile request authorized");
        return Ok(sender);
    }
    let authority = zbus::Proxy::new(
        connection,
        "org.freedesktop.PolicyKit1",
        "/org/freedesktop/PolicyKit1/Authority",
        "org.freedesktop.PolicyKit1.Authority",
    )
    .await
    .map_err(|e| { tracing::error!(target:"awcc_service::power_bus", code="polkit_proxy_failed", request_id, endpoint, error=%e, "cannot contact authorization service"); error(e) })?;
    let subject = (
        "system-bus-name",
        HashMap::from([("name", Value::from(sender.as_str()))]),
    );
    let reply = authority.call_method("CheckAuthorization", &(subject, action, HashMap::<String,String>::new(), 1_u32, "")).await.map_err(|e| {
        tracing::error!(target:"awcc_service::power_bus", code="polkit_call_failed", request_id, endpoint, error=%e, "authorization service call failed");
        error(e)
    })?;
    let body = reply.body();
    let received = body.signature().to_string();
    let (allowed, challenge, _): (bool,bool,HashMap<String,String>) = body.deserialize().map_err(|e| {
        tracing::error!(target:"awcc_service::power_bus", code="polkit_reply_decode_failed", request_id, endpoint, expected_signature="(bba{ss})", received_signature=%received, error=%e, "authorization reply could not be decoded");
        error(e)
    })?;
    if !allowed {
        tracing::warn!(target:"awcc_service::power_bus", code="power_authorization_denied", request_id, endpoint, challenge, elapsed_ms=started.elapsed().as_millis() as u64, "authorization policy denied the profile request");
        return Err(fdo::Error::AccessDenied(
            "profile operation is not authorized".into(),
        ));
    }
    tracing::info!(target:"awcc_service::power_bus", code="power_authorization_allowed", request_id, endpoint, authority="polkit", elapsed_ms=started.elapsed().as_millis() as u64, "profile request authorized");
    Ok(sender)
}

struct Modern(Arc<Bridge>);
struct Compat(Arc<Bridge>);
macro_rules! endpoint {
    ($type:ident, $name:literal) => {
        #[zbus::interface(name = $name)]
        impl $type {
            #[zbus(property)]
            fn active_profile(&self) -> fdo::Result<String> {
                self.0
                    .status()
                    .active
                    .map(|p| p.as_str().to_string())
                    .ok_or_else(|| {
                        error("provider initializing; no active policy has been confirmed")
                    })
            }
            #[zbus(property)]
            async fn set_active_profile(
                &mut self,
                value: &str,
                #[zbus(header)] header: Option<Header<'_>>,
                #[zbus(connection)] connection: &zbus::Connection,
            ) -> fdo::Result<()> {
                let generation = self.0.reserve_desired()?;
                let request_id = format!("dbus-intent-{generation}");
                tracing::info!(target:"awcc_service::power_bus", code="power_request_received", request_id=%request_id, endpoint=$name, member="ActiveProfile.Set", requested=%crate::diagnostics::short(value), "desktop profile selection received");
                authorize(
                    connection,
                    header.as_ref(),
                    "io.github.hoggormino.awcc_rs.switch-profile",
                    &request_id, $name,
                )
                .await?;
                let profile = PowerProfile::parse(value).map_err(|reason| { tracing::warn!(target:"awcc_service::power_bus", code="power_profile_invalid", request_id=%request_id, endpoint=$name, requested=%crate::diagnostics::short(value), error=%reason, "unsupported public profile name"); fdo::Error::InvalidArgs(reason) })?;
                let bridge = Arc::clone(&self.0);
                blocking::unblock(move || {
                    bridge.apply_desired(ControlOp::SelectPower(profile), Some(generation))
                })
                .await
            }
            #[zbus(property)]
            fn profiles(&self) -> Vec<HashMap<String, OwnedValue>> {
                self.0
                    .status()
                    .available
                    .into_iter()
                    .map(|p| {
                        HashMap::from([
                            ("Profile".into(), text(p.as_str())),
                            ("Driver".into(), text("awcc")),
                            ("CpuDriver".into(), text("cpufreq")),
                            ("PlatformDriver".into(), text("awcc")),
                        ])
                    })
                    .collect()
            }
            #[zbus(property)]
            fn performance_degraded(&self) -> String {
                self.0.status().problem.unwrap_or_default()
            }
            #[zbus(property)]
            fn performance_inhibited(&self) -> String {
                String::new()
            }
            #[zbus(property)]
            fn version(&self) -> &str {
                concat!("awcc-", env!("CARGO_PKG_VERSION"))
            }
            #[zbus(property)]
            fn battery_aware(&self) -> bool {
                false
            }
            #[zbus(property)]
            fn set_battery_aware(&mut self, enabled: bool) -> fdo::Result<()> {
                if enabled {
                    tracing::warn!(target:"awcc_service::power_bus", code="power_optional_property_unsupported", endpoint=$name, property="BatteryAware", "desktop requested unsupported optional provider behavior");
                    Err(fdo::Error::NotSupported(
                        "AC/battery selection belongs to the desktop".into(),
                    ))
                } else {
                    Ok(())
                }
            }
            #[zbus(property)]
            fn actions(&self) -> Vec<String> {
                vec![]
            }
            #[zbus(property)]
            fn actions_info(&self) -> Vec<HashMap<String, OwnedValue>> {
                vec![]
            }
            #[zbus(property)]
            fn active_profile_holds(&self) -> Vec<HashMap<String, OwnedValue>> {
                self.0
                    .status()
                    .holds
                    .iter()
                    .map(|h| {
                        HashMap::from([
                            ("Profile".into(), text(h.profile.as_str())),
                            ("Reason".into(), text(&h.reason)),
                            ("ApplicationId".into(), text(&h.application_id)),
                        ])
                    })
                    .collect()
            }
            async fn hold_profile(
                &self,
                profile: &str,
                reason: &str,
                application_id: &str,
                #[zbus(header)] header: Header<'_>,
                #[zbus(connection)] connection: &zbus::Connection,
            ) -> fdo::Result<u32> {
                let request_id = self.0.new_id("hold");
                tracing::info!(target:"awcc_service::power_bus", code="power_hold_received", request_id=%request_id, endpoint=$name, requested=%crate::diagnostics::short(profile), "temporary profile hold received");
                let owner = authorize(
                    connection,
                    Some(&header),
                    "io.github.hoggormino.awcc_rs.hold-profile",
                    &request_id, $name,
                )
                .await?;
                let profile = PowerProfile::parse(profile).map_err(|reason| { tracing::warn!(target:"awcc_service::power_bus", code="power_profile_invalid", request_id=%request_id, endpoint=$name, error=%crate::diagnostics::short(&reason), "unsupported hold profile name"); fdo::Error::InvalidArgs(reason) })?;
                let cookie = self.0.sequence.fetch_add(1, Ordering::Relaxed);
                let hold = ProfileHold {
                    cookie,
                    owner,
                    profile,
                    reason: reason.into(),
                    application_id: application_id.into(),
                };
                let bridge = Arc::clone(&self.0);
                blocking::unblock(move || bridge.apply_request(ControlOp::HoldPower(hold), None, request_id)).await?;
                Ok(cookie)
            }
            async fn release_profile(
                &self,
                cookie: u32,
                #[zbus(header)] header: Header<'_>,
                #[zbus(connection)] connection: &zbus::Connection,
            ) -> fdo::Result<()> {
                let request_id = self.0.new_id("release");
                tracing::info!(target:"awcc_service::power_bus", code="power_hold_release_received", request_id=%request_id, endpoint=$name, cookie, "profile hold release received");
                let owner = authorize(
                    connection,
                    Some(&header),
                    "io.github.hoggormino.awcc_rs.hold-profile",
                    &request_id, $name,
                )
                .await?;
                let bridge = Arc::clone(&self.0);
                blocking::unblock(move || bridge.apply_request(ControlOp::ReleasePower { cookie, owner }, None, request_id))
                    .await
            }
            async fn set_action_enabled(&self, _action: &str, _enabled: bool) -> fdo::Result<()> {
                tracing::warn!(target:"awcc_service::power_bus", code="power_optional_action_unsupported", endpoint=$name, "desktop requested an unsupported optional action");
                Err(fdo::Error::NotSupported(
                    "no optional actions are implemented".into(),
                ))
            }
            #[allow(dead_code)]
            #[zbus(signal)]
            async fn profile_released(
                emitter: &zbus::object_server::SignalEmitter<'_>,
                cookie: u32,
            ) -> zbus::Result<()>;
        }
    };
}
endpoint!(Modern, "org.freedesktop.UPower.PowerProfiles");
endpoint!(Compat, "net.hadess.PowerProfiles");

pub struct PowerBus {
    connection: zbus::blocking::Connection,
    bridge: Arc<Bridge>,
    stop: Arc<AtomicBool>,
}
impl PowerBus {
    /// Claim both names without replacing or queueing behind another provider. Call before any
    /// hardware factory or startup restore. No thermal writes occur when ownership is refused.
    pub fn system() -> Result<Self, String> {
        Self::build(zbus::blocking::connection::Builder::system().map_err(|e| { tracing::error!(target:"awcc_service::power_bus", code="power_bus_connect_failed", error=%e, "cannot connect to system bus"); e.to_string() })?)
    }
    fn build(builder: zbus::blocking::connection::Builder<'_>) -> Result<Self, String> {
        let bridge = Arc::new(Bridge::default());
        let connection = builder
            .serve_at(MODERN.1, Modern(bridge.clone()))
            .map_err(|e| e.to_string())?
            .serve_at(COMPAT.1, Compat(bridge.clone()))
            .map_err(|e| e.to_string())?
            .build()
            .map_err(|e| { tracing::error!(target:"awcc_service::power_bus", code="power_bus_connect_failed", error=%e, "cannot connect and register provider endpoints"); e.to_string() })?;
        tracing::info!(target:"awcc_service::power_bus", code="power_bus_connected", "connected to system profile bus");
        for (name, _) in [MODERN, COMPAT] {
            connection
                .request_name_with_flags(name, zbus::fdo::RequestNameFlags::DoNotQueue.into())
                .map_err(|e| {
                    let bus = zbus::blocking::fdo::DBusProxy::new(&connection).ok();
                    let owner = bus.and_then(|bus| zbus::names::BusName::try_from(name).ok().and_then(|name| bus.get_name_owner(name).ok())).map(|name| name.to_string());
                    let competing_pid = owner.as_deref().and_then(|owner| zbus::blocking::fdo::DBusProxy::new(&connection).ok().and_then(|bus| zbus::names::BusName::try_from(owner).ok().and_then(|name| bus.get_connection_unix_process_id(name).ok())));
                    tracing::error!(target:"awcc_service::power_bus", code="power_bus_name_failed", endpoint=name, competing_owner=?owner, competing_pid, error=%e, "cannot acquire provider name; hardware owner will not start");
                    format!("cannot own {name}: {e}")
                })?;
            tracing::info!(target:"awcc_service::power_bus", code="power_bus_name_acquired", endpoint=name, "provider name acquired");
        }
        Ok(Self {
            connection,
            bridge,
            stop: Arc::new(AtomicBool::new(false)),
        })
    }
    pub fn attach(&self, control: ControlHandle) -> Result<(), String> {
        let initial = control
            .request(
                ControlRequest::Op {
                    id: "provider-bootstrap".into(),
                    op: ControlOp::EnablePower,
                },
                Duration::from_secs(12),
            )
            .map_err(|e| format!("{e:?}"))?
            .ok_or("provider bootstrap timed out")?;
        match initial.result {
            Ok(ref life) if !life.is_error_or_rollback() => {}
            Ok(life) => return Err(life.detail_message()),
            Err(e) => return Err(e.message),
        }
        tracing::info!(target:"awcc_service::power_bus", code="power_provider_ready", active=?initial.view.control.power.active, available=?initial.view.control.power.available, "PowerProfiles provider initialized");
        *self
            .bridge
            .control
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(control);
        let (connection, bridge, stop) = (
            self.connection.clone(),
            self.bridge.clone(),
            self.stop.clone(),
        );
        std::thread::Builder::new()
            .name("awcc-power-events".into())
            .spawn(move || {
                let mut previous = PowerStatus::default();
                let mut release_seq = 0;
                while !stop.load(Ordering::Acquire) {
                    let now = bridge.status();
                    if now != previous {
                        for (iface, path) in [MODERN, COMPAT] {
                            let mut changed = HashMap::from([
                                (
                                    "ActiveProfile",
                                    text(now.active.unwrap_or(PowerProfile::Balanced).as_str()),
                                ),
                                (
                                    "PerformanceDegraded",
                                    text(now.problem.as_deref().unwrap_or("")),
                                ),
                            ]);
                            let holds: Vec<HashMap<String, OwnedValue>> = now
                                .holds
                                .iter()
                                .map(|h| {
                                    HashMap::from([
                                        ("Profile".into(), text(h.profile.as_str())),
                                        ("Reason".into(), text(&h.reason)),
                                        ("ApplicationId".into(), text(&h.application_id)),
                                    ])
                                })
                                .collect();
                            let profiles: Vec<HashMap<String, OwnedValue>> = now
                                .available
                                .iter()
                                .map(|p| {
                                    HashMap::from([
                                        ("Profile".into(), text(p.as_str())),
                                        ("Driver".into(), text("awcc")),
                                        ("CpuDriver".into(), text("cpufreq")),
                                        ("PlatformDriver".into(), text("awcc")),
                                    ])
                                })
                                .collect();
                            if let Ok(value) = OwnedValue::try_from(Value::new(holds)) {
                                changed.insert("ActiveProfileHolds", value);
                            }
                            if let Ok(value) = OwnedValue::try_from(Value::new(profiles)) {
                                changed.insert("Profiles", value);
                            }
                            let invalidated: Vec<&str> = vec![];
                            let emitted = connection.emit_signal(
                                None::<&str>,
                                path,
                                "org.freedesktop.DBus.Properties",
                                "PropertiesChanged",
                                &(iface, changed, invalidated),
                            );
                            match emitted {
                                Ok(()) => tracing::info!(target:"awcc_service::power_bus", code="power_properties_published", endpoint=iface, generation=now.generation, active=?now.active, "profile properties notification sent"),
                                Err(e) => tracing::error!(target:"awcc_service::power_bus", code="power_properties_publish_failed", endpoint=iface, generation=now.generation, error=%e, "profile properties notification failed"),
                            }
                            for (seq, cookie, owner) in &now.releases {
                                if *seq > release_seq
                                    && let Err(e) = connection.emit_signal(
                                        Some(owner.as_str()),
                                        path,
                                        iface,
                                        "ProfileReleased",
                                        &(cookie,),
                                    ) { tracing::error!(target:"awcc_service::power_bus", code="power_release_publish_failed", endpoint=iface, cookie, error=%e, "hold release notification failed"); }
                            }
                        }
                        release_seq = now.releases.last().map_or(release_seq, |r| r.0);
                        previous = now.clone();
                    }
                    match zbus::blocking::fdo::DBusProxy::new(&connection) {
                        Ok(bus) => {
                            for owner in now.holds.iter().map(|hold| hold.owner.clone()).collect::<std::collections::BTreeSet<_>>() {
                                let name = match zbus::names::BusName::try_from(owner.as_str()) {
                                    Ok(name) => name,
                                    Err(error) => { tracing::warn!(target:"awcc_service::power_bus", code="power_owner_monitor_failed", error=%error, "hold owner name could not be checked"); continue; }
                                };
                                match bus.name_has_owner(name) {
                                    Ok(false) => {
                                        tracing::info!(target:"awcc_service::power_bus", code="power_hold_owner_disconnected", sender=%owner, "releasing disconnected caller holds");
                                        if let Err(error) = bridge.apply(ControlOp::DisconnectPower(owner)) { tracing::error!(target:"awcc_service::power_bus", code="power_hold_cleanup_failed", error=%error, "hold cleanup failed"); }
                                    }
                                    Ok(true) => {}
                                    Err(error) => tracing::warn!(target:"awcc_service::power_bus", code="power_owner_monitor_failed", error=%error, "hold owner connection could not be checked"),
                                }
                            }
                        }
                        Err(error) => tracing::warn!(target:"awcc_service::power_bus", code="power_bus_monitor_failed", error=%error, "provider bus monitoring unavailable"),
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
impl Drop for PowerBus {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        tracing::info!(target:"awcc_service::power_bus", code="power_provider_stopped", "PowerProfiles provider stopping");
    }
}

/// Common powerprofilesctl commands, also usable from a tarball without installing a helper.
pub fn run_cli(args: &[String]) -> Result<u8, String> {
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "help") {
        println!(
            "Usage: powerprofilesctl get | set PROFILE | list | list-holds | launch [--profile PROFILE] [--reason TEXT] [--app-id ID] -- COMMAND...\nAWCC PowerProfiles client"
        );
        return Ok(0);
    }
    if args[0] == "--version" {
        println!("awcc {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    let connection = zbus::blocking::Connection::system().map_err(|e| e.to_string())?;
    let proxy = zbus::blocking::Proxy::new(&connection, MODERN.0, MODERN.1, MODERN.0)
        .map_err(|e| e.to_string())?;
    match args[0].as_str() {
        "get" => println!(
            "{}",
            proxy
                .get_property::<String>("ActiveProfile")
                .map_err(|e| e.to_string())?
        ),
        "set" => {
            let p = args.get(1).ok_or("missing profile")?;
            PowerProfile::parse(p)?;
            proxy
                .set_property("ActiveProfile", p.as_str())
                .map_err(|e| e.to_string())?;
        }
        "list" | "list-holds" => {
            let prop = if args[0] == "list" {
                "Profiles"
            } else {
                "ActiveProfileHolds"
            };
            let profiles = proxy
                .get_property::<Vec<HashMap<String, OwnedValue>>>(prop)
                .map_err(|e| e.to_string())?;
            for p in profiles {
                for (k, v) in p {
                    println!(
                        "{k}: {}",
                        <&str>::try_from(&v).map_or_else(|_| format!("{v:?}"), str::to_owned)
                    );
                }
                println!();
            }
        }
        "launch" => {
            let mut profile = "performance";
            let mut reason = "AWCC command workload";
            let mut app = "io.github.hoggormino.awcc_rs";
            let mut index = 1;
            while index < args.len() && args[index].starts_with("--") && args[index] != "--" {
                let value = args
                    .get(index + 1)
                    .ok_or("missing launch option value")?
                    .as_str();
                match args[index].as_str() {
                    "--profile" => profile = value,
                    "--reason" => reason = value,
                    "--app-id" => app = value,
                    _ => return Err("unknown launch option".into()),
                }
                index += 2;
            }
            if args.get(index).is_some_and(|s| s == "--") {
                index += 1;
            }
            let program = args.get(index).ok_or("missing command after --")?;
            let cookie: u32 = proxy
                .call("HoldProfile", &(profile, reason, app))
                .map_err(|e| e.to_string())?;
            let result = std::process::Command::new(program)
                .args(&args[index + 1..])
                .status();
            let _: Result<(), _> = proxy.call("ReleaseProfile", &(cookie,));
            return result
                .map(|s| s.code().unwrap_or(1).clamp(0, 255) as u8)
                .map_err(|e| e.to_string());
        }
        _ => return Err("unknown powerprofilesctl command; use --help".into()),
    }
    Ok(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{
        ThermalOrchestrator,
        control::{ControlCore, NullSettingsSink},
        settings::SettingsLoad,
    };
    use std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
        time::Instant,
    };
    #[derive(serde::Serialize, zbus::zvariant::Type)]
    struct AuthorizationReply(bool, bool, HashMap<String, String>);
    fn test_diagnostics() -> Arc<awcc_diagnostics::DiagnosticsRuntime> {
        static RUNTIME: std::sync::OnceLock<Arc<awcc_diagnostics::DiagnosticsRuntime>> =
            std::sync::OnceLock::new();
        RUNTIME
            .get_or_init(|| {
                let mut cfg = awcc_diagnostics::DiagnosticsConfig::from_env_and_cli(
                    "provider-test",
                    awcc_diagnostics::resolve_log_level(Some("debug"), false, None).unwrap(),
                    false,
                );
                cfg.enable_file_sink = false;
                cfg.log_dir = None;
                cfg.enable_stderr_console = false;
                cfg.event_ring_capacity = 16384;
                awcc_diagnostics::init_global_diagnostics(cfg)
            })
            .clone()
    }
    #[derive(serde::Serialize, zbus::zvariant::Type)]
    struct WrongAuthorizationReply(bool, bool, HashMap<String, OwnedValue>);
    struct WrongAuthority;
    #[zbus::interface(name = "org.freedesktop.PolicyKit1.Authority")]
    impl WrongAuthority {
        fn check_authorization(
            &self,
            _subject: (&str, HashMap<String, OwnedValue>),
            _action: &str,
            _details: HashMap<String, String>,
            _flags: u32,
            _id: &str,
        ) -> WrongAuthorizationReply {
            WrongAuthorizationReply(true, false, HashMap::new())
        }
    }
    struct Authority(Arc<AtomicBool>);
    #[zbus::interface(name = "org.freedesktop.PolicyKit1.Authority")]
    impl Authority {
        fn check_authorization(
            &self,
            _subject: (&str, HashMap<String, OwnedValue>),
            _action: &str,
            _details: HashMap<String, String>,
            _flags: u32,
            _id: &str,
        ) -> AuthorizationReply {
            AuthorizationReply(
                self.0.load(Ordering::SeqCst),
                false,
                HashMap::from([(
                    "polkit.temporary_authorization_id".into(),
                    "test-grant".into(),
                )]),
            )
        }
    }
    #[test]
    fn private_bus_contract_authorization_holds_and_owner_cleanup() {
        let diagnostics = test_diagnostics();
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        struct Cleanup(std::process::Child);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut address = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim();
        let _cleanup = Cleanup(daemon);
        let allowed = Arc::new(AtomicBool::new(true));
        let _authority = zbus::blocking::connection::Builder::address(address)
            .unwrap()
            .serve_at(
                "/org/freedesktop/PolicyKit1/Authority",
                Authority(allowed.clone()),
            )
            .unwrap()
            .name("org.freedesktop.PolicyKit1")
            .unwrap()
            .build()
            .unwrap();
        let service =
            PowerBus::build(zbus::blocking::connection::Builder::address(address).unwrap())
                .unwrap();
        let core = ControlCore::new(
            ThermalOrchestrator::new_simulated_m15_r6().unwrap(),
            SettingsLoad::Missing,
            Arc::new(NullSettingsSink),
            Duration::from_secs(2),
        );
        let handle = ControlHandle::spawn(core).unwrap();
        service.attach(handle.clone()).unwrap();
        // A second provider cannot silently queue and later acquire the names.
        assert!(
            PowerBus::build(zbus::blocking::connection::Builder::address(address).unwrap())
                .is_err()
        );
        let client = zbus::blocking::connection::Builder::address(address)
            .unwrap()
            .build()
            .unwrap();
        let authority = zbus::blocking::Proxy::new(
            &client,
            "org.freedesktop.PolicyKit1",
            "/org/freedesktop/PolicyKit1/Authority",
            "org.freedesktop.PolicyKit1.Authority",
        )
        .unwrap();
        let reply = authority
            .call_method(
                "CheckAuthorization",
                &(
                    (
                        "system-bus-name",
                        HashMap::from([("name", Value::from(":1.0"))]),
                    ),
                    "io.github.hoggormino.awcc_rs.switch-profile",
                    HashMap::<String, String>::new(),
                    0_u32,
                    "",
                ),
            )
            .unwrap();
        // Polkit returns one structure, with string-valued authorization details.
        assert_eq!(reply.body().signature().to_string(), "(bba{ss})");
        let (is_authorized, is_challenge, details): (bool, bool, HashMap<String, String>) =
            reply.body().deserialize().unwrap();
        assert!(is_authorized && !is_challenge);
        assert_eq!(details["polkit.temporary_authorization_id"], "test-grant");
        drop(authority);
        let proxy = zbus::blocking::Proxy::new(&client, MODERN.0, MODERN.1, MODERN.0).unwrap();
        assert_eq!(
            proxy.get_property::<String>("ActiveProfile").unwrap(),
            "balanced"
        );
        proxy.set_property("ActiveProfile", "power-saver").unwrap();
        let cookie: u32 = proxy
            .call(
                "HoldProfile",
                &("performance", "test workload", "awcc-test"),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            proxy.get_property::<String>("ActiveProfile").unwrap(),
            "performance"
        );
        if !rustix::process::geteuid().is_root() {
            allowed.store(false, Ordering::SeqCst);
            let denied = proxy.set_property("ActiveProfile", "balanced").unwrap_err();
            assert!(
                denied
                    .to_string()
                    .contains("profile operation is not authorized")
            );
            let denied = proxy
                .call::<_, _, u32>(
                    "HoldProfile",
                    &("performance", "denied workload", "awcc-test"),
                )
                .unwrap_err();
            assert!(
                denied
                    .to_string()
                    .contains("profile operation is not authorized")
            );
            assert_eq!(
                service.bridge.status().active,
                Some(PowerProfile::Performance)
            );
            allowed.store(true, Ordering::SeqCst);
        }
        let saver: u32 = proxy
            .call(
                "HoldProfile",
                &("power-saver", "test low battery", "awcc-test"),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            proxy.get_property::<String>("ActiveProfile").unwrap(),
            "power-saver"
        );
        let mut signals = proxy.receive_signal("ProfileReleased").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let value = signals
                .next()
                .and_then(|m| m.body().deserialize::<u32>().ok());
            let _ = tx.send(value);
        });
        proxy.call::<_, _, ()>("ReleaseProfile", &(saver,)).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(3)).unwrap(),
            Some(saver)
        );
        let compat = zbus::blocking::Proxy::new(&client, COMPAT.0, COMPAT.1, COMPAT.0).unwrap();
        compat
            .call::<_, _, ()>("ReleaseProfile", &(cookie,))
            .unwrap();
        let legacy_cookie: u32 = compat
            .call(
                "HoldProfile",
                &("performance", "compatibility test", "awcc-test"),
            )
            .unwrap();
        proxy
            .call::<_, _, ()>("ReleaseProfile", &(legacy_cookie,))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            compat.get_property::<String>("ActiveProfile").unwrap(),
            "power-saver"
        );
        let _: u32 = proxy
            .call(
                "HoldProfile",
                &("performance", "disconnect test", "awcc-test"),
            )
            .unwrap();
        drop(compat);
        drop(proxy);
        drop(client);
        let until = Instant::now() + Duration::from_secs(4);
        while !service.bridge.status().holds.is_empty() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            service.bridge.status().active,
            Some(PowerProfile::PowerSaver)
        );
        let events = diagnostics.events_snapshot().0;
        let request_id = events
            .iter()
            .find(|event| {
                event.code.as_deref() == Some("power_request_received")
                    && event
                        .fields
                        .get("requested")
                        .is_some_and(|value| value == "power-saver")
            })
            .unwrap()
            .fields["request_id"]
            .clone();
        for code in [
            "power_authorization_allowed",
            "power_cpu_completed",
            "power_thermal_completed",
            "power_state_published",
        ] {
            assert!(
                events
                    .iter()
                    .any(|event| event.code.as_deref() == Some(code)
                        && event.fields.get("request_id") == Some(&request_id)),
                "missing {code} for {request_id}"
            );
        }
        let _ = handle.stop(Duration::from_secs(3));
    }

    #[test]
    fn malformed_authorization_reply_is_logged_with_signatures_before_any_policy_change() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let diagnostics = test_diagnostics();
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        struct Cleanup(std::process::Child);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut address = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let address = address.trim();
        let _cleanup = Cleanup(daemon);
        let _authority = zbus::blocking::connection::Builder::address(address)
            .unwrap()
            .serve_at("/org/freedesktop/PolicyKit1/Authority", WrongAuthority)
            .unwrap()
            .name("org.freedesktop.PolicyKit1")
            .unwrap()
            .build()
            .unwrap();
        let service =
            PowerBus::build(zbus::blocking::connection::Builder::address(address).unwrap())
                .unwrap();
        let core = ControlCore::new(
            ThermalOrchestrator::new_simulated_m15_r6().unwrap(),
            SettingsLoad::Missing,
            Arc::new(NullSettingsSink),
            Duration::from_secs(60),
        );
        let handle = ControlHandle::spawn(core).unwrap();
        service.attach(handle.clone()).unwrap();
        let before = service.bridge.status();
        let client = zbus::blocking::connection::Builder::address(address)
            .unwrap()
            .build()
            .unwrap();
        let proxy = zbus::blocking::Proxy::new(&client, MODERN.0, MODERN.1, MODERN.0).unwrap();
        let error = proxy
            .set_property("ActiveProfile", "performance")
            .unwrap_err();
        assert!(error.to_string().contains("Signature mismatch"));
        assert_eq!(service.bridge.status(), before);
        let event = diagnostics
            .events_snapshot()
            .0
            .into_iter()
            .find(|event| event.code.as_deref() == Some("polkit_reply_decode_failed"))
            .unwrap();
        assert_eq!(event.severity, awcc_diagnostics::Severity::Error);
        assert_eq!(event.fields["expected_signature"], "(bba{ss})");
        assert_eq!(event.fields["received_signature"], "(bba{sv})");
        assert!(event.fields.contains_key("request_id"));
        assert!(handle.stop(Duration::from_secs(3)).is_some());
    }
}
