#!/bin/sh
# Shared body embedded in native-package maintainer scripts. Keep POSIX sh compatible.
# Fixed paths: no destination or tool override is accepted from the installer's environment.
AWCC_NATIVE_ROOT=''
AWCC_NATIVE_OWNER=0
AWCC_NATIVE_UNIT=awccd.service
AWCC_NATIVE_VENDOR="${AWCC_NATIVE_ROOT}/usr/lib/systemd/system/awccd.service"
AWCC_NATIVE_AUTH="${AWCC_NATIVE_ROOT}/etc/awcc/authorized-users"
AWCC_NATIVE_STATE="${AWCC_NATIVE_ROOT}/var/lib/awcc"
AWCC_NATIVE_MARKER="${AWCC_NATIVE_STATE}/native-package-initialized"
AWCC_NATIVE_CLI="${AWCC_NATIVE_ROOT}/usr/bin/awcc-cli"

awcc_native_notice() {
    printf 'awcc-rs: %s\n' "$*" >&2
}

awcc_native_owned_mode() {
    # stat emits exactly two numeric fields; both are validated below.
    # shellcheck disable=SC2046
    set -- $(stat -c '%u %a' -- "$1" 2>/dev/null) || return 1
    [ "$#" -eq 2 ] && [ "$1" = "$AWCC_NATIVE_OWNER" ] || return 1
    case "$2" in ''|*[!0-7]*) return 1 ;; esac
    [ "$((0$2 & 022))" -eq 0 ]
}

# Refuse symlinked, non-root-owned or writable ancestors before writes or cleanup.
awcc_native_safe_dir() {
    awcc_dir=$1
    while :; do
        [ ! -L "$awcc_dir" ] && [ -d "$awcc_dir" ] || return 1
        awcc_native_owned_mode "$awcc_dir" || return 1
        [ "$awcc_dir" = "${AWCC_NATIVE_ROOT:-/}" ] && return 0
        awcc_dir=${awcc_dir%/*}
        [ -n "$awcc_dir" ] || awcc_dir=/
    done
}

awcc_native_safe_file() {
    [ ! -L "$1" ] && [ -f "$1" ] || return 1
    awcc_native_safe_dir "${1%/*}" && awcc_native_owned_mode "$1"
}

awcc_native_no_override() {
    # A full unit override (including a mask) belongs to the administrator/AppImage installer.
    # Package lifecycle scripts must not authorize users for, start, or stop that other unit.
    for awcc_override in \
        "${AWCC_NATIVE_ROOT}/etc/systemd/system/awccd.service" \
        "${AWCC_NATIVE_ROOT}/run/systemd/system/awccd.service" \
        "${AWCC_NATIVE_ROOT}/run/systemd/transient/awccd.service" \
        "${AWCC_NATIVE_ROOT}/run/systemd/generator.early/awccd.service" \
        "${AWCC_NATIVE_ROOT}/run/systemd/generator/awccd.service" \
        "${AWCC_NATIVE_ROOT}/run/systemd/generator.late/awccd.service" \
        "${AWCC_NATIVE_ROOT}/usr/local/lib/systemd/system/awccd.service"; do
        if [ -e "$awcc_override" ] || [ -L "$awcc_override" ]; then
            awcc_native_notice "kept administrator-managed or masked unit $awcc_override unchanged."
            return 1
        fi
    done
    if [ -e "${AWCC_NATIVE_STATE}/install-owner.json" ] || [ -L "${AWCC_NATIVE_STATE}/install-owner.json" ]; then
        awcc_native_notice 'kept existing AppImage-managed service data unchanged.'
        return 1
    fi
    return 0
}

awcc_native_is_ours() {
    awcc_native_no_override && awcc_native_safe_file "$AWCC_NATIVE_VENDOR"
}

awcc_native_live_systemd() {
    [ -d "${AWCC_NATIVE_ROOT}/run/systemd/system" ] && command -v systemctl >/dev/null 2>&1
}

awcc_native_selected_unit() {
    awcc_fragment=$(systemctl show --property=FragmentPath --value "$AWCC_NATIVE_UNIT") || return 1
    [ -n "$awcc_fragment" ] || return 1
    [ "$(readlink -f -- "$awcc_fragment")" = "$(readlink -f -- "$AWCC_NATIVE_VENDOR")" ]
}

awcc_native_prepare_state() {
    awcc_native_safe_dir "${AWCC_NATIVE_ROOT}/var/lib" || return 1
    if [ ! -e "$AWCC_NATIVE_STATE" ] && [ ! -L "$AWCC_NATIVE_STATE" ]; then
        mkdir -m 0755 -- "$AWCC_NATIVE_STATE" || return 1
    fi
    awcc_native_safe_dir "$AWCC_NATIVE_STATE"
}

awcc_native_lock() {
    awcc_native_prepare_state || return 1
    awcc_lock="${AWCC_NATIVE_STATE}/.install.lock"
    if [ ! -e "$awcc_lock" ] && [ ! -L "$awcc_lock" ]; then
        (umask 077; set -C; : > "$awcc_lock") || return 1
    fi
    awcc_native_safe_file "$awcc_lock" || return 1
    # Keep the inode after purge: unlinking it while a waiter has it open would permit two
    # independent installers. The descriptor is held until this maintainer process exits.
    exec 9< "$awcc_lock"
    if ! flock -n 9; then
        awcc_native_notice 'another service setup operation is running; retry the package operation after it finishes.'
        return 1
    fi
}

awcc_native_mark_initialized() {
    if [ ! -e "$AWCC_NATIVE_MARKER" ] && [ ! -L "$AWCC_NATIVE_MARKER" ]; then
        (umask 077; set -C; printf '1\n' > "$AWCC_NATIVE_MARKER") || \
            awcc_native_notice 'could not record initial setup; open AWCC and choose Set up service.'
    fi
}

awcc_native_authorize_installer() {
    # Updates/reconfiguration never restore a removed grant. An existing policy, including an
    # intentionally empty file, is also authoritative on reinstallation.
    [ "$1" = fresh ] || return 0
    if [ -e "$AWCC_NATIVE_MARKER" ] || [ -L "$AWCC_NATIVE_MARKER" ] || \
       [ -e "$AWCC_NATIVE_AUTH" ] || [ -L "$AWCC_NATIVE_AUTH" ]; then
        return 0
    fi
    if [ "${PKEXEC_UID+x}" = x ]; then
        awcc_uid=$PKEXEC_UID
    else
        awcc_uid=${SUDO_UID:-}
    fi
    case "$awcc_uid" in ''|*[!0-9]*) awcc_uid= ;; esac
    if [ -n "$awcc_uid" ] && [ "$awcc_uid" -gt 0 ] 2>/dev/null && \
       [ "$awcc_uid" -lt 4294967295 ] 2>/dev/null; then
        # Resolve only a real local account; never guess USER, an active session, or UID 1000.
        awcc_uid=$(awk -F: -v uid="$awcc_uid" '$3 == uid && $3 > 0 { print $3; exit }' \
            "${AWCC_NATIVE_ROOT}/etc/passwd")
    else
        awcc_uid=
    fi
    if [ -z "$awcc_uid" ]; then
        awcc_native_notice 'installer user could not be identified; open AWCC and choose Set up service once.'
        return 0
    fi
    if "$AWCC_NATIVE_CLI" service authorize "$awcc_uid"; then
        awcc_native_notice "authorized the installing user (UID $awcc_uid)."
    else
        awcc_native_notice 'automatic user authorization failed; open AWCC and choose Set up service.'
    fi
}

awcc_native_reload_udev() {
    if [ -d "${AWCC_NATIVE_ROOT}/run/udev" ] && command -v udevadm >/dev/null 2>&1; then
        if udevadm control --reload-rules; then
            # Re-evaluate only supported Alienware USB controllers, never all system devices.
            udevadm trigger --action=change --subsystem-match=usb \
                --attr-match=idVendor=187c --attr-match=idProduct=0550 || true
            udevadm trigger --action=change --subsystem-match=usb \
                --attr-match=idVendor=187c --attr-match=idProduct=0551 || true
        else
            awcc_native_notice 'udev is not available; device access rules will apply at the next boot.'
        fi
    fi
}

awcc_native_load_module() {
    awcc_native_live_systemd || return 0
    [ ! -e "${AWCC_NATIVE_ROOT}/proc/acpi/call" ] || return 0
    # Respect Debian image/container policy before touching the host kernel as well as before
    # starting the service. deb-systemd-invoke performs the authoritative service policy check.
    if [ "$1" = deb ] && [ -x "${AWCC_NATIVE_ROOT}/usr/sbin/policy-rc.d" ]; then
        "${AWCC_NATIVE_ROOT}/usr/sbin/policy-rc.d" "$AWCC_NATIVE_UNIT" start >/dev/null 2>&1 || return 0
    fi
    if ! command -v modprobe >/dev/null 2>&1 || ! modprobe acpi_call; then
        awcc_native_notice 'acpi_call is unavailable for this kernel; install its matching kernel module for hardware control.'
    fi
}

awcc_native_configure() {
    # Arguments: package format (deb|arch|rpm), transaction (fresh|upgrade).
    awcc_native_reload_udev
    if ! awcc_native_is_ours; then
        awcc_native_notice 'automatic service setup was skipped; resolve the existing service ownership first.'
        return 0
    fi
    if ! awcc_native_lock; then
        awcc_native_notice 'could not lock service setup; open AWCC and choose Set up service after installation.'
        return 0
    fi
    if awcc_native_live_systemd; then
        if ! systemctl daemon-reload || ! awcc_native_selected_unit; then
            awcc_native_notice 'systemd selected a different or unavailable unit; automatic setup was skipped.'
            return 0
        fi
    fi
    if awcc_native_safe_dir "${AWCC_NATIVE_ROOT}/etc/awcc"; then
        awcc_native_authorize_installer "$2"
        awcc_native_mark_initialized
    else
        awcc_native_notice 'unsafe service configuration path; automatic authorization was skipped.'
        return 0
    fi

    if [ "$1" = deb ]; then
        if [ "$2" = fresh ]; then
            deb-systemd-helper enable "$AWCC_NATIVE_UNIT" >/dev/null || \
                awcc_native_notice 'could not enable the service; open AWCC and choose Set up service.'
        else
            deb-systemd-helper update-state "$AWCC_NATIVE_UNIT" >/dev/null || true
        fi
    elif command -v systemctl >/dev/null 2>&1 && [ "$2" = fresh ]; then
        systemctl enable "$AWCC_NATIVE_UNIT" || \
            awcc_native_notice 'could not enable the service; open AWCC and choose Set up service.'
    fi

    if ! awcc_native_live_systemd; then
        awcc_native_notice 'systemd is not running here; service startup is deferred until boot.'
        return 0
    fi
    if [ "$2" = fresh ]; then
        awcc_native_load_module "$1"
        awcc_service_action=start
    else
        awcc_service_action=try-restart
    fi
    if [ "$1" = deb ]; then
        deb-systemd-invoke "$awcc_service_action" "$AWCC_NATIVE_UNIT" || \
            awcc_native_notice 'service startup failed; open AWCC and choose Set up service for details.'
    else
        systemctl "$awcc_service_action" "$AWCC_NATIVE_UNIT" || \
            awcc_native_notice 'service startup failed; open AWCC and choose Set up service for details.'
    fi
    return 0
}

awcc_native_remove() {
    if ! awcc_native_is_ours; then
        # Masking an already running unit does not stop it. Keep the mask, but do not
        # remove its executable while that instance is still using the native package.
        for awcc_mask in "${AWCC_NATIVE_ROOT}/etc/systemd/system/awccd.service" \
            "${AWCC_NATIVE_ROOT}/run/systemd/system/awccd.service"; do
            if [ -L "$awcc_mask" ] && [ "$(readlink -f -- "$awcc_mask")" = /dev/null ] && \
               awcc_native_live_systemd && systemctl is-active --quiet "$AWCC_NATIVE_UNIT"; then
                awcc_native_notice 'the masked service is still active; stop awccd.service before removing the package.'
                return 1
            fi
        done
        return 0
    fi
    awcc_native_lock || return 1
    # Called before package files disappear. Never run this for an upgrade's old-package erase.
    if awcc_native_live_systemd; then
        if ! awcc_native_selected_unit; then
            awcc_native_notice 'systemd selected a different unit; refusing to stop it.'
            return 1
        fi
        if [ "$1" = deb ]; then
            deb-systemd-invoke stop "$AWCC_NATIVE_UNIT" || return 1
        else
            systemctl stop "$AWCC_NATIVE_UNIT" || return 1
        fi
        if systemctl is-active --quiet "$AWCC_NATIVE_UNIT"; then
            awcc_native_notice 'service is still active; refusing removal before it has stopped.'
            return 1
        fi
    fi
    if [ "$1" = deb ]; then
        deb-systemd-helper disable "$AWCC_NATIVE_UNIT" >/dev/null || return 1
    elif command -v systemctl >/dev/null 2>&1; then
        systemctl disable "$AWCC_NATIVE_UNIT" || return 1
    fi
    awcc_native_notice 'service removed; settings and authorization are retained in /etc/awcc and /var/lib/awcc.'
    return 0
}

awcc_native_after_remove() {
    awcc_native_reload_udev
    if [ "$1" = removed ]; then
        awcc_native_notice 'USB access already granted to a connected controller may remain until reconnection or reboot; existing administrator permissions are preserved.'
    fi
    if awcc_native_live_systemd; then
        systemctl daemon-reload || true
        if [ "$1" = removed ] && awcc_native_no_override; then
            systemctl reset-failed "$AWCC_NATIVE_UNIT" >/dev/null 2>&1 || true
        fi
    fi
}

awcc_native_remove_file() {
    [ -e "$1" ] || [ -L "$1" ] || return 0
    if awcc_native_safe_file "$1"; then
        rm -f -- "$1"
    else
        awcc_native_notice "kept unsafe or administrator-owned path $1."
    fi
}

awcc_native_purge() {
    awcc_native_no_override || return 0
    if [ -e "$AWCC_NATIVE_VENDOR" ] || [ -L "$AWCC_NATIVE_VENDOR" ]; then
        awcc_native_notice 'kept service data because another native installation still supplies the unit.'
        return 0
    fi
    awcc_native_lock || return 1
    # This runs from Debian postrm, including after the CLI has already been removed.
    # Exact known files only: never recursively delete an administrator's /etc or state tree.
    awcc_native_remove_file "$AWCC_NATIVE_AUTH" || return 1
    for awcc_file in "${AWCC_NATIVE_ROOT}/etc/awcc"/.authorized-users.*.tmp; do
        awcc_native_remove_file "$awcc_file" || return 1
    done
    if awcc_native_safe_dir "$AWCC_NATIVE_STATE"; then
        awcc_native_remove_file "${AWCC_NATIVE_STATE}/settings.json" || return 1
        awcc_native_remove_file "$AWCC_NATIVE_MARKER" || return 1
        for awcc_file in "${AWCC_NATIVE_STATE}"/.settings.json.*.tmp; do
            awcc_native_remove_file "$awcc_file" || return 1
        done
        awcc_logs="${AWCC_NATIVE_STATE}/awcc-rs/logs"
        if awcc_native_safe_dir "$awcc_logs"; then
            for awcc_file in "$awcc_logs"/session-*.jsonl "$awcc_logs"/evidence-*.json "$awcc_logs"/evidence-*.json.tmp; do
                awcc_native_remove_file "$awcc_file" || return 1
            done
            rmdir -- "$awcc_logs" "${AWCC_NATIVE_STATE}/awcc-rs" 2>/dev/null || true
        fi
        rmdir -- "$AWCC_NATIVE_STATE" 2>/dev/null || true
    fi
    if awcc_native_safe_dir "${AWCC_NATIVE_ROOT}/etc/awcc"; then
        rmdir -- "${AWCC_NATIVE_ROOT}/etc/awcc" 2>/dev/null || true
    fi
    deb-systemd-helper purge "$AWCC_NATIVE_UNIT" >/dev/null || return 1
    return 0
}
