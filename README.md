# RustyDesktop — serveur RDP headless pour KDE Plasma (Rust)

Serveur RDP (IronRDP 0.13, H.264 via EGFX) pour KDE Plasma Wayland headless, en remplacement de xrdp : bureau synthétique de test, session KWin virtuelle, ou connexion distante avec PAM (`--login`, une session par utilisateur). Presse-papiers, audio, entrées, multi-écran et **redimensionnement dynamique** (Display Control, nombre d'écrans compris).

## Multi-écran
- `--outputs N` (1 à 16) annonce N écrans ; un client mstsc à plusieurs moniteurs voit les siens (patch local d'IronRDP dans `third_party/`, ADR 011).
- Display Control : le serveur suit la disposition complète du client (taille, position, nombre de moniteurs). KWin ≥ 6.7 : modes personnalisés et sorties virtuelles. KWin 6.3 (Debian 13) : nombre de sorties fixé par `--outputs`, chaque sortie mise à l'échelle à la taille demandée (`rdsktp-scaling`).
- Mode `--login` : même comportement, relayé à l'agent par `SetLayout`/`LayoutApplied`.
- Tests : `cargo xtask test --client` avec `WIN_MONITORS=2` (mstsc réel), `cargo xtask vm-test --session login --outputs 2 --multi-resize` (`RDP_TEST_PASSWORD` : mot de passe de test quelconque, posé sur le compte de la VM), `rdp-test-client --multi-resize` (`--max-monitors N`, `--no-identity-blocks`).

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
