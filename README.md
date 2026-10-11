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
| Presse-papiers | Texte seul, bidirectionnel, accents et `€`, plus de 64 Kio. |
| Audio | Sortie son (tonalité 440 Hz contrôlée). |
| Reconnexion | Reprise après coupure ou fermeture ; la session survit aux déconnexions. |
| Multi-écran | Jusqu'à 16 écrans ; mstsc à 2 moniteurs contre le bureau synthétique et KWin réel (patch local d'IronRDP). |
| Redimensionnement dynamique | Display Control, nombre d'écrans variable. KWin ≥ 6.7 : modes personnalisés et sorties virtuelles ; KWin 6.3 : mise à l'échelle par sortie. |

### Limites connues
- **KWin 6.3** : nombre de sorties figé par `--outputs`, redimensionnement par mise à l'échelle (bandes noires), disposition refusée tant qu'une sortie n'a pas envoyé de trame.
- **Bureau statique sous KWin** : KWin n'envoie une trame que sur dommage ; après un redimensionnement l'écran peut rester vide tant que rien ne bouge.
- **Presse-papiers** : ni images, ni fichiers, ni sélection primaire ; non testé avec Klipper.
- **NLA en mode login** impossible avec des comptes dynamiques (limite d'IronRDP) : TLS seul.
- **Patch IronRDP local** (`third_party/`) à réexaminer à chaque montée de version.

### Non couvert ou non vérifié
Transfert de fichiers, imprimantes, lecteurs, cartes à puce, microphone, USB ; plusieurs utilisateurs ou clients simultanés ; verrouillage de session et SDDM ; HiDPI ; clients autres que mstsc et IronRDP (FreeRDP, Remmina, mobiles) ; curseur distant ; coût CPU de N encodeurs logiciels ; paquet Debian.

## Plan priorisé
Efforts estimés, pas mesurés.

**P0 — bloquants pour un usage réel**
1. Tests avec FreeRDP et Remmina (connexion, image, clavier, souris, presse-papiers, audio, redimensionnement ; `--login` et `--session kwin`). Effort faible.
2. Verrouillage de session et SDDM : verrouiller, se reconnecter, déverrouiller sans perdre la session. Effort moyen.
3. Durcissement du mode login : limitation des essais par IP et par compte, journalisation des échecs. Effort moyen.

**P1 — fonctions de base manquantes**
4. Presse-papiers d'images, puis fichiers. Moyen.
5. HiDPI. Moyen.
6. Curseur distant (vérifier d'abord s'il est déjà couvert). Faible à moyen.
7. Plusieurs utilisateurs en parallèle. Moyen.

**P2 — qualité et exploitation**
8. Mesure du coût CPU en multi-écran. Faible.
9. Paquet Debian. Moyen.
10. Plusieurs clients sur une même session. Moyen.
11. Proposer le patch IronRDP en amont. Variable.

**P3 — facultatif** : fichiers partagés, imprimantes, microphone, USB, cartes à puce ; NLA en mode login (dépend d'IronRDP) ; abandon du repli de mise à l'échelle quand KWin ≥ 6.7 sera la cible.

Ordre conseillé : 1 puis 6 (vérifications rapides), puis 2 et 3, puis 4, 5 et 7, le reste selon l'usage.

## Commandes
- `cargo test --workspace` — tests unitaires (core, client de test + mock, xtask).
- `cargo xtask test` — tests unitaires seulement (rapide) ; `--server` ajoute le test serveur (VM Debian 13, `vm-test`), `--client` le test client (client Windows mstsc, lent : snapshot QEMU Windows 11, session RDP avec le moteur du client Microsoft : connexion, images, presse-papiers, audio, demande de redimensionnement). `--integration` lance les deux, indépendants. Prérequis : KVM, qemu ; VM Debian créée une fois (`bash vm/vm.sh create`), VM Windows installée une fois (`bash vm/win11.sh install`, avec swtpm).
- `cargo run -p rdp-test-client -- --mock --out /tmp/out` — client de test contre le mock.
- `bash vm/vm.sh create` — télécharge/provisionne la VM Debian 13 (une fois, état dans `vm/state/`).
- `cargo xtask vm-test` — cycle complet build → VM → client → rapport (`artifacts/vm-test/latest.json`).
- `bash ci/check-deps.sh` — vérifie les règles de dépendance hexagonales.

Les overlays qcow2 utilisent des chemins absolus : si le dépôt est déplacé, relancer `vm.sh create`.
Architecture : `docs/decisions/001-architecture-hexagonale.md`.

## Nommage
Le projet s'appelle **RustyDesktop**. Le serveur RDP est le binaire (et service) `rdsktp` ; le démon privilégié sera `rdsktp-authd`.
