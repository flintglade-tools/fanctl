#!/usr/bin/env bash
# sudo ./install.sh              install or update the nct6687_rs driver, fanctl and services
# sudo ./install.sh --uninstall  remove everything this script installs
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "run with sudo" >&2; exit 1; }
here=$(cd "$(dirname "$0")" && pwd)
lib=/usr/local/lib/nct6687-rs

# Open files pin the driver, so stop the curve service and refuse if anything else holds it.
unload() {
    [[ -d /sys/module/nct6687_rs ]] || return 0
    modprobe -r nct6687_rs || { echo "close every running fanctl (hold, curve, watch) and try again" >&2; exit 1; }
}

if [[ ${1:-} == --uninstall ]]; then
    systemctl disable --now fanctl.service nct6687-rs.service 2>/dev/null || true
    unload
    rm -rf "$lib" /var/cache/nct6687-rs /usr/local/bin/fanctl /etc/udev/rules.d/70-nct6687.rules \
        /etc/systemd/system/nct6687-rs.service /etc/systemd/system/fanctl.service /lib/modules/*/extra/nct6687_rs.ko
    systemctl daemon-reload; depmod -a
    echo "removed (kept /etc/fanctl.conf); the BIOS controls all fans"
    exit 0
fi

[[ $(cat /sys/class/dmi/id/board_name) == "PRO Z690-A WIFI (MS-7D25)" ]] || { echo "unsupported board" >&2; exit 1; }
[[ -x $here/cli/target/release/fanctl ]] || { echo "build first: (cd $here/cli && cargo build --release)" >&2; exit 1; }

install -d -m 0755 "$lib" /var/cache/nct6687-rs
install -m 0644 "$here/kmod/nct6687_rs.rs" "$here/kmod/Kbuild" "$lib/"
install -m 0755 "$here/kmod/build.sh" "$here/kmod/load.sh" "$lib/"
install -m 0755 "$here/cli/target/release/fanctl" /usr/local/bin/fanctl
# Reuse a compiler already extracted next to this script instead of fetching it again.
if [[ ! -d /var/cache/nct6687-rs/toolchain && -d $here/toolchain ]]; then
    cp -a "$here/toolchain" /var/cache/nct6687-rs/toolchain
fi
chown -R root:root "$lib" /var/cache/nct6687-rs

echo 'KERNEL=="nct6687", GROUP="wheel", MODE="0660"' > /etc/udev/rules.d/70-nct6687.rules
udevadm control --reload

cat > /etc/systemd/system/nct6687-rs.service <<UNIT
[Unit]
Description=Build (after kernel updates) and load the nct6687_rs fan driver
Wants=network-online.target
After=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=$lib/load.sh

[Install]
WantedBy=multi-user.target
UNIT

cat > /etc/systemd/system/fanctl.service <<UNIT
[Unit]
Description=Fan curves from /etc/fanctl.conf (fans return to the BIOS curve when stopped)
Requires=nct6687-rs.service
After=nct6687-rs.service

[Service]
ExecStart=/usr/local/bin/fanctl curve /etc/fanctl.conf
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

[[ -f /etc/fanctl.conf ]] || cat > /etc/fanctl.conf <<'CONF'
# Fan curves for fanctl.service. One line per fan: FAN TEMP:PCT ... (CPU temperature in °C).
# Duty is linear between points and flat beyond the ends. Fans without a line keep the BIOS curve.
# CPU_FAN1/PUMP_FAN1 never go below 30 %, and go to 100 % at 85 °C, whatever this file says.
# Enable with: sudo systemctl enable --now fanctl
#
# cpu   40:30  60:45  75:70  85:100
# pump  40:60  70:100
# sys2  40:35  70:60  85:100
CONF

systemctl daemon-reload
curves_were_running=$(systemctl is-active fanctl.service || true)
systemctl stop fanctl.service 2>/dev/null || true
unload
"$lib/load.sh" --rebuild
systemctl enable nct6687-rs.service
systemctl start nct6687-rs.service
udevadm settle
[[ $curves_were_running == active ]] && systemctl start fanctl.service
ls -l /dev/nct6687
/usr/local/bin/fanctl
