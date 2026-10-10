# 004 — Spike KWin virtuel : capture et entrées (M3)

Statut : accepté. Mesuré sur la VM Debian 13 (KWin 6.3.6, xdg-desktop-portal-kde 6.3.5, PipeWire 1.4.2).

## Constats
- `kwin_wayland --virtual --socket <nom> --width W --height H --no-lockscreen --no-global-shortcuts` démarre sans GPU
  dans un `dbus-run-session`. Il enregistre `org.kde.KWin` sur le bus de session. `--output-count` existe (mode fenêtré).
- Un client Wayland quelconque ne voit PAS `zkde_screencast_unstable_v1` ni `org_kde_kwin_fake_input`
  (ni `zwlr_screencopy`, ni `ext_image_copy_capture`). Il voit `zwlr_data_control_manager_v1` (presse-papiers, M4).
- **Autorisation** : un `.desktop` avec `Exec=<chemin absolu de l'exécutable>` et
  `X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1,org_kde_kwin_fake_input` suffit (journal KWin :
  `authorized "<exe>" "<interface>"`). KWin cherche le service via sycoca : il faut `XDG_MENU_PREFIX=plasma-`
  (présent dans une vraie session Plasma) et un sycoca à jour (`kbuildsycoca6`) ; sans cela l'autorisation échoue
  silencieusement.
- `org.kde.KWin.EIS.RemoteDesktop.connectToEIS(i)` répond sans autorisation (fd libei + cookie).
- `/org/kde/KWin/ScreenShot2` est absent ; le portail ScreenCast/RemoteDesktop n'est pas listé en session headless.
- Le protocole `zkde_screencast` offre aussi `stream_virtual_output` (création de sorties virtuelles) : piste pour M5.

## Décisions
- **Capture** : `zkde_screencast.stream_output` → nœud PipeWire → consommateur PipeWire (crate `pipewire`).
  Pas de portail (pas de prompt utilisateur).
- **Entrées** : `org_kde_kwin_fake_input` (même mécanisme d'autorisation, aucune dépendance D-Bus/libei).
  libei reste l'alternative si fake_input disparaît (interface qualifiée de détail d'implémentation par KDE).
- Le paquet installe `/usr/share/applications/rdsktp.desktop` (Exec=/usr/bin/rdsktp) avec ces interfaces.
- Adaptateurs séparés (`rdsktp-capture`, `rdsktp-input`), chacun ouvre sa propre connexion Wayland.
- Prérequis de build : `libpipewire-0.3-dev`, `libclang` (bindgen) — Fedora : `pipewire-devel clang-devel`.

## Addendum : KWin exige OpenGL et un nœud DRM de rendu fonctionnel

Le screencast KWin refuse le flux sans compositing OpenGL (« OpenGL compositing is required for screencasting »). Constats sur la VM Debian 13 (KWin 6.3.6, Mesa 25.0.7) :

- **Sans `/dev/dri`** (VGA VMware, `vmwgfx` refuse l'hyperviseur) : KWin virtual retombe sur un compositeur non-GL (« Configured compositor not supported by Platform »). `KWIN_COMPOSE`, `LIBGL_ALWAYS_SOFTWARE`, `EGL_PLATFORM=surfaceless` n'y changent rien.
- **`virtio-vga` sans 3D** : un nœud `renderD128` existe, OpenGL s'initialise (llvmpipe), mais Mesa (`kms_swrast`) alloue ses tampons par `DRM_IOCTL_MODE_CREATE_DUMB`, interdit par le noyau sur un nœud de rendu (EACCES). Le swapchain de KWin reste vide et `WorkspaceSceneOpenGL::textureForOutput` → `EglSwapchainSlot::texture()` plante (SIGSEGV) dès qu'un flux démarre. `MESA_LOADER_DRIVER_OVERRIDE=swrast`, `LIBGL_ALWAYS_SOFTWARE=1`, l'accès à `/dev/udmabuf` n'y changent rien.
- **`virtio-vga-gl` (virgl, `-display egl-headless`)** : Mesa utilise le pilote `virtio_gpu` et KWin streame correctement. **6/6 checks** de `vm-test --session kwin` (key→pixel p95 ≈ 67 ms, ~22 fps).

Prérequis qui en découlent pour la prod :
- un GPU (même virtuel) exposant un nœud DRM de rendu avec un pilote Mesa 3D (virgl, GPU passthrough, etc.) ; **pas de chemin 100 % logiciel validé** avec Mesa 25.0 de Debian 13 ;
- l'utilisateur de la session doit être dans les groupes `video` et `render` (`SupplementaryGroups=` dans l'unité) ;
- `vm/vm.sh` : `VM_VGA=virtio-gl` par défaut (`-device virtio-vga-gl -display egl-headless`), l'hôte doit avoir un rendu GL.

Question ouverte pour l'utilisateur : la VM de production a-t-elle un GPU virtuel 3D ? Sinon il faut une autre piste (Mesa plus récent, backport, ou autre compositeur).
