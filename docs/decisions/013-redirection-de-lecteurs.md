# ADR 013 : redirection de lecteurs (RDPDR), dossiers du client montés dans la session

## Contexte
Un client RDP peut partager des dossiers (mstsc : « Lecteurs locaux », xfreerdp : `/drive:`, Remmina : « Partager un dossier »). Le but : les voir dans la session Linux comme des dossiers ordinaires, en lecture et en écriture, avec des fichiers de plusieurs gigaoctets, sans stockage intermédiaire.

## Décision
- **Port** (`crates/core/src/drive.rs`) : `RemoteDrive` (bloquant, par chemin relatif séparé par `/` : `stat`, `list`, `open`/`read`/`write`/`flush`/`close`, `mkdir`, `remove`, `rename`, `truncate`, `set_modified`, `space`) et `DriveHost` (`added`/`removed`, ce que l'adaptateur RDP appelle). Aucune dépendance au protocole ; `DriveError` se traduit en errno côté FUSE.
- **Adaptateur RDP** (`crates/rdp/src/drive.rs`) : `DriveBackend` implémente `RdpdrServerBackend` ; `RdpDrive` traduit chaque opération en IRP (`IRP_MJ_CREATE`, `READ`, `WRITE`, `QUERY_INFORMATION`, `SET_INFORMATION` pour la taille et le renommage, `DIRECTORY_CONTROL`, `QUERY_VOLUME_INFORMATION`, `CLOSE`) et attend la réponse (délai 30 s). Les lectures sont découpées en IRP de 256 Kio et rebouclées jusqu'au bloc demandé : un retour court vaut fin de fichier pour le noyau. Les suppressions demandent `FILE_DELETE_ON_CLOSE` et `FileDispositionInformation` (un dossier non vide est refusé avant). Les dossiers se lisent une entrée par requête jusqu'à `STATUS_NO_MORE_FILES`. `FlushBuffers` n'est pas envoyé (chaque écriture est terminée quand le client répond).
- **Corrélation requête/réponse** : le backend IronRDP ne reçoit pas d'identifiant de requête. Patch `third_party/ironrdp-rdpdr` : `ServerEvent::Rdpdr(Option<u64>, message)` porte une étiquette, `RdpdrServer::set_next_tag` la dépose avant `send_message`, et le hook `on_request_sent(completion_id, tag)` associe l'étiquette à l'identifiant de complétion. Autant de requêtes que l'on veut peuvent être en vol.
- **`PAKID_CORE_USER_LOGGEDON`** : le serveur l'envoie après la réponse de capacités ; sans lui, les clients Windows n'annoncent leurs lecteurs qu'après une connexion interactive et rien ne se monte.
- **Jamais sur la boucle tokio** : les appels bloquants du port tournent sur un thread dédié (`drive-host` pour `added`/`removed`) et sur les threads FUSE ; la déconnexion libère le backend, ce qui réveille les attentes en erreur et démonte les lecteurs.
- **FUSE lecture-écriture** (`crates/drivefs`, crate `rdsktp-drivefs`, `fuser` 0.18, `fusermount3`) : table d'inodes avec compteurs `forget`, cache de listing de 2 s utilisé par `lookup`/`getattr`, durée de validité des attributs 1 s, quatre threads. Un lecteur apparaît sous `<répertoire>/<nom>` ; le nom est nettoyé par `directory_name` (`C:` → `C`, pas de séparateurs, unicité suffixée).
- **Où** : `--drives-dir DIR` ; par défaut en `--session kwin` : `$XDG_RUNTIME_DIR/rdsktp-drives` ; `off` désactive. Absent en `--test-mode` sauf demande explicite. En `--login`, c'est l'agent (qui tourne sous l'utilisateur) qui monte : `$XDG_RUNTIME_DIR/rdsktp-drives`.
- **Mode `--login`** : le serveur garde la connexion RDP et l'agent monte le FUSE. Protocole agent : `ToAgent::DriveAdded`/`DriveRemoved`/`DriveReply`, `FromAgent::DriveCall` (opération `DriveOp`, résultat `DriveResult`, 13 opérations, tags 17 à 19 et 14). `AgentLink` exécute chaque appel sur un thread (la lecture du thread du lien ne bloque jamais), refuse les lectures de plus de 1 Mio, et l'agent expose `ProxyDrive` (un `RemoteDrive` dont chaque appel est un aller-retour, délai 60 s) au même `DriveMounts`. À la fin de la connexion, tout est démonté.

## Test
- Unitaires : port (`directory_name`, `is_valid_path`), protocole agent (aller-retour de toutes les opérations, compteur mensonger refusé), lien agent avec un faux lecteur (`drives_are_announced_to_the_agent_and_served_on_its_calls`), FUSE réel avec un `RemoteDrive` sur `std::fs` (`crates/drivefs`).
- Bouclage en mémoire (`bins/rdp-test-client/tests/drive_loopback.rs`) : serveur `RdpdrServer` + `DriveBackend` face à un vrai client `Rdpdr` d'IronRDP adossé à un dossier (`drive_client.rs`) : lecture, écriture, création, renommage, suppression, dossier non vide, noms accentués.
- Bout en bout : scénario `drives` du client de test (`--drive-share DIR --drive-mount DIR`) : fichiers dans les deux sens (`--drive-mib`, 64 Mio ≈ 180 Mio/s en lecture et 110 Mio/s en écriture en synthétique), renommage, `mkdir`/`rmdir`, suppression, erreur propre sur un fichier absent. `bash ci/drives-test.sh` le lance contre `rdsktp --test-mode` et vérifie que rien ne reste monté ensuite. `ci/clients-test.sh` fait vérifier les fichiers depuis l'hôte pendant que xfreerdp et Remmina (conteneur) restent connectés.

## Limites
- Seuls les lecteurs de type fichiers sont acceptés (pas d'imprimantes, de ports ni de cartes à puce). La casse est celle de Linux : deux fichiers qui ne différent que par la casse coexistent côté bureau, pas côté Windows.
- Pas de verrous (`lock`), de liens, ni d'attributs étendus ; les droits Unix sont synthétiques (0755/0644, 0555/0444 pour un lecteur en lecture seule).
- Un client qui se déconnecte pendant une écriture laisse un fichier tronqué ; les opérations en vol échouent avec `EIO`.

## Particularités des clients (constatées)

- **Windows (mstsc)** : annonce ses lecteurs après `PAKID_CORE_USER_LOGGEDON` ; refuse avec `STATUS_INVALID_PARAMETER` un `Create` dont `SharedAccess` vaut `0xFFFFFFFF` (`SharedAccess::all()` de la crate, à bits ouverts) : le serveur envoie lecture | écriture | suppression ; répond `STATUS_INVALID_PARAMETER` pour un chemin absent (traduit en « introuvable ») ; ne répond jamais à `FlushBuffers` ; applique le motif de listage dans le dossier ouvert (`\*`).
- **FreeRDP (xfreerdp, Remmina)** : nom du lecteur annoncé en C-string ASCII (UTF-16 chez Windows) ; résout le motif de listage depuis la racine du lecteur (`\dossier\*`) ; répond `STATUS_NOT_SUPPORTED` à `FlushBuffers` ; ne supprime que sur `FileDispositionInformation` ; Remmina nomme le lecteur d'après le dossier partagé. Le serveur essaie donc le motif nommant le dossier, puis `\*`.

## Résultats

- xfreerdp et Remmina (`bash ci/clients-test.sh`) : fichiers dans les deux sens, renommage, dossiers, suppression contrôlés depuis l'hôte, toutes les autres vérifications inchangées.
- mstsc (`cargo xtask test --client`, VM Windows 11) : les 15 contrôles passent, dont `drives_roundtrip` / `drives_host` (lecture de 3 Mio et écriture de 2 Mio à travers le montage, `mkdir`, suppression, renommage vus des deux côtés).
- Client IronRDP (`bash ci/drives-test.sh`) : 64 Mio dans chaque sens.
- **Non testé en conditions réelles** : le relais de l'agent en mode `--login` (couvert seulement par les tests unitaires du protocole et du lien ; `cargo xtask vm-test --session login` n'a pas été lancé).
