# Couverture de tests

`ci/coverage.sh` mesure la couverture du workspace (instrumentation de la source par LLVM, via `cargo-llvm-cov`) et produit un
rapport HTML à parcourir (`target/coverage/html/index.html`), un fichier lcov et le tableau par fichier. La CI le publie en
artefact (job `coverage`) sans jamais bloquer une fusion.

```
cargo install cargo-llvm-cov --locked && rustup component add llvm-tools   # une fois
ci/coverage.sh                    # COVERAGE_OPEN=1 ouvre le rapport ; COVERAGE_BRANCH=1 ajoute les branches (nightly)
```

Sont exclus : les copies patchées d'IronRDP (`third_party/`), l'outillage de test (`xtask`, `bins/bench`, `bins/rdp-test-client`)
et les tests d'intégration eux-mêmes.

## Politique

Un pourcentage mesure le code **exécuté** par les tests, pas la qualité de leurs vérifications : la vraie question est
« quels comportements importants restent sans test ? ».

| Type de code | Repère |
|---|---|
| Logique métier, calculs, règles de validation | 90–100 % |
| Parseurs, protocoles, gestion des erreurs | 90–100 %, en regardant les branches |
| Application dans son ensemble | 75–85 % (repère global : 80 %) |
| Raccordement, démarrage, services externes | selon le risque : tests d'intégration ciblés |

- Le repère global de 80 % n'est pas une barrière : il ne bloque pas une correction utile pour quelques dixièmes de point.
- Exigences fortes sur les comportements critiques : succès, erreurs, limites, reprises (déconnexion pendant un transfert,
  paquet mal formé, reconnexion après coupure).
- Toute correction de bug vient avec un test qui le reproduit, quand c'est praticable.
- Les zones non couvertes sont revues pour décider **consciemment** lesquelles méritent un test.
- Couverture de lignes (cette ligne a-t-elle été exécutée ?) et de branches (chaque issue d'une condition a-t-elle été
  exercée ?) : pour le code plein de conditions, les branches renseignent mieux (`COVERAGE_BRANCH=1`, nightly). Même 100 % ne
  garantit ni les combinaisons ni les valeurs limites.

## État au 2026-10-17 (lignes : 74 % ; régions : 74 % ; fonctions : 74 %)

Mesure initiale : 61,8 % de lignes. Ce que le pourcentage cache, à traiter par risque plutôt que par chiffre :

- **Protocole et métier, bien couverts (≥ 90 %)** : `core` (layout, débit, imprimantes, throttle, config, drive), `reconnect`,
  `agent-proto`, `authd-proto`, `remote/{drives,link}`, `spool`, `clipboard/{fuse,local_files}`, `encode/auto`.
- **Couverts à 80–90 %** : `rdp/{drive,clipboard,clipfiles,dib}` (faux client RDPDR/CLIPRDR en mémoire), `drivefs`,
  `remote/{ports,printers}`, `capture/multi`, `bins/rdsktp-agent/printers`.
- **À renforcer** : `rdp/server.rs` (75 %), `rdp/pump.rs` (65 % : boucle de capture liée au runtime IronRDP),
  `session/lib.rs` (50 % : le démarrage lance un vrai KWin ; les parties pures — version, processus périmés — sont testées),
  `bins/authd/src/main.rs` (54 %), `bins/rdsktp/src/main.rs` (47 %, `main` et accept-loop), `input/xkb` (47 %, dépend de libxkbcommon).
- **Non mesuré par `cargo test`** : `bins/rdsktp-agent/{main,drives}` et `bins/rdsktp-testapp` (0 %, processus de session testés
  en VM), à décider si l'extraction d'une logique pure justifie un test unitaire.
- **Raccordement, couvert par les tests en VM (mstsc, FreeRDP, mode login) et non par `cargo test`** : `bins/rdsktp`,
  `bins/rdsktp-agent`, `crates/remote/{lib,ports}.rs`, `crates/clipboard/wayland.rs`, `crates/audio/pulse.rs`,
  `bins/authd/src/pam.rs`, `bins/rdsktp-cups-backend`. Les mesurer demanderait de collecter la couverture des processus lancés
  en VM ; à décider si le gain le justifie.
