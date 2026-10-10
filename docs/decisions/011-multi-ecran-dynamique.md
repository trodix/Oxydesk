# ADR 011 — Multi-écran et nombre d'écrans dynamique

Date : 2026-10-10. Complète l'ADR 006 (multi-écran EGFX) et l'ADR 010 (validation).

## 1. Domaine
- `Layout::from_monitors(&[MonitorSpec])` normalise une disposition cliente : tailles paires, 16 moniteurs au plus, moniteur principal en sortie 0, les autres triés par (x, y), rectangle englobant ramené à l'origine (exigence RDP). `OutputId` = rang.
- `CaptureSource::apply_layout`, `place` et `retire` (ports) ; fabrique d'encodeurs `make_encoder` : le nombre de sorties peut changer en cours de session. La pompe (`crates/rdp/src/pump.rs`) crée ou détruit les encodeurs, réinitialise EGFX (`ResetGraphics` + surfaces) et déclenche une désactivation-réactivation avec la taille englobante.

## 2. Patch local d'IronRDP (`third_party/`)
ADR 006 prévoyait de ne pas patcher IronRDP. Ce n'est plus tenable : `ironrdp-acceptor` annonçait toujours **un** moniteur couvrant tout le bureau dans le Monitor Layout PDU, et mstsc (plein écran, `use multimon:i:1`) n'utilisait alors que son premier écran. `ironrdp-server` n'exposait pas non plus les moniteurs du GCC client.
- `third_party/ironrdp-acceptor` et `third_party/ironrdp-server` (0.10 / 0.13) sont vendorisés, déclarés dans `[patch.crates-io]` et exclus du workspace pour le lint.
- `Acceptor::set_monitors` : liste annoncée dans le Monitor Layout PDU (reprise après réactivation). `RdpServerDisplay::monitors()`.
- `MonitorPolicy` (`set_monitor_policy`) : appelée à la lecture du GCC, **avant** Demand Active, avec les moniteurs du client ; elle renvoie la taille du bureau et la liste annoncée. Le serveur adopte ainsi la disposition du client sans réactivation (une réactivation juste après la connexion laissait des écrans blancs dans mstsc).
- À réexaminer à chaque montée de version d'IronRDP ; l'idéal reste de proposer ces deux points en amont.

## 3. KWin
- `MultiCapture::apply_layout` : ajoute des sorties (fabrique), retire les dernières (`retire` puis abandon), redimensionne toutes les sorties puis les positionne (`kscreen-doctor output.<nom>.position.X,Y`, vérifié sur la ligne `Geometry:`). `resize` à plusieurs sorties passe par `apply_layout` à une sortie.
- Sorties ajoutées : `zkde_screencast.stream_virtual_output` (KWin 6.7, protocole ≥ 2). KWin préfixe le nom demandé de `Virtual-` ; la sortie disparaît avec son flux. Les sorties du démarrage (`--output-count`) ne peuvent pas être détruites : `retire` les désactive (`kscreen-doctor output.X.disable`), `start_named` les réactive.
- Sélection par nom (`wl_output.name`) en plus du rang gauche-droite du démarrage.
- KWin 6.3 : ni modes personnalisés ni sorties virtuelles. La mise à l'échelle (`rdsktp-scaling`) reste mono-sortie ; au-delà de `--outputs`, la disposition est refusée et journalisée.
- Limite observée sur un bureau **statique** : KWin n'envoie une trame que sur dommage. Repasser d'une sortie à 800×600 à 1024×576 (taille d'origine) peut ne produire aucune trame tant que rien ne bouge. La pompe conserve la dernière trame d'une sortie dont la taille ne change pas ; le reste n'est pas corrigé (les bureaux réels animent l'écran).

## 4. Tests
- Client IronRDP (`rdp-test-client --multi-resize`) : Display Control multi-moniteurs + réactivation côté client. Étapes 2 → 3 → 1 → 2 sorties puis retour à la disposition configurée ; contrôle de la taille du bureau et des blocs d'identité du synthétique (`--no-identity-blocks` sur un bureau réel). Mock inclus. Résultats : synthétique 14/14, KWin 6.7.5 de l'hôte 5/5 (bureau animé par `rdsktp-testapp`).
- mstsc réel, 2 moniteurs (VM Windows 11 à deux QXL, `bash vm/win11.sh drivers` une fois, puis `WIN_MONITORS=2 cargo xtask test --client`, avec `WIN_SESSION=kwin` pour KWin) : connexion, seconde sortie sur le second moniteur, disposition suivant les moniteurs du client.
- Non testable avec mstsc : changer la résolution d'un moniteur local en multimon fait quitter le plein écran à mstsc, sans envoi de disposition. Le redimensionnement dynamique multi-écran est donc couvert par le client IronRDP.
- `cargo xtask vm-test --multi-resize --outputs N` passe le scénario au serveur déployé dans la VM Debian.

## 5. Reste à faire
Protocole agent (`SetLayout`/`LayoutApplied`) et mode `--login` multi-écran ; mise à l'échelle par sortie sur KWin 6.3.
