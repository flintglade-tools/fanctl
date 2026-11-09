# fanctl

fanctl is fan control for Linux on one motherboard, the MSI PRO Z690-A WIFI (MS-7D25): a Rust kernel driver for the board's Nuvoton NCT6687D embedded controller, and the `fanctl` command that reads the sensors and sets fan duty through it.

- `kmod/nct6687_rs.rs`: the kernel driver. It creates `/dev/nct6687`.
- `cli/`: the `fanctl` command.
- `kmod/build.sh`, `kmod/load.sh` and `install.sh` build, sign, install and load the driver.

## Hardware safety

This software writes to the fan controller of a running computer. Use it only on the MS-7D25 board; the driver and the installer both refuse to run on any other board. Wrong fan settings can overheat the CPU, the voltage regulators or other parts. The driver enforces these rules whatever a program asks for:

- Manual control lasts only as long as the process that asked for it. When it exits, crashes or is killed, the driver puts every fan it changed back to the exact BIOS state it saved first. An open file also prevents the driver from being unloaded.
- CPU_FAN1 and PUMP_FAN1 never go below 30 %.
- While the CPU reads 85 °C or more, any request for CPU_FAN1 or PUMP_FAN1 is raised to 100 %. `hold` and `curve` repeat their request every 1–2 s, so this applies continuously. Both ignore Ctrl-Z, so they cannot be suspended by accident while holding a fan.
- The driver refuses to load on any board except the PRO Z690-A WIFI (MS-7D25).

Temperature labels other than CPU follow the MSI mapping used by the reference drivers; check them against the BIOS readings on your board before building curves on them.

## Use

```sh
fanctl                     # CPU temperature, clock, any thermal cap, board sensors, every fan header
fanctl watch 2             # refresh every 2 s
fanctl hold sys2=60        # hold SYS_FAN2 at 60 % until Ctrl-C
fanctl hold cpu=100 pump=100
fanctl curve /etc/fanctl.conf
sudo systemctl enable --now fanctl   # run /etc/fanctl.conf curves at every boot
```

Fan names are `cpu`, `pump` and `sys1`..`sys6`, matching CPU_FAN1, PUMP_FAN1 and SYS_FAN1..6. Members of the `wheel` group can use fanctl without sudo.

A curve file has one line per fan, `FAN TEMP:PCT ...`, with the CPU temperature in °C. Duty is linear between points and flat beyond the ends. Fans without a line keep the BIOS curve.

```
cpu   40:30  60:45  75:70  85:100
pump  40:60  70:100
sys2  40:35  70:60  85:100
```

## Requirements

- Fedora with a kernel built with Rust support, and the matching `kernel-devel` package
- Rust 1.85 or newer to build the `fanctl` command
- `curl`, `rpm2cpio` and `cpio` to fetch the compiler for the driver
- Secure Boot: an akmods machine-owner key enrolled in the firmware (`/etc/pki/akmods/private/private_key.priv` and `/etc/pki/akmods/certs/public_key.der`), as Fedora's `akmods` package creates

## Install, update and uninstall

```sh
(cd cli && cargo build --release)
sudo ./install.sh              # also run this after changing the source
sudo ./install.sh --uninstall  # the BIOS controls the fans again
```

The installer:

- signs the driver with the akmods key, so it loads under Secure Boot;
- unloads the read-only `nct6683` driver;
- installs `fanctl` to `/usr/local/bin`, a udev rule for the `wheel` group, `nct6687-rs.service` and `fanctl.service`;
- writes a commented `/etc/fanctl.conf` if none exists.

## Kernel updates

At each boot, `nct6687-rs.service` rebuilds and re-signs the driver if the kernel has changed. Rust kernel modules must be built with the exact compiler that built the kernel. `kmod/build.sh` reads that version from the kernel's config and extracts the matching Fedora `rustc` RPM into a private directory. The download is the unsigned build from Fedora's Koji, over HTTPS, and its RPM signature is not checked. Koji also keeps copies signed with the Fedora key under `data/signed/<key id>/` for released builds. The service waits for `network-online.target`, and curl retries the download five times.

The kernel's Rust API changes between kernel releases. If a new kernel breaks the build, `nct6687-rs.service` fails and the BIOS keeps control of the fans. Check with `journalctl -u nct6687-rs`. Without the driver loaded, `fanctl` cannot read the fans either.

## Development

```sh
cd cli
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

GPL-2.0-or-later. See [LICENSE](LICENSE).

The driver's EC access and fan-configuration handshake translate the `nct6687d` driver, which derives from the upstream `nct6683` driver; both are GPL-2.0-or-later.

- Copyright (C) 2020 Frederic Boltz: [nct6687.c](https://github.com/Fred78290/nct6687d/blob/main/nct6687.c)
- Derived from nct6683 driver, Copyright (C) 2013 Guenter Roeck: [nct6683.c](https://github.com/torvalds/linux/blob/master/drivers/hwmon/nct6683.c)
- Rewritten in Rust as a misc-device driver by Flintglade, 2026.
