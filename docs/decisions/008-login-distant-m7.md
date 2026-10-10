# ADR 008 — Login distant : authd + agent par utilisateur (M7)

## Décision
Trois processus, trois identités :

| Processus | Identité | Rôle |
|---|---|---|
| `rdsktp --login` | utilisateur `rdsktp` (sysusers), aucun privilège | serveur RDP (IronRDP), reçoit les identifiants, assemble les ports |
| `rdsktp-authd` | root | PAM (`pam_authenticate` + `pam_acct_mgmt`), démarre `rdsktp-agent@<user>.service`, transmet un descripteur connecté à l'agent |
| `rdsktp-agent` | l'utilisateur connecté | session KWin, capture, **encodage H.264**, entrées, presse-papiers, audio |

- **Port `SessionBroker`** (core) : `login(user, password) -> Result<DesktopPorts, LoginError>`. `DesktopPorts` regroupe les ports d'un bureau. Adaptateur : `crates/remote` (`RemoteBroker`), dont les ports sont des mandataires vers l'agent (`AgentLink`). Les ports de bureau directs (`--test-mode`, autologin) sont inchangés.
- **`rdsktp` n'a aucun privilège ni dépendance PAM** (`ci/check-deps.sh` : PAM hors de `bins/authd` = violation). Il ne voit jamais de chemin de socket utilisateur : authd renvoie le descripteur par `SCM_RIGHTS`.
- **Protocoles** : `authd-proto` (une requête = une connexion, JSON borné à 8 Kio, `Debug` du mot de passe masqué, noms d'utilisateur validés `[a-z_][a-z0-9_-]{0,31}` avant tout usage dans un nom d'unité systemd) ; `agent-proto` (binaire à trames `[tag][len u32][payload]`, charge bornée à 32 Mio). Les crates `*-proto` sont des contrats exemptés de la règle « adaptateur → core seulement ».
- **Pourquoi l'encodage dans l'agent** : les frames brutes (1080p ≈ 8 Mo) ne traversent pas la socket ; seul le H.264 (quelques dizaines de Kio) circule, et l'encodeur tourne sous l'identité de l'utilisateur, au plus près de la capture.
- **Reprise sans trou** : côté serveur, `AgentLink` garde une file courte (4 images) par sortie, livrée dans l'ordre : on ne peut pas ne garder que « la dernière » d'un flux inter-dépendant. Premier essai en boîte « dernier gagne » : la gigue entre la cadence de l'agent et celle de la pompe provoquait des trous constants, donc des images clés demandées en boucle (key→pixel p95 ≈ 203 ms, et rupture du flux avec 2 écrans). Au-delà de 4 images en attente (contre-pression EGFX), on jette la file et on demande une image clé à l'agent (`RequestKeyframe`) ; les images inter reçues d'ici là sont ignorées. Un nouveau client EGFX déclenche aussi une demande (`PassthroughEncoder::request_keyframe`). La pompe ne ré-envoie jamais une image inter déjà encodée.
- **Cycle de vie** : la session KWin appartient à l'agent et survit aux déconnexions RDP ; une nouvelle connexion de l'agent remplace la précédente (reconnexion). La pompe de `rdsktp-rdp` n'est plus arrêtée par une erreur de capture : elle attend la session suivante.
- **Identifiants** : `with_credential_validator` d'IronRDP (async ; le login bloquant passe par `spawn_blocking`). Refus → `Reject` ; erreur de backend (authd injoignable, agent mort) → erreur de validateur, journalisée sans le mot de passe. `authd` attend 1 s avant toute réponse négative et répond identiquement pour compte inconnu / mauvais mot de passe / compte interdit (`--min-uid`, défaut 1000).
- **Unité de l'agent** : modèle `rdsktp-agent@.service` (`User=%i`, `PAMName=rdsktp-session` pour une vraie session logind → `XDG_RUNTIME_DIR` et bus de session). `%U` n'est pas exploitable dans ce contexte : l'agent dérive `/run/user/<uid>` lui-même et attend le bus avant de lancer KWin. Socket : `<runtime>/rdsktp-agent.sock`, mode 0600.
- **PAM par `dlopen`** (`libloading`, `libpam.so.0`) : ni en-têtes ni crate `pam` sur le poste de dev ; la conversation PAM répond au mot de passe à toute invite masquée, les réponses sont allouées avec `calloc`/`strdup` comme PAM l'exige.

## Contraintes IronRDP
Le validateur n'est appelé qu'en **mode TLS** (identifiants du ClientInfoPdu). Avec CredSSP/NLA, NTLM exige des identifiants préchargés : impossible pour des comptes dynamiques. Conséquences : `--login` est incompatible avec `--nla-user` ; un client qui impose NLA (mstsc « exiger NLA ») ne pourra pas se connecter. Les identifiants transitent dans le tunnel TLS (certificat auto-signé tant qu'aucun certificat n'est fourni dans `--state-dir`). À réévaluer : NLA avec un validateur côté serveur si IronRDP l'expose un jour.

## Oracles
- Tests unitaires : `agent-proto` (aller-retour de tous les messages, EOF, entrées corrompues), `authd-proto` (aller-retour, mot de passe absent de `Debug`, tailles bornées, descripteur transmis utilisable, validation des noms), `remote` (trou → resynchronisation sur clé, flux sans trou, Hello incohérent, presse-papiers/audio/mort de l'agent), `xtask` (`--session login`).
- Test VM `cargo xtask vm-test --session login` (compte `dev`, mot de passe pris dans `RDP_TEST_PASSWORD`, jamais en argument) : les 7 scénarios habituels via le login PAM réel, plus un **témoin négatif** (`deny-control`) : un mauvais mot de passe aléatoire doit faire échouer le check `connect`.

## Vérifié (VM Debian 13, vrai PAM, compte `dev`)
| Mode | Résultat | key→pixel p95 |
|---|---|---|
| `--session login` | 7/7 + témoin négatif | 123,6 ms |
| `--session login --outputs 2` | 8/8 (multi_screen) + témoin négatif | 101,7 ms |
| `--session kwin` (régression, chemin direct) | 7/7 | 68,5 ms |
| `--session synthetic` (régression) | 7/7 | 37,9 ms |

Surcoût du chemin login ≈ 50 ms : deux boucles cadencées à 30 Hz en série (agent, pompe) plus le saut de socket. Pistes : réveiller la pompe à l'arrivée d'une image de l'agent, ou sonder l'agent plus vite que le serveur. Audio à 2 écrans : ratio 0,77 / continuité 0,91 (seuils passés mais plus bas qu'à 1 écran : charge CPU de la VM).

Bugs trouvés par ces tests (corrigés) : `ProtectHome=yes` sur authd masque aussi `/run/user` (socket de l'agent injoignable) ; KWin n'accorde la capture qu'à un exécutable ayant son `.desktop` (ADR 004) : `rdsktp-agent.desktop` ajouté ; `Requires=`/`After=` du drop-in de test dans la mauvaise section ; le canal EGFX est créé *avant* la validation des identifiants (ne pas le réinitialiser à l'installation de la session) ; un « refus » est observé côté client par l'absence d'image (le handshake RDP aboutit avant la validation).

## Optimisation de latence (mesurée sur la VM, p95 key→pixel, 60 échantillons, 2 passes)
Cause du surcoût : deux étages cadencés à 30 Hz en série (agent, pompe). Port `CaptureSource::set_waker` + `core::wake::Wake` : la capture (KWin ou agent distant) réveille le consommateur à chaque frame.

| Scénario | p95 |
|---|---|
| direct, pompe cadencée | 68 / 69 ms |
| direct, pompe réveillée | 54 / 54 ms |
| login, agent + pompe cadencés | 102 / 102 ms |
| login, pompe réveillée seule | 73 / 74 ms |
| login, agent réveillé sans plafond + pompe réveillée | 88 / 60 ms (instable) |
| **login, agent réveillé, plafond 16 ms + pompe réveillée (retenu)** | 60 / 66 ms |
| login, agent sondé à 4 ms + pompe réveillée | 66 / 66 ms |

Défauts retenus : pompe pilotée par la capture (`RDSKTP_PUMP=tick` pour revenir à la cadence fixe), agent `--loop event --min-interval-ms 16` (≤ 60 images/s). Contrôle final : kwin 53,7 ms ; synthetic 26,0 ms ; login 55,5 ms ; login 2 écrans 77,9 / 74,2 / 69,5 ms, 7/7 (8/8 à 2 écrans). Audio à 2 écrans : un run à ratio 0,73 (seuil passé), les autres à 1,00.

## Non vérifié / suite
Voir le rapport de jalon : VMware de production, NLA, plusieurs utilisateurs simultanés (le serveur RDP traite une connexion à la fois), arrêt de l'agent à la déconnexion/inactivité (la session persiste tant que l'unité tourne), SELinux (Fedora ≠ Debian), durcissement systemd de l'agent (`ProtectSystem`… non appliqué pour ne pas casser KWin), limitation du débit au-delà du délai de 1 s, pas de journal d'audit dédié.
