# Security

Report vulnerabilities privately through [GitHub private vulnerability reporting](https://github.com/flintglade-tools/fanctl/security/advisories/new), or at [support@flintglade.com](mailto:support@flintglade.com).

Fixes go to the `main` branch. There are no maintained older branches.

`/dev/nct6687` is created mode 0660 for root and the `wheel` group. Any member of `wheel` can read the sensors and set fan duty, within the limits the driver enforces (see the README). The installer runs as root, signs the driver with the akmods key, and downloads the Fedora `rustc` RPM that matches the running kernel over HTTPS from Fedora's Koji.
