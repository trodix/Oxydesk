# RustyDesktop — serveur RDP headless pour KDE Plasma (Rust)

Serveur RDP (IronRDP 0.13, H.264 via EGFX) pour KDE Plasma Wayland headless, en remplacement de xrdp : bureau synthétique de test, session KWin virtuelle, ou connexion distante avec PAM (`--login`, une session par utilisateur). Presse-papiers, audio, entrées, multi-écran et **redimensionnement dynamique** (Display Control, nombre d'écrans compris).

## Multi-écran
- `--outputs N` (1 à 16) annonce N écrans ; un client mstsc à plusieurs moniteurs voit les siens (patch local d'IronRDP dans `third_party/`, ADR 011).
- Display Control : le serveur suit la disposition complète du client (taille, position, nombre de moniteurs). KWin ≥ 6.7 : modes personnalisés et sorties virtuelles. KWin 6.3 (Debian 13) : nombre de sorties fixé par `--outputs`, chaque sortie mise à l'échelle à la taille demandée (`rdsktp-scaling`).
- Mode `--login` : même comportement, relayé à l'agent par `SetLayout`/`LayoutApplied`.
- Tests : `cargo xtask test --client` avec `WIN_MONITORS=2` (mstsc réel), `cargo xtask vm-test --session login --outputs 2 --multi-resize` (`RDP_TEST_PASSWORD` : mot de passe de test quelconque, posé sur le compte de la VM), `rdp-test-client --multi-resize` (`--max-monitors N`, `--no-identity-blocks`).

## État des fonctionnalités

### Fonctionne (testé)
| Fonction | Détail |
|---|---|
| Connexion RDP | TLS, NLA possible en mode fixe (`--nla-user`). Testée avec mstsc (VM Windows 11) et le client de test IronRDP. |
| Vidéo | H.264 par EGFX, openh264 (logiciel) ou VA-API (GPU). Latence touche-pixel de 25 à 70 ms. |
| Session KDE Wayland | Session KWin virtuelle (`--session kwin`), capture PipeWire/screencast, DMA-BUF avec encodeur GPU. |
| Mode login distant | PAM (`authd`), un agent par utilisateur, vraie session logind. Validé sur Debian 13 (KWin 6.3). |
| Clavier et souris | Entrées par `fake_input` de KWin. |
| Forme du curseur | Le client dessine lui-même le curseur (pas de latence) ; le serveur lui envoie la forme (PDU pointeur RDP) à chaque changement : flèche, texte, main, redimensionnement… Vérifié avec xfreerdp contre KWin 6.7. |
| Presse-papiers | Texte, images et fichiers (plus de 1 Gio), bidirectionnel, accents et `€`, plus de 64 Kio. |
| Audio | Sortie son (tonalité 440 Hz contrôlée). |
| Reconnexion | Reprise après coupure ou fermeture ; la session survit aux déconnexions. |
| Multi-écran | Jusqu'à 16 écrans ; mstsc à 2 moniteurs contre le bureau synthétique et KWin réel (patch local d'IronRDP). |
| Redimensionnement dynamique | Display Control, nombre d'écrans variable. KWin ≥ 6.7 : modes personnalisés et sorties virtuelles ; KWin 6.3 : mise à l'échelle par sortie. |

### Limites connues
- **KWin 6.3** : nombre de sorties figé par `--outputs`, redimensionnement par mise à l'échelle (bandes noires), disposition refusée tant qu'une sortie n'a pas envoyé de trame.
- **Bureau statique sous KWin** : KWin n'envoie une trame que sur dommage ; après un redimensionnement l'écran peut rester vide tant que rien ne bouge.
- **Presse-papiers** : pas de sélection primaire ; non testé avec Klipper.
- **NLA en mode login** impossible avec des comptes dynamiques (limite d'IronRDP) : TLS seul.
- **Patch IronRDP local** (`third_party/`) à réexaminer à chaque montée de version.

### Compatibilité FreeRDP (test automatisé, `sdl-freerdp` 3.32)
Validé : TLS, EGFX V8.1/V10.7 avec AVC420 (≈ 45 trames en 6 s en synthétique, ≈ 28 sur KWin réel), canal audio, 2 écrans (bureau 3840×1080, 2 surfaces), reconnexion, NLA avec bon mot de passe accepté et mauvais refusé.
Défauts trouvés puis corrigés :
- **`AVC_DISABLED` perdu à la négociation** : la bibliothèque EGFX intersecte les drapeaux du client avec ceux du serveur, et le serveur n'avait pas ce drapeau. Le serveur le reprend maintenant du client (`GfxHandler::preferred_capabilities`) ; un client sans H.264 reçoit des pixels non compressés par EGFX.
- **Pas de repli sans GFX** : un client sans EGFX (`/network:modem`) ne recevait rien. Après 3 s sans EGFX prêt, la pompe envoie des mises à jour bitmap non compressées (IronRDP les encode).
- **SIGTERM** : `rdsktp` ne s'arrêtait proprement que sur Ctrl-C ; la session KWin et ses applications restaient orphelines à l'arrêt d'un service.

Limites des replis (derniers recours, bien plus lourds que H.264) : en mode `--login` l'agent n'envoie que du H.264, donc ces clients n'ont rien tant que l'agent n'envoie pas de pixels bruts ; la justesse des pixels n'est pas contrôlée (le test vérifie la réception sans erreur, pas l'image) ; débit élevé (≈ 8 Mo par image en 1080p), régulé par la contre-pression EGFX.

### Non couvert ou non vérifié
Copie d'images et de fichiers avec de vrais clients (mstsc, xfreerdp, Remmina), imprimantes, lecteurs, cartes à puce, microphone, USB ; plusieurs utilisateurs ou clients simultanés ; verrouillage de session et SDDM ; HiDPI ; Remmina lui-même et les clients mobiles ; curseurs animés et curseurs d'applications plus grands que 96 px ; coût CPU de N encodeurs logiciels ; paquet Debian.

## Plan priorisé
Efforts estimés, pas mesurés.

**Règle de validation.** La cible principale est la compatibilité avec **mstsc** (client Windows 11 de la VM, `cargo xtask test --client`). Toute fonction ajoutée doit être testée sur mstsc, puis sur les autres clients déjà testés dans le projet : xfreerdp et Remmina (`ci/clients-test.sh`, `ci/freerdp-test.sh`, `ci/freerdp-login-test.sh`). Un test avec le seul client de test IronRDP ne suffit pas pour déclarer une fonction faite.

**P0 — bloquants pour un usage réel**
1. Tests avec FreeRDP et Remmina. **Fait en partie** : `ci/freerdp-test.sh` (synthétique et KWin réel) couvre connexion, EGFX/H.264, 2 écrans, audio, reconnexion, NLA. **Fait** : clavier, souris, presse-papiers, redimensionnement, xfreerdp et Remmina via `ci/clients-test.sh` (conteneur Docker ou Podman, Xvfb + xdotool). Mode `--login` sur la VM : `ci/freerdp-login-test.sh` (voir plus bas). **Fait aussi** : images et fichiers du presse-papiers sur **mstsc** (`cargo xtask test --client`, 13/13 contrôles dont un fichier de 128 Mio collé dans l'Explorateur ; `WIN_BIG_MIB` règle la taille) et sur **xfreerdp** (`ci/clients-test.sh`). Limites des clients : xfreerdp 3.15 n'offre que des images DIB (`image/bmp`, pas `image/png`) ; Remmina 1.4.39 échange les images mais n'a pas de presse-papiers de fichiers (scénario ignoré, « SKIP »). Pour ces tests le serveur de test tourne avec `RDSKTP_ECHO_FILES=copy` (l'écho copie les fichiers à côté : un client abandonne le contenu de son ancien presse-papiers dès qu'il change) et le conteneur reçoit `/dev/fuse` (FreeRDP expose les fichiers distants par FUSE).
1bis. **Repli sans H.264 / sans GFX** : fait en synthétique et en session KWin (voir « Compatibilité FreeRDP »). **Fait aussi en mode `--login`** : l'agent envoie des pixels bruts (`ToAgent::SetPixels`, `FromAgent::VideoRaw`) quand le client ne sait pas décoder le H.264, et revient au flux H.264 ensuite ; ils partent en EGFX non compressé ou en bitmaps selon le client. **Pixels contrôlés** : les scénarios `fallback_nohh264` et `fallback_nogfx` de `ci/clients-test.sh` comparent l'image reçue au fond synthétique (0 pixel faux sur 50 336), le bloc magenta d'angle, le battement et le marqueur clavier, sur xfreerdp et Remmina. Reste : le même contrôle en mode `--login` (VM, `ci/freerdp-login-test.sh`).
2. Verrouillage de session et SDDM : **fait** (voir « Verrouillage de session » ci-dessous).
3. Durcissement du mode login : **fait** (voir « Limitation des essais » ci-dessous).

**P1 — fonctions de base manquantes**
4. Presse-papiers d'images : **fait** (voir « Presse-papiers d'images » ci-dessous). Les fichiers : **fait** (voir « Presse-papiers de fichiers »). Reste leur validation sur mstsc (point 1).
5. HiDPI. Moyen.
6. Curseur distant. **Fait** (voir « Curseur distant » ci-dessous).
7. Plusieurs utilisateurs en parallèle. Moyen.
8. Disposition du clavier. **Fait** (voir « Disposition du clavier » ci-dessous) ; testé automatiquement avec mstsc ; reste la saisie réelle (touches mortes, AltGr, pavé numérique). Moyen.
9. Redirection de lecteurs (`rdpdr`). **Fait** (voir « Redirection de lecteurs » ci-dessous). Élevé.

**P2 — qualité et exploitation**
10. Mesure du coût CPU en multi-écran. Faible.
11. Paquet Debian (unités systemd, gestion du certificat TLS, configuration). Moyen.
12. Plusieurs clients sur une même session. Moyen.
13. Proposer le patch IronRDP en amont. Variable.

**P3 — facultatif** : imprimantes, USB, cartes à puce, transport UDP ; NLA en mode login (dépend d'IronRDP) ; abandon du repli de mise à l'échelle quand KWin ≥ 6.7 sera la cible.

**Tout en bas de la liste (pas prioritaire)**
14. Microphone (canal audio en entrée). Moyen.
15. Écrans 4K en mode `--login` : une trame brute dépasse les 32 Mo du protocole agent (ne concerne que le repli sans H.264). Moyen.

Ordre conseillé : 1 (mstsc d'abord) et 1bis, puis 5, 7 et 8, puis 9, le reste selon l'usage.

## Presse-papiers d'images
Les images se copient dans les deux sens, en plus du texte. Côté bureau, l'adaptateur Wayland échange du PNG (`image/png`, via data-control) ; côté RDP, le format est CF_DIB (24 bits, ligne par ligne du bas vers le haut), converti par `crates/rdp/src/dib.rs`. Le texte l'emporte quand une application offre les deux ; la dernière copie gagne. La transparence est aplatie sur du blanc vers Windows (les applications ne s'accordent pas sur l'alpha d'un DIB) ; en sens inverse, les DIB 24 et 32 bits non compressés sont lus (masques BGR(A) usuels), les autres formats (palette, RLE, 16 bits) sont ignorés avec un avertissement. Plafond : 64 Mio de pixels décodés. En mode `--login`, l'image passe par l'agent (`ToAgent::ClipboardImageSet`, `FromAgent::ClipboardImage`). Test : les scénarios `clipboard_image_roundtrip` (37×23, largeur impaire pour le remplissage des lignes) et `clipboard_image_large` (1920×1080 de pixels pseudo-aléatoires, soit un PNG de 6 Mo et un DIB de 6 Mo) du client de test passe par le client RDP, le serveur, la sélection Wayland et l'application de test, et revient identique (vérifié sur la VM avec `cargo xtask vm-test --session login`). Validé avec de vrais clients : mstsc (images 37×23 et 64×48, `cargo xtask test --client`), xfreerdp et Remmina (image 301×77 identique après aller-retour ; xfreerdp ne lit que des cibles DIB, pas `image/png`).

## Presse-papiers de fichiers
Les fichiers et dossiers se copient dans les deux sens (CLIPRDR : `FileGroupDescriptorW` et `FileContents`), sans limite de taille pratique : le contenu est lu **par blocs de 512 Kio à la demande**, jamais stocké ni mis en mémoire en entier. Côté bureau, une liste de fichiers venant du client est présentée par un système de fichiers FUSE en lecture seule (`fuser`, paquet `fuse3` requis) et publiée en `text/uri-list` ; les lectures de l'application déclenchent les `FileContentsRequest` vers le client. Dans l'autre sens, les fichiers copiés sur le bureau sont lus par plages quand le client colle. Chemins relatifs des dossiers conservés ; les liens symboliques vers des dossiers et les fichiers spéciaux sont ignorés. Une liste périmée échoue vite, une plage sans réponse expire après 60 s, et une erreur de presse-papiers n'interrompt plus la connexion. En mode `--login`, l'agent relaie les plages (`FilesSet`, `Files`, `FileRead`, `FileData`). Conception : [ADR 012](docs/decisions/012-fichiers-presse-papiers.md).

Test : scénarios `clipboard_files*` du client de test (`--large-file-mib N` pour un gros fichier). Un fichier de 1,5 Gio revient identique en bureau synthétique, en session KWin et en mode `--login` sur la VM. Validé avec mstsc (dossier avec accent et fichier vide, puis un fichier de 128 Mio collé par l'Explorateur, comparaison SHA-256) et xfreerdp (5 fichiers dont 1,5 Mo, via FUSE). Remmina 1.4.39 ne gère pas les fichiers.

## Redirection de lecteurs
Les dossiers partagés par le client (mstsc « Lecteurs locaux », xfreerdp `/drive:NOM,DOSSIER`, Remmina « Partager un dossier ») sont montés dans la session sous `$XDG_RUNTIME_DIR/rdsktp-drives/<NOM>` (FUSE, paquet `fuse3`), en **lecture et écriture** : création, renommage, suppression, dossiers, fichiers de plusieurs gigaoctets lus et écrits par blocs, sans stockage intermédiaire. Le montage disparaît à la déconnexion. `--drives-dir DIR` change le répertoire (`off` le désactive) ; activé par défaut avec `--session kwin`. En mode `--login`, le serveur relaie les opérations à l'agent, qui monte le dossier sous l'utilisateur. Conception : [ADR 013](docs/decisions/013-redirection-de-lecteurs.md).

Test : `bash ci/drives-test.sh` (client de test IronRDP contre le bureau synthétique ; 64 Mio dans chaque sens, renommage, dossiers, suppression, démontage) ; `bash ci/clients-test.sh` fait aussi contrôler les fichiers par l'hôte pendant que xfreerdp et Remmina restent connectés. Validé avec mstsc (`cargo xtask test --client`), xfreerdp et Remmina ; le relais du mode `--login` n'est pas encore testé en VM (voir l'ADR 013).

## Disposition du clavier
RDP envoie des touches physiques (scancodes) : le caractère obtenu dépend de la disposition de la session. À chaque connexion, le serveur lit l'identifiant de disposition Windows (KLID) annoncé par le client (patch `RdpServerInputHandler::keyboard_layout` dans `third_party/ironrdp-server`), le convertit en disposition XKB (`rdsktp_core::klid_to_xkb`, ~45 langues : `0x040C` → `fr`, `0x0807` → `ch(de)`…) et la session l'adopte. Adaptateur KWin : `kxkbrc` (groupe `Layout`) écrit par `kwriteconfig6`, puis signal D-Bus `org.kde.keyboard.reloadConfig` (celui du module clavier de KDE) ; en mode `--login`, l'ordre passe par l'agent (`ToAgent::KeyboardLayout`). Une disposition inconnue ou absente laisse celle de la session. `RDSKTP_KEYBOARD_LAYOUT=off` désactive l'adoption (à utiliser quand le serveur partage le bus D-Bus d'un vrai bureau : la disposition de l'utilisateur serait modifiée).

Test : le client de test annonce `--keyboard-layout 0x40C` ; le bureau synthétique l'affiche par la couleur du marqueur (scénario `keyboard_layout`). Sur la VM Debian 13 (KWin 6.3, mode `--login`), `kxkbrc` contient `LayoutList=fr` et KWin répond `getLayoutsList` = `fr`. **mstsc automatique** (`cargo xtask test --client`, 14/14) : le script active la disposition française (`WIN_KLID`, `040C` par défaut) sur le fil qui héberge le contrôle avant la connexion, et le contrôle `keyboard_layout` vérifie que le bureau synthétique l'a reçue (marqueur orange). Avec `WIN_SESSION=kwin`, le serveur est lancé avec `RDSKTP_KEYBOARD_LAYOUT=off` (il partage le bus D-Bus de l'hôte) et ce contrôle est ignoré. Non essayé : la saisie réelle (AltGr, touches mortes, pavé numérique).

## Verrouillage de session
KWin n'est plus lancé avec `--no-lockscreen` : l'écran de verrouillage (`kscreenlocker_greet`) fait partie de la session, donc il apparaît dans l'image capturée, le clavier et la souris RDP l'atteignent, et le mot de passe tapé est vérifié par PAM. Le verrou survit à la déconnexion : le client qui revient retrouve le même KWin, toujours verrouillé, et le déverrouille avec le mot de passe du compte. Test (VM Debian 13, xfreerdp en conteneur) : `RDP_TEST_PASSWORD=... bash ci/lock-test.sh` (verrou affiché, persistance à la reconnexion avec le même KWin, mauvais mot de passe refusé, déverrouillage).
Comment verrouiller : le bouton « Verrouiller » du lanceur Plasma, `loginctl lock-session` ou `dbus-send --session --type=method_call --dest=org.freedesktop.ScreenSaver /ScreenSaver org.freedesktop.ScreenSaver.Lock`. Meta+L verrouille aussi (les raccourcis globaux de KWin sont actifs ; mstsc garde Win+L pour la machine locale). Le verrouillage sur inactivité est assuré par `kscreenlocker` dans KWin (pas par SDDM, qui ne fait qu'ouvrir la session) : réglage `Autolock`/`Timeout` (minutes, 5 par défaut) de `~/.config/kscreenlockerrc`, section `[Daemon]` ; vérifié sur la VM avec `Timeout=1`.
**SDDM** n'a pas de rôle dans cette architecture : `rdsktp-authd` (PAM) remplace l'écran de connexion et ouvre une vraie session logind par utilisateur, sans gestionnaire d'affichage. La session RDP est indépendante d'une éventuelle session SDDM de la même machine.

## Limitation des essais (mode `--login`)
Après `--max-login-failures` (5 par défaut) mots de passe refusés depuis une même adresse, ou `--max-user-failures` (10) contre un même compte, en moins de 5 minutes, l'adresse ou le compte est bloqué `--login-lockout-secs` (300 s). Une adresse bloquée est coupée dès l'acceptation TCP ; un compte bloqué est refusé sans interroger PAM, même avec le bon mot de passe. Un succès remet les compteurs à zéro. Les échecs, blocages et connexions réussies sont journalisés avec l'adresse et le compte (échappé). Les tables sont bornées (4096 entrées). Le seuil par compte est plus haut car il permet à quiconque connaît un nom de verrouiller son titulaire ; derrière un proxy ou un NAT, toutes les connexions partagent une même adresse. Logique dans `crates/core/src/throttle.rs`, branchement dans `crates/rdp/src/server.rs`. Les compteurs sont en mémoire : un redémarrage les efface. Test : `ci/freerdp-login-test.sh`.

## Curseur distant
KWin dessine toujours son propre curseur dans l'image capturée (backend virtuel), quel que soit le mode de pointeur du screencast : il doublerait celui du client RDP, avec une traînée. La session utilise donc un thème de curseur transparent (`crates/session/src/cursor_theme.rs`) où chaque nom de curseur (`left_ptr`, `text`, `pointer`…) est un fichier transparent dont la **largeur code le rang du nom**. Le screencast (mode `metadata`, `SPA_META_Cursor` de PipeWire) livre ce bitmap ; `ThemeResolver` y reconnaît le marqueur et le remplace par l'image du vrai thème de l'utilisateur (`cursorTheme` de `kcminputrc`, `XCURSOR_THEME`, sinon breeze/Adwaita, héritage compris, taille nominale proche de 32). Le résultat part en `DisplayUpdate::RGBAPointer` (ou `HidePointer`) ; en mode `--login`, l'agent l'envoie par `FromAgent::Cursor`.
Limites : un curseur d'application (bitmap propre) passe tel quel, donc seulement s'il tient dans 96 px, et reste dessiné dans l'image par KWin ; curseurs animés (attente) : première image ; un client qui se connecte reçoit la forme au prochain changement seulement. `RDSKTP_POINTER=hidden` désactive la forme (ancien comportement).

## Commandes
- `cargo test --workspace` — tests unitaires (core, client de test + mock, xtask).
- `cargo xtask test` — tests unitaires seulement (rapide) ; `--server` ajoute le test serveur (VM Debian 13, `vm-test`), `--client` le test client (client Windows mstsc, lent : snapshot QEMU Windows 11, session RDP avec le moteur du client Microsoft : connexion, images, presse-papiers, audio, demande de redimensionnement). `--freerdp` le test de compatibilité FreeRDP (voir plus bas). `--clients` les vrais clients xfreerdp et Remmina dans un conteneur (voir plus bas). `--integration` lance les quatre, indépendants. Prérequis : KVM, qemu ; VM Debian créée une fois (`bash vm/vm.sh create`), VM Windows installée une fois (`bash vm/win11.sh install`, avec swtpm).
- `bash ci/freerdp-test.sh` — compatibilité FreeRDP (`sdl-freerdp` sans affichage, `FREERDP_SESSION=kwin` pour une vraie session KWin) : connexion TLS, EGFX/H.264, 2 écrans, audio, reconnexion, NLA accepté/refusé. Journaux dans `artifacts/freerdp-test/`.
- `bash ci/clients-test.sh [xfreerdp|remmina]` — vrais clients (xfreerdp 3 et Remmina) dans un conteneur Debian 13 (`docker/clients/`) : Xvfb, openbox, xdotool, xclip. Contre le bureau synthétique, il vérifie comme un utilisateur la taille et les images affichées, le clavier (couleur du marqueur), la souris (bloc suivant le pointeur, boutons gauche/droit), le presse-papiers (aller-retour) et le redimensionnement dynamique (fenêtre redimensionnée, pour les deux clients). Seul Docker **ou** Podman est requis sur la machine (`CONTAINER_ENGINE=docker|podman` pour forcer ; Podman préféré, sans root). Journaux et captures dans `artifacts/clients-test/`.
- `RDP_TEST_PASSWORD=... bash ci/freerdp-login-test.sh` — FreeRDP contre le mode `--login` de la VM Debian 13 (à démarrer avant : `cargo xtask vm-test --session login --keep`) : H.264, client sans H.264, client sans EGFX, retour à H.264 sur le même agent, mauvais mot de passe refusé. Journaux dans `artifacts/freerdp-login-test/`.
- `cargo run -p rdp-test-client -- --mock --out /tmp/out` — client de test contre le mock.
- `bash vm/vm.sh create` — télécharge/provisionne la VM Debian 13 (une fois, état dans `vm/state/`).
- `cargo xtask vm-test` — cycle complet build → VM → client → rapport (`artifacts/vm-test/latest.json`).
- `bash ci/check-deps.sh` — vérifie les règles de dépendance hexagonales.

Les overlays qcow2 utilisent des chemins absolus : si le dépôt est déplacé, relancer `vm.sh create`.
Architecture : `docs/decisions/001-architecture-hexagonale.md`.

## Nommage
Le projet s'appelle **RustyDesktop**. Le serveur RDP est le binaire (et service) `rdsktp` ; le démon privilégié sera `rdsktp-authd`.
