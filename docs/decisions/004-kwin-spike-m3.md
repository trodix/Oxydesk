# 004 — Virtual KWin spike: capture and input (M3)

Status: accepted. Measured on the Debian 13 VM (KWin 6.3.6, xdg-desktop-portal-kde 6.3.5, PipeWire 1.4.2).

## Findings
- `kwin_wayland --virtual --socket <name> --width W --height H --no-lockscreen --no-global-shortcuts` starts without a GPU
  inside a `dbus-run-session`. It registers `org.kde.KWin` on the session bus. `--output-count` exists (windowed mode).
- An arbitrary Wayland client does NOT see `zkde_screencast_unstable_v1` nor `org_kde_kwin_fake_input` (nor `zwlr_screencopy`,
  nor `ext_image_copy_capture`). It does see `zwlr_data_control_manager_v1` (clipboard, M4).
- **Authorisation**: a `.desktop` file with `Exec=<absolute path of the executable>` and
  `X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1,org_kde_kwin_fake_input` is enough (KWin log:
  `authorized "<exe>" "<interface>"`). KWin looks the service up through sycoca: it needs `XDG_MENU_PREFIX=plasma-` (present in
  a real Plasma session) and an up-to-date sycoca (`kbuildsycoca6`); otherwise the authorisation fails silently.
- `org.kde.KWin.EIS.RemoteDesktop.connectToEIS(i)` answers without authorisation (libei fd + cookie).
- `/org/kde/KWin/ScreenShot2` is absent; the ScreenCast/RemoteDesktop portal is not listed in a headless session.
- The `zkde_screencast` protocol also offers `stream_virtual_output` (creation of virtual outputs), used later for dynamic
  monitors on KWin 6.7 (ADR 011).

## Decisions
- **Capture**: `zkde_screencast.stream_output` → PipeWire node → PipeWire consumer (`pipewire` crate). No portal (no user prompt).
- **Input**: `org_kde_kwin_fake_input` (same authorisation mechanism, no D-Bus/libei dependency). libei remains the alternative
  if fake_input goes away (KDE calls the interface an implementation detail).
- The package installs two desktop files, `/usr/share/applications/oxydesk.desktop` (`Exec=/usr/bin/oxydesk`, direct KWin
  mode) and `/usr/share/applications/oxydesk-agent.desktop` (`Exec=/usr/bin/oxydesk-agent`, login mode, ADR 008), both with these
  interfaces (`dist/`).
- Separate adapters (`oxydesk-capture`, `oxydesk-input`), each opening its own Wayland connection.
- Build prerequisites: `libpipewire-0.3-dev`, `libclang` (bindgen); Fedora: `pipewire-devel clang-devel`.

## Addendum: KWin needs OpenGL and a working DRM render node

KWin's screencast refuses the stream without OpenGL compositing ("OpenGL compositing is required for screencasting"). Findings
on the Debian 13 VM (KWin 6.3.6, Mesa 25.0.7):

- **Without `/dev/dri`** (VMware VGA, `vmwgfx` refuses the hypervisor): the virtual KWin falls back to a non-GL compositor
  ("Configured compositor not supported by Platform"). `KWIN_COMPOSE`, `LIBGL_ALWAYS_SOFTWARE`, `EGL_PLATFORM=surfaceless` change
  nothing.
- **`virtio-vga` without 3D**: a `renderD128` node exists and OpenGL initialises (llvmpipe), but Mesa (`kms_swrast`) allocates
  its buffers with `DRM_IOCTL_MODE_CREATE_DUMB`, which the kernel forbids on a render node (EACCES). KWin's swapchain stays
  empty and `WorkspaceSceneOpenGL::textureForOutput` → `EglSwapchainSlot::texture()` crashes (SIGSEGV) as soon as a stream
  starts. `MESA_LOADER_DRIVER_OVERRIDE=swrast`, `LIBGL_ALWAYS_SOFTWARE=1` and access to `/dev/udmabuf` change nothing.
- **`virtio-vga-gl` (virgl, `-display egl-headless`)**: Mesa uses the `virtio_gpu` driver and KWin streams correctly. **6/6
  checks** of `vm-test --session kwin` (key→pixel p95 ≈ 67 ms, ~22 fps).

Resulting production prerequisites:
- a GPU (even virtual) exposing a DRM render node with a 3D Mesa driver (virgl, GPU passthrough, ...); **no 100 % software path
  validated** with the Mesa 25.0 of Debian 13;
- the session user must be in the `video` and `render` groups (`SupplementaryGroups=` in the unit);
- `vm/vm.sh`: `VM_VGA=virtio-gl` by default (`-device virtio-vga-gl -display egl-headless`); the host must render GL.

Open question: does the production VM have a 3D virtual GPU? If not, another approach is needed (newer Mesa,
backport, or another compositor).
