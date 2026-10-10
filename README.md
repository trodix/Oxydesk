# RustyDesktop — serveur RDP headless pour KDE Plasma (Rust)

État : **M0** (fondations). Aucun serveur n'est encore implémenté.

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
