# ADR 010 — Validation avant production (hors VMware de prod)

Date : 2026-10-10. Périmètre : tout ce qui pouvait être validé sans la VM VMware de production ni Windows.

## 1. Build et CI sur Debian 13 (conteneur `ci/Dockerfile.trixie`)
- Exécutés dans le conteneur trixie : `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test --workspace`, `ci/check-deps.sh`, builds avec les features `kwin` et `vaapi` (bindgen de `cros-libva` contre les en-têtes `libva-dev` de trixie), `cargo deb -p rdsktp`.
- Corrections nécessaires : `libva-dev` ajouté à l'image ; `cargo fmt --all` n'avait jamais été appliqué (≈39 fichiers) ; lints clippy (`chunks_exact_to_as_chunks`, variables inutilisées hors feature `kwin`, `field_reassign_with_default`, processus enfant jamais attendu dans `rdsktp-testapp`) ; `exclude = ["third_party"]` dans le workspace (le code vendorisé était linté comme membre).
- Job CI `features` ajouté (`.github/workflows/ci.yml`). La feature `vaapi` reste désactivée par défaut dans le `.deb` (pas de dépendance `libva2` documentée tant qu'on ne l'active pas).

## 2. Clients réels et reconnexion
- **xfreerdp 3.32.1 graphique** (`/gfx:AVC420 /sound /clipboard +auto-reconnect`) sur la VM QEMU en session `kwin` : connexion, EGFX V8_1 avec AVC420, bureau KWin affiché (capture `artifacts/`, non versionnée), audio RDPSND ouvert, 693 images envoyées en 30 s dont 1 abandonnée. Non exercés à la main : saisie interactive, `/multimon`, redimensionnement (couverts seulement par le client de test).
- **mstsc.exe réel (Windows 11 Pro, build 26300, VM QEMU `vm/win11.sh`)** contre le serveur en mode `--login` (Debian 13 + KWin, PAM) : connexion TLS, authentification PAM (`session granted … user=dev`), EGFX V10, bureau KWin affiché, saisie clavier prouvée (trois touches envoyées dans la session mstsc changent le marqueur de `rdsktp-testapp`, couleur lue sur la capture). Non exercés avec mstsc : presse-papiers, audio, `/multimon` (la VM Windows n'a qu'un écran), redimensionnement dynamique.
- **KRDC** : non exécuté (utilise FreeRDP ; aucun écart attendu mais non vérifié).
- **Nouveau scénario `reconnect`** (client de test, 8ᵉ vérification) : perte brutale du lien (socket fermée sans PDU de déconnexion) puis reconnexion, puis déconnexion propre et reconnexion ; chaque fois un bureau non noir de la bonne taille doit revenir. Mock avec faute `no_reconnect` (test : seule `reconnect` échoue). Résultat réel : synthétique 3/3 et KWin 8/8, reconnexion en ≈ 20 ms.
- **Bogue trouvé et corrigé par ce scénario** : `ironrdp-server` utilise **un seul canal d'événements pour toutes les connexions**. Le thread `audio-pump` d'une connexion morte continuait d'y écrire des paquets qui atteignaient la connexion suivante avant la négociation RDPSND → `failed to send rdpsnd event … client format not yet received`, reconnexion coupée. Correctif (`crates/rdp/src/sound.rs`) : émetteur capturé au démarrage du flux, arrêt du pump au `Drop` du handler, drapeau revérifié après la lecture bloquante avant l'envoi.
- **Deuxième connexion pendant la première** : non refusée, elle reste dans le backlog TCP et n'est servie qu'à la fin de la première (mesuré : 17 s d'attente sans retour au client). Comportement à documenter pour les utilisateurs ; une vraie politique (refus ou remplacement) reste à décider.
- Bruit sans gravité : `Illegal SNI extension` (client connecté par IP).

## 3. NLA
Mesures xfreerdp contre le serveur en TLS seul (`/auth-only`) :

| Option client | Résultat |
|---|---|
| défaut | succès (repli sur TLS) |
| `/sec:tls` | succès |
| `/sec:nla` | échec : `Protocol Security Negotiation Failure` (le serveur n'offre pas NLA) |
| `/sec:rdp` | échec : le serveur exige TLS |

- **mstsc 26300 (mesuré, contre le serveur sans NLA)** :

| Fichier `.rdp` | Résultat |
|---|---|
| défaut (CredSSP proposé) | **succès** : le serveur choisit TLS, mstsc accepte le repli, envoie les identifiants (avertissement de certificat auto-signé à accepter) |
| `enablecredsspsupport:i:0` seul | échec : mstsc ne propose que le protocole `0x0` (RDP hérité), le serveur exige TLS (`0x904`/`0x7`) |
| `negotiate security layer:i:1` + `enablecredsspsupport:i:0` | TLS établi, mais mstsc n'envoie **aucun identifiant** (`user=` vide) : refusé par PAM |

- IronRDP (`ironrdp-server` 0.13.0, dernière version stable) : `CredentialValidator` n'est appelé que pour le mode TLS (identifiants du ClientInfoPdu, déjà utilisé avec PAM). Pour CredSSP/Hybrid, NTLM exige des identifiants préchargés : aucune délégation vers PAM. Pas de voie claire, donc aucun code (cf. ADR 008).
- **Conséquence prod (corrige la première version de cet ADR)** : ne **pas** poser `enablecredsspsupport:i:0`, qui empêche mstsc d'envoyer les identifiants. Le mstsc par défaut se replie seul sur TLS et fonctionne ; il faut seulement accepter l'avertissement du certificat auto-signé (distribuer le certificat ou fournir un certificat de confiance). Non testé : une stratégie de groupe imposant NLA côté client, qui devrait empêcher ce repli.

## 4. VMware / vSphere (Lot 4)
Non fait : VMware Workstation n'est pas installé sur le poste. Informations à relever sur l'hôte de production : version d'ESXi, version de matériel virtuel de la VM, « 3D Support » activé ou non, « 3D Renderer » (Automatique / Logiciel / Matériel), VRAM allouée. Rappel (ADR 004) : KWin exige OpenGL et un nœud DRM de rendu ; sans eux la session KWin ne démarre pas.

## 4 bis. VM Windows de test
`bash vm/win11.sh install` crée une VM Windows 11 (QEMU/KVM, OVMF Secure Boot, TPM 2.0 émulé par `swtpm`, disque sparse 80 Go dans `vm/state/win11`, git-ignoré) à partir de l'ISO du dossier `iso/` (racine du projet, ignoré par git), installation sans surveillance (`vm/win11/autounattend.xml.tmpl`, compte local jetable au mot de passe aléatoire). Le CD de données (`win11.sh data HOST:PORT USER`) fournit `target.rdp` et les identifiants ; un script fixe (`mstsc-test.ps1`) les enregistre et ouvre `mstsc.exe` à chaque ouverture de session. Pas d'agent de commande à distance : les actions suivantes se font par le clavier via le moniteur QEMU (`sendkey`, `screendump`). La souris QEMU n'a pas fonctionné.

## 4 ter. Mode `--login` avec mot de passe réel
`vm-test --session login` (compte jetable, mot de passe aléatoire) : 8/8 sur 2 exécutions sur 3. Une exécution a échoué sur `audio_tone` (ratio 0,38) ; non reproduite, cause inconnue (soupçon : son de démarrage de Plasma mélangé au signal).

## 4 quater. Tests Windows automatiques (sans humain ni LLM)
`RDP_TEST_PASSWORD=… bash vm/win11.sh test HOST:PORT USER` rejoue un scénario fixe dans la VM Windows et rend un code de sortie (0 = tout passe).
- **ISO** : si elle manque, `win11.sh install` la télécharge dans `iso/` (dossier modifiable par `WIN11_ISO_DIR`, fichier par `WIN11_ISO`) depuis `https://public.trodix.com/iso/windows/` (variable `WIN11_ISO_URL`) et refuse tout fichier dont le SHA-256 diffère de celui de l’ISO testée.
- **Moteur** : le contrôle ActiveX RDP de Microsoft (`mstscax.dll`, moteur de `mstsc.exe`) hébergé dans un formulaire WinForms par `vm/win11/rdp-activex-test.ps1` : aucune fenêtre de confirmation, mot de passe injecté par l'interface COM non scriptable, politique CredSSP par défaut (repli TLS). Pas d'évènements COM sous PowerShell : l'état est lu par sondage (`Connected`).
- **Sans réinstaller ni redémarrer** : `win11.sh snapshot` démarre Windows une fois sur une surcouche qcow2 du disque de base (arrêté proprement), attend que le script écrive `READY` sur le port série, puis `savevm ready`. Chaque `test` fait `-loadvm ready`, remplace le CD de données (`change dat`, paramètres seulement) et le script, qui sondait un `params.txt`, démarre aussitôt. Le snapshot est refait automatiquement si le script ou `win11.sh` changent. Pour QEMU, le disque est en AHCI (NVMe bloque `savevm`) et le NVRAM OVMF en qcow2. Durée mesurée : 24 s par test (38 s avec un démarrage de Windows), dont environ 8 s d'attentes volontaires.
- **Lancement** : `cargo xtask test` ne lance que les tests unitaires ; `cargo xtask test --client` ajoute ce test client (`--server` : test serveur sur la VM Debian 13 ; `--integration` : les deux, indépendants) (compile et lance un serveur `--test-mode` local, appelle `win11.sh test`, vérifie les prérequis et s'arrête avec la liste de ce qui manque). Il n'est pas dans la CI (ni KVM ni ISO).
- **Résultats** : le script écrit une ligne JSON par contrôle sur COM1 (`-serial file`), lue par l'hôte.
- **Contrôles** (8/8 + 2 côté hôte contre `--test-mode`) : `connect`, `heartbeat` (pixels réels lus à l'écran de l'invité), `clipboard_roundtrip` et `clipboard_roundtrip_large` (23 et 65 556 caractères, € / emoji / CJK ; l'écho du serveur est détecté par le numéro de séquence du presse-papiers puis le texte relu depuis un processus fils), `guest_audio_device`, `still_connected_after_audio`, `resize_request_survives`, `desktop_resized`, `disconnect`. Côté hôte : QEMU enregistre la sortie audio de l'invité (backend `wav`, 48 kHz) et `rdp-test-client --analyze-wav` (Goertzel) vérifie le 440 Hz ; un son de référence local de 1000 Hz prouve que la chaîne d'enregistrement fonctionne.
- **Bogue trouvé et corrigé** : avec le format 48 kHz en premier, mstsc jouait le son à 405 Hz (hauteur × 44,1/48) sur un périphérique 44,1 kHz ; en 44,1 kHz le 440 Hz est exact. `crates/rdp/src/sound.rs` propose désormais 44,1 kHz en premier. Le test aurait signalé un faux positif sans le son de référence séparé (1000 Hz), d'où sa fréquence distincte.
- **Redimensionnement** : le serveur n'implémente pas le canal de contrôle d'affichage (taille fixe) ; le test vérifie seulement que la demande n'abat pas la session. Le redimensionnement dynamique reste à écrire.
- **Multi-écran** : non testé, la VM n'a qu'un moniteur (pas de second moniteur utilisable avec le pilote d'affichage de base de Windows). Reste couvert par xfreerdp.

## 5. Reste pour la production
Test sur la VM VMware réelle (KWin sans GPU, `vmwgfx`/llvmpipe) ; mstsc : multi-écran, redimensionnement dynamique (à implémenter), stratégie NLA imposée ; KRDC ; instabilité rare de `audio_tone` en mode login ; politique de connexion concurrente ; NVENC ; redirection USB.

## 4 quinquies. Redimensionnement dynamique (Display Control)

- **Chemin** : `RdpServerDisplay::request_layout` (moniteur principal, taille ajustée paire) → `Shared.resize` → le pump appelle `CaptureSource::resize` (nouveau port, `ResizeUnsupported` par défaut), recalcule le `Layout`, envoie `DisplayUpdate::Resize` (désactivation-réactivation) et **attend** que la boucle client ait redémarré avant d'annoncer `ResetGraphics` + nouvelles surfaces EGFX (sinon mstsc jette l'annonce, mesuré). `AutoEncoder` recrée son codec quand la taille des trames change. `Display::size()` renvoie la taille courante, donc une reconnexion repart à la bonne taille.
- **Couvert** :
  - bureau synthétique (`--test-mode`) : `cargo xtask test --client`, contrôle mstsc `desktop_resized` (un bloc magenta dessiné au coin bas-droit du bureau doit apparaître à la bonne position, le contrôle centrant un bureau plus petit que sa fenêtre) ;
  - **KWin 6.7 réel** (poste Fedora) : `WIN_SESSION=kwin cargo xtask test --client` lance `rdsktp --session kwin` avec `rdsktp-testapp` (qui redessine sa fenêtre et son bloc magenta à la nouvelle taille) ; `desktop_resized` passe, la sortie virtuelle reçoit un mode personnalisé (`kscreen-doctor output.Virtual-0.addCustomMode…` puis `mode`, puis relecture de la géométrie car `kscreen-doctor` renvoie 0 même en cas d'échec). Il faut un `.desktop` autorisant `target/debug/rdsktp` et `rdsktp-testapp` (`X-KDE-Wayland-Interfaces`, voir ADR 004) ; le paquet `.deb` dépend désormais de `libkscreen-bin` ;
  - **mode login** : protocole agent `Resize` / `Resized` (test de roundtrip et de la liaison `AgentLink::resize` avec agent factice : réponse, refus, agent muet), `MultiCapture` délègue quand il n'y a qu'une sortie ;
  - mstsc de la VM Windows contre le serveur **login de la VM Debian 13** (`vm-test --session login --keep`, puis `RDP_TEST_PASSWORD=… WIN_EXPECT_RESIZE=0 bash vm/win11.sh test 10.0.2.2:3390 dev`, la VM Windows atteignant le `hostfwd` de la VM Debian par 10.0.2.2) : connexion, image, presse-papiers, audio passent (8/8).
- **Limite mesurée : KWin 6.3.6 (Debian 13) ne peut pas redimensionner son backend `--virtual`**, par aucune voie :
  - modes personnalisés absents (`kscreen-doctor` 6.3 : « Unable to parse arguments » pour `addCustomMode`, protocole `kde_output_management_v2` v12 sans `set_custom_mode`) ;
  - sorties virtuelles : `zkde_screencast.stream_virtual_output` est refusé (« Could not find output », `QObject::connect(KWin::ScreencastStreamV1Interface, …) invalid nullptr`) car le backend virtuel 6.3 ne sait pas en créer. Un repli par sortie virtuelle a été écrit puis retiré : il ne fonctionnait que sur KWin 6.7 où les modes personnalisés suffisent déjà ;
  - `trixie-backports` ne propose pas de KWin plus récent (6.3.6 seulement) ;
  - redémarrer KWin à la nouvelle taille détruirait les applications de l'utilisateur ; mettre à l'échelle côté serveur ferait ce que la mise à l'échelle du client (smart sizing) fait déjà.
  Recherches : les modes personnalisés du backend `--virtual` n'arrivent qu'avec Plasma 6.6 (KWin MR 8766, sur la base de MR 8534) ; KWin 6.6 exige Qt 6.10 et KF 6.22 (Debian 13 : Qt 6.8.2, KF 6.13), donc pas de rétroportage simple. Tous les écrans `--virtual` de 6.3 ont la même taille (`--output-count`), ce qui exclut un jeu de sorties de tailles différentes.
  **Repli retenu : mise à l'échelle côté session** (`crates/scaling`). `ScalingCapture` tente d'abord le vrai redimensionnement ; s'il est refusé, le bureau garde sa taille native (1920x1080), chaque image est ajustée à la taille demandée par le client (proportions conservées, bandes noires, rééchantillonnage bilinéaire, `ScaleMap` dans `rdsktp-core`) et `ScalingInput` ré-associe la souris. Le repli est installé dans l'agent (mode `--login`, où l'encodage H.264 est fait dans l'agent) et dans `rdsktp --session kwin`. Mesuré sur la VM Debian 13 (KWin 6.3.6) avec mstsc : 9/9 sans `WIN_EXPECT_RESIZE=0` (`desktop_resized`, `resize_request_survives`) ; journal agent : `desktop 1920x1080 scaled to 1024x576`. Sur KWin 6.7.5 (hôte) le vrai redimensionnement passe toujours (`desktop_resized`). Limites : image rééchantillonnée (flou possible sous le seuil d'un facteur 2), CPU de l'agent supplémentaire, texte moins net qu'avec un vrai redimensionnement. Pour un rendu net en production : KWin ≥ 6.6.
- **Pas encore couvert** : plusieurs écrans (demande ignorée) ; mapping de la souris (aucun changement requis tant qu'il y a une seule sortie à l'origine) ; capture DMA-BUF après redimensionnement (non testé).
