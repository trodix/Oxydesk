# 001 — Architecture hexagonale de RustyDesktop (workspace Cargo)

Statut : accepté (M0)

- `crates/core` : domaine + ports (traits). Dépendances légères uniquement.
- Les autres crates sont des adaptateurs qui ne dépendent que de `core`, jamais entre eux
  (vérifié par `ci/check-deps.sh`). `rdp` (adaptateur primaire) et les `bins/*` assemblent.
- Les backends techniques sont des features Cargo ; les adaptateurs de test (capture synthétique,
  presse-papiers mémoire, ...) implémentent les mêmes ports.
- Un port n'est créé que s'il a au moins deux implémentations (réelle ou de test).
- Frontière de sécurité = frontière de crate : seul `rdsktp-authd` dépendra de PAM.

Reste à décider en M1 : version/commit d'IronRDP épinglé (ADR 002).
