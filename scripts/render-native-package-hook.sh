#!/usr/bin/env bash
# Emit a self-contained maintainer script. In particular, postrm cannot source package files
# after dpkg has removed them, and pacman stores .INSTALL independently of installed binaries.
set -euo pipefail
SCRIPT_DIR="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
HOOK="${1:-}"
case "${HOOK}" in
    arch.install | arch.pre-remove | deb.postinst | deb.prerm | deb.postrm | rpm.post | rpm.preun | rpm.postun) ;;
    *) echo "Usage: $0 arch.install|arch.pre-remove|deb.postinst|deb.prerm|deb.postrm|rpm.post|rpm.preun|rpm.postun" >&2; exit 1 ;;
esac
cat "${SCRIPT_DIR}/../packaging/linux/native-service.sh"
printf '\n'
cat "${SCRIPT_DIR}/../packaging/linux/native-hooks/${HOOK}"
