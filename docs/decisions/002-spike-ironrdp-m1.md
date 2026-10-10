# 002 — Spike IronRDP (M1) : verdict et version épinglée

Statut : **GO provisoire pour IronRDP**, avec 3 points ouverts (multi-écran, presse-papiers, mstsc).
Date : 2026-10-10. Poste : Ryzen 5 5600X, Fedora 44. Outils : `bins/spike`, `bins/bench`.

## Versions épinglées (crates.io)
`ironrdp-server =0.13.0` (features `egfx`, `helper`), `ironrdp-egfx =0.3.0`, `ironrdp-dvc =0.8.0`,
`ironrdp-cliprdr 0.7.0`, `ironrdp-rdpsnd 0.9.0`. MSRV d'ironrdp-server : 1.89 (toolchain du projet : 1.99).
`openh264 0.9.8` en feature `libloading` (libopenh264 chargée à l'exécution).

## Résultats mesurés
| Critère (§9) | Résultat |
|---|---|
| 1. H.264 AVC420 via EGFX | **OK avec xfreerdp 3.31** (TLS) : EGFX V8.1 + AVC420 négocié, ~30 fps soutenus, 1 frame abandonnée par backpressure sur ~430. Pas vérifié visuellement (pas d'écran virtuel) ; KRDC non testé. |
| 2. Encodage 1080p / 720p | Contenu de bureau (synthétique) : **4,2 ms / 1,6 ms** (p50, 1 thread). Pire cas bruit total : **32,6 ms / 13,9 ms** (1080p = tout le budget 30 fps). Latence touche→pixel non mesurée (attend le client IronRDP, M2). |
| 5. NLA/CredSSP | **OK avec xfreerdp** : bon mot de passe → session, mauvais → `AUTHENTICATION_FAILED`. `mstsc` non testé (pas de Windows). |

## Constats de lecture de code (non testés)
- **NLA exige des identifiants préchargés** (NTLM) : incompatible avec un login PAM arbitraire. Pour M7 :
  mode TLS + `CredentialValidator` (validation PAM via `rdsktp-authd`), à confronter au refus des clients
  qui imposent NLA. À trancher en M7.
- **Multi-écran** : `RdpServerDisplay::size()` n'expose qu'un `DesktopSize`, et l'acceptor envoie un
  `MonitorLayout` à **un seul** écran. `ironrdp-egfx` gère en revanche plusieurs surfaces,
  `map_surface_to_output(x, y)` et `resize_with_monitors` (≤16). Hypothèse : multi-écran faisable via EGFX
  seul ; sinon patch amont de l'acceptor. À tester en M5 avec un client multi-écran.
- **Presse-papiers** : `CliprdrBackend`/`CliprdrBackendFactory` présents côté serveur. Gros volumes non testés (M2/M4).
- **Audio** : `RdpsndServerHandler` (formats, choix, start/stop) ; `AudioFormat` générique (PCM, et Opus
  possible selon `WaveFormat`). Non testé avec un vrai client (M6).
- **UDP** : pas de support. **Auto-reconnexion** : rien dans `ironrdp-server`.

## Encodeur
- `openh264` en chargement dynamique : pas de g++/nasm requis. libopenh264 est en **2.6.0 sur Fedora 44 et
  Debian 13** (`libopenh264-8`). Le crate vérifie normalement un hash Cisco : on utilise
  `from_blob_path_unchecked` (builds de distribution). Licence/brevets H.264 : le paquet Debian/Fedora
  est un build source ; à valider côté juridique si distribution.
- x264 (crate `x264 0.5.0`, dernier release 2022) écarté pour l'instant : absent de Fedora, crate ancien.

## Points ouverts / suite
1. Latence touche→pixel et presse-papiers : mesurer en M2 avec le backend IronRDP du client de test.
2. Tester sur la VM avec flags CPU de la prod (`lscpu` prod à fournir) : la VM de test a AVX2, pas AVX-512.
3. Provisioning : `libpipewire-0.3-0` n'existe pas sous ce nom sur Debian 13 (probablement `…-0t64`) ; à corriger.
