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
    time::Duration,
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
        let handle = self
            .control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| error("provider initializing"))?;
        let id = self.sequence.fetch_add(1, Ordering::Relaxed);
        let request = ControlRequest::Op {
            id: format!("dbus-{id}"),
            op,
        };
        let submission = if let Some(g) = desired {
            handle.request_desired(request, Duration::from_secs(12), g)
        } else {
            handle.request(request, Duration::from_secs(12))
        };
        let reply = submission
            .map_err(|e| error(format!("{e:?}")))?
            .ok_or_else(|| error("operation pending; inspect provider state before retrying"))?;
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
) -> fdo::Result<String> {
    let sender = header
        .and_then(|h| h.sender())
        .ok_or_else(|| fdo::Error::AccessDenied("missing bus credentials".into()))?
        .to_string();
    let bus = fdo::DBusProxy::new(connection).await?;
    let name = zbus::names::BusName::try_from(sender.as_str()).map_err(error)?;
    if bus.get_connection_unix_user(name).await? == 0 {
        return Ok(sender);
    }
    let authority = zbus::Proxy::new(
        connection,
        "org.freedesktop.PolicyKit1",
        "/org/freedesktop/PolicyKit1/Authority",
        "org.freedesktop.PolicyKit1.Authority",
    )
    .await
    .map_err(error)?;
    let subject = (
        "system-bus-name",
        HashMap::from([("name", Value::from(sender.as_str()))]),
    );
    // Authorization result details are strings; subject details above are variants.
    let (allowed, _, _): (bool, bool, HashMap<String, String>) = authority
        .call(
            "CheckAuthorization",
            &(subject, action, HashMap::<String, String>::new(), 1_u32, ""),
        )
        .await
        .map_err(|e| fdo::Error::AccessDenied(e.to_string()))?;
    if !allowed {
        return Err(fdo::Error::AccessDenied(
            "profile operation is not authorized".into(),
        ));
    }
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
                authorize(
                    connection,
                    header.as_ref(),
                    "io.github.hoggormino.awcc_rs.switch-profile",
                )
                .await?;
                let profile = PowerProfile::parse(value).map_err(fdo::Error::InvalidArgs)?;
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
                let owner = authorize(
                    connection,
                    Some(&header),
                    "io.github.hoggormino.awcc_rs.hold-profile",
                )
                .await?;
                let profile = PowerProfile::parse(profile).map_err(fdo::Error::InvalidArgs)?;
                let cookie = self.0.sequence.fetch_add(1, Ordering::Relaxed);
                let hold = ProfileHold {
                    cookie,
                    owner,
                    profile,
                    reason: reason.into(),
                    application_id: application_id.into(),
                };
                let bridge = Arc::clone(&self.0);
                blocking::unblock(move || bridge.apply(ControlOp::HoldPower(hold))).await?;
                Ok(cookie)
            }
            async fn release_profile(
                &self,
                cookie: u32,
                #[zbus(header)] header: Header<'_>,
                #[zbus(connection)] connection: &zbus::Connection,
            ) -> fdo::Result<()> {
                let owner = authorize(
                    connection,
                    Some(&header),
                    "io.github.hoggormino.awcc_rs.hold-profile",
                )
                .await?;
                let bridge = Arc::clone(&self.0);
                blocking::unblock(move || bridge.apply(ControlOp::ReleasePower { cookie, owner }))
                    .await
            }
            async fn set_action_enabled(&self, _action: &str, _enabled: bool) -> fdo::Result<()> {
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
        Self::build(zbus::blocking::connection::Builder::system().map_err(|e| e.to_string())?)
    }
    fn build(builder: zbus::blocking::connection::Builder<'_>) -> Result<Self, String> {
        let bridge = Arc::new(Bridge::default());
        let connection = builder
            .serve_at(MODERN.1, Modern(bridge.clone()))
            .map_err(|e| e.to_string())?
            .serve_at(COMPAT.1, Compat(bridge.clone()))
            .map_err(|e| e.to_string())?
            .build()
            .map_err(|e| e.to_string())?;
        for (name, _) in [MODERN, COMPAT] {
            connection
                .request_name_with_flags(name, zbus::fdo::RequestNameFlags::DoNotQueue.into())
                .map_err(|e| format!("cannot own {name}: {e}"))?;
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
                            let _ = connection.emit_signal(
                                None::<&str>,
                                path,
                                "org.freedesktop.DBus.Properties",
                                "PropertiesChanged",
                                &(iface, changed, invalidated),
                            );
                            for (seq, cookie, owner) in &now.releases {
                                if *seq > release_seq {
                                    let _ = connection.emit_signal(
                                        Some(owner.as_str()),
                                        path,
                                        iface,
                                        "ProfileReleased",
                                        &(cookie,),
                                    );
                                }
                            }
                        }
                        release_seq = now.releases.last().map_or(release_seq, |r| r.0);
                        previous = now.clone();
                    }
                    if let Ok(bus) = zbus::blocking::fdo::DBusProxy::new(&connection) {
                        for owner in now
                            .holds
                            .iter()
                            .map(|h| h.owner.clone())
                            .collect::<std::collections::BTreeSet<_>>()
                        {
                            if let Ok(name) = zbus::names::BusName::try_from(owner.as_str())
                                && bus.name_has_owner(name).ok() == Some(false)
                            {
                                let _ = bridge.apply(ControlOp::DisconnectPower(owner));
                            }
                        }
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
        let _ = handle.stop(Duration::from_secs(3));
    }
}
