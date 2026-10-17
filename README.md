# RustyDesktop (`rdsktp`)

Headless RDP server for KDE Plasma (Wayland), written in Rust, as a replacement for xrdp. Built on
[IronRDP](https://github.com/Devolutions/IronRDP) (with a few local patches in `third_party/`). H.264 video over
EGFX, one KWin session per user.

## Features

- **Remote login**: PAM authentication, one real logind/KDE session per user (`login` mode, TLS). A fixed
  virtual KWin session (`session = kwin`) and a synthetic test desktop (`test-mode`) are also available.
- **Video**: H.264 (software openh264 or GPU VA-API), bitrate and frames in flight adapted to the client's link.
- **Display**: dynamic resize (Display Control) and up to 16 monitors.
- **Input and clipboard**: keyboard, mouse, cursor shape; clipboard with text, images and files (both ways).
- **Audio** output, **drive** redirection (FUSE) and **printer** redirection (CUPS, raw or XPS jobs).
- **Login throttling** per client address and per account.

Tested with Windows `mstsc`, FreeRDP (`xfreerdp`, `sdl-freerdp`) and Remmina.

## Install

Packages for **x86_64** and **aarch64** are attached to each [GitHub release](../../releases):

| Format | Targets |
|---|---|
| `.deb` | Debian 13 |
| `.rpm` | Fedora, Rocky Linux / RHEL 9 and 10 |
| `.tar.gz` | any of the above (`./install.sh`, `./uninstall.sh`) |

Run-time dependencies: `fuse3`, `libopenh264`, `libkscreen` (`kscreen-doctor`); recommended: `cups`,
`ghostscript` (printers). Then:

```
systemctl enable --now rdsktp-authd rdsktp
```

and connect an RDP client to port 3389 with a Linux account and password.

## Configuration

`/etc/rdsktp/rdsktp.conf` (fully commented) holds `key = value` lines; the keys are the long options of `rdsktp`
without the `--` (`rdsktp --help`). Precedence, highest first:

1. command-line options
2. drop-ins `/etc/rdsktp/rdsktp.conf.d/*.conf` (alphabetical order)
3. `/etc/rdsktp/rdsktp.conf`
4. built-in defaults

An unknown key stops the server at start-up. `--config FILE` uses another file. The file is kept on upgrades. The
NLA password is only read from `RDSKTP_PASSWORD`. The default mode is `login`.

## Build and test

```
cargo build --release                 # add --features kwin for the real KWin session (libpipewire-dev, libclang)
cargo test --workspace
cargo xtask test --server|--client|--freerdp|--clients|--integration
```

Integration tests use a Debian 13 VM (`bash vm/vm.sh create`), a Windows 11 VM for `mstsc` (`bash vm/win11.sh install`)
and containers for FreeRDP/Remmina. Packages: `ci/package-container.sh deb|rpm|tar OUT`, then
`ci/package-test.sh OUT`. Architecture (hexagonal) and design decisions: `docs/decisions/`.

## Known limits

- KWin 6.3: the number of outputs is fixed when a user's session starts (in `login` mode it follows the client's monitors; a later
  connection with another count keeps it), and resizing is done by scaling (black bars).
- No NLA in `login` mode (dynamic accounts, IronRDP limit): TLS only.
- Clipboard: no primary selection; not tested with Klipper.
- Printers: no full Easy Print (MS-RDPEXPS): tray, duplex and colour are not transferred.
- SELinux enforcing (Fedora/Rocky) is untested and no policy is shipped.
- Not covered: smart cards, microphone, USB, several simultaneous clients per user, HiDPI.

## License

[AGPL-3.0-only](LICENSE). Dependencies are permissive (MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0); `libopenh264`
is loaded at run time only; `third_party/` keeps the licenses of its crates.
