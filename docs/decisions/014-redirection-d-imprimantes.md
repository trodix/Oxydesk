# ADR 014 : redirection d'imprimantes (RDPDR), imprimantes du client utilisables depuis la session

## Contexte
Un client RDP peut partager ses imprimantes (mstsc : « Imprimantes », xfreerdp : `/printer:NOM[,PILOTE]`, Remmina : « Partager les imprimantes »). Le but : les voir dans la session Linux comme des files CUPS ordinaires (LibreOffice, Firefox, `lp`…), et que le résultat sorte sur l'imprimante du poste de l'utilisateur.

Le protocole ([MS-RDPEPC]) passe par le canal RDPDR déjà utilisé pour les lecteurs (ADR 013) :
- le client annonce chaque imprimante comme un périphérique `RDPDR_DTYP_PRINT` dont les données portent des indicateurs (par défaut, réseau, ASCII), le nom du pilote Windows, le nom de l'imprimante et des « champs en cache » ;
- un travail d'impression est `IRP_MJ_CREATE` (début du travail) puis une suite de `IRP_MJ_WRITE` (les octets) puis `IRP_MJ_CLOSE` (fin) ;
- le client transmet les octets **tels quels** à son imprimante (travail « RAW » côté Windows, `cupsPrintFile` en mode brut côté FreeRDP) : c'est au serveur de produire le langage que l'imprimante comprend.

## Décision

### Ports (`crates/core/src/printer.rs`, hexagonal)
- `PrinterInfo { name, driver, is_default, language }` : ce que le client annonce, avec le **langage d'impression** déduit du nom du pilote (`PrintLanguage` : `PostScript`, `Pcl`, `Text`).
- `RemotePrinter` (bloquant, comme `RemoteDrive`) : `begin(title) -> Box<dyn PrintJob>` ; `PrintJob` : `write(&mut self, &[u8])`, `finish(self)`, `abort(self)`. Aucune dépendance au protocole.
- `PrinterHost` : `added(name, info, Arc<dyn RemotePrinter>)` / `removed(name)`, ce que l'adaptateur RDP appelle (comme `DriveHost`). `DesktopPorts` gagne `printers`.

### Adaptateur RDP (`crates/rdp/src/printer.rs`)
- Le backend RDPDR de l'ADR 013 accepte aussi les périphériques `Print` (la capacité `CAP_PRINTER_TYPE` est annoncée au client). Les données d'annonce sont décodées (UTF-16, ou ASCII si l'indicateur est posé : FreeRDP).
- Un travail = `Create` (chemin vide), `Write` par blocs de 64 Kio avec la position cumulée, `Close`. Un `Write` refusé annule le travail (`abort`). `Create` est envoyé avec un chemin de longueur 0 (les clients refusent tout autre chemin) et la réponse de FreeRDP à un `Create` échoué n'a pas l'octet « Information » : le décodeur le tolère. Les blocs sont envoyés **l'un après l'autre** : la lecture de la source ralentit avec le client (pas de tampon du travail entier).
- Le langage est déduit du nom du pilote : `PS`/`PostScript`/`Imagesetter`/`LaserWriter` → PostScript ; `PCL` → PCL ; `Text Only`/`Generic / Text` → texte ; autre → PostScript par défaut (cas de FreeRDP, qui annonce `MS Publisher Imagesetter`). Les pilotes « Print to PDF » et XPS n'acceptent pas de travail brut : l'imprimante est annoncée et le client refuse le travail (erreur propre dans le journal).

### Côté Linux (`crates/spool`, crate `rdsktp-spool`)
- Pour chaque imprimante, un **socket Unix** `<répertoire>/<nom>.sock` (droits `0600`, répertoire `0700` nommé `rdsktp-printers`, par défaut sous `$XDG_RUNTIME_DIR`). **Une connexion = un travail** : les octets lus sont écrits dans `PrintJob::write`, la fin de flux (EOF) termine le travail (`finish`), et la dernière ligne renvoyée à l'émetteur est `OK` ou `ERROR: <raison>`. Le travail est abandonné (`abort`) seulement si le client RDP ou le lien échoue en cours de route. Un seul travail à la fois par imprimante (verrou), délai d'inactivité de lecture de 120 s. Cette interface suffit à tout script (`socat - UNIX-CONNECT:…`) et sert de couture de test.
- **File CUPS** (`CupsQueues`, optionnel, `--printers`) : `lpadmin -p <utilisateur>-<nom> -E -v rdsktp:<chemin du socket> -m <modèle> -D "<nom> (RDP client)" -o printer-is-shared=false -u allow:<utilisateur>` puis `lpadmin -x` au retrait ; les files restantes d'une session précédente sont supprimées au démarrage (`lpstat -v`). Le modèle suit le langage : `drv:///cupsfilters.drv/…` (PostScript, PCL) ou `drv:///cupsfilters.drv/textonly.ppd` (texte). `lpadmin` n'est accepté par `cupsd` que pour les membres de son `SystemGroup` (`lpadmin`, `wheel` ou `sudo` selon la distribution) : sinon l'avertissement est journalisé et seuls les sockets existent. CUPS convertit donc le travail de l'application dans le langage voulu par l'imprimante du client avant de l'envoyer au backend. La file n'est visible que de l'utilisateur de la session (`-u allow:`), et le nom n'est jamais interprété par un shell (arguments passés directement à `lpadmin`).
- **Backend CUPS** (`bins/rdsktp-cups-backend`, installé dans `/usr/lib/cups/backend/rdsktp`, propriétaire `root`, mode `0700` : CUPS le lance en `root`) : lit le travail sur l'entrée standard (ou le fichier en argument 6), se connecte au socket de l'URI `rdsktp:/chemin` et copie. Il refuse tout chemin qui n'est pas un socket dans un répertoire `rdsktp-printers` appartenant à l'utilisateur du travail (argument 2), pour qu'un administrateur de file ne puisse pas viser un socket arbitraire avec les droits du backend.
- Si CUPS est absent ou si `lpadmin` est refusé, les sockets restent disponibles et un message d'avertissement l'indique : l'absence de CUPS n'empêche pas la session.

### Mode `--login`
Comme pour les lecteurs : le serveur garde la connexion RDP, l'agent (sous l'utilisateur) héberge les sockets et les files CUPS. Protocole agent : `ToAgent::PrinterAdded`/`PrinterRemoved`/`PrintReply`, `FromAgent::PrintCall` (opération `PrintOp` : `Begin`, `Write`, `Finish`, `Abort`). `AgentLink` exécute chaque appel sur un thread et refuse les blocs de plus de 1 Mio ; l'agent expose un `ProxyPrinter` au même `PrinterMounts`.

### Options (`bins/rdsktp`)
`--printers DIR|off` (défaut avec `--session kwin` : `$XDG_RUNTIME_DIR/rdsktp-printers`) ; `--printers-cups on|off` (défaut : files CUPS si `lpadmin` répond) ; `off` ne crée que les sockets. En mode `--login`, l'agent applique ces défauts sous l'utilisateur.

## Test
- Unitaires : décodage de l'annonce (UTF-16, ASCII, champs vides, tronqué), déduction du langage, nom de file sûr, travail par socket (octets exacts, plusieurs travaux, déconnexion en cours), protocole agent (aller-retour des trames), `AgentLink` avec un faux agent.
- Bout en bout : client de test IronRDP (annonce une imprimante, enregistre les octets du travail), `bash ci/printers-test.sh` (travail par socket, puis par un vrai `cupsd` privé, `lp` et le backend).
- Vrais clients : xfreerdp (conteneur avec un `cupsd` dont le backend enregistre le travail), mstsc (VM Windows : imprimante « Generic / Text Only » sur un port fichier, le contenu du fichier est comparé à ce que l'hôte a envoyé).

## Limites
- Un seul sens : session Linux → imprimante du client. Pas d'état d'imprimante (papier, erreurs), ni de lecture du travail en retour.
- Les imprimantes dont le pilote Windows n'accepte pas de travail brut (GDI pur, « Print to PDF », XPS) sont annoncées mais le client refuse le travail.
- Pas de gestion des options de travail (recto-verso, bac, couleur) : ce que CUPS met dans le flux est tout ce que l'imprimante reçoit.
- Les imprimantes ajoutées pendant la session sont prises en compte à la prochaine annonce du client (mstsc les ré-annonce).

## Résultats
- Tests unitaires : annonce (UTF-16, ASCII, champs en cache, tronquée), langages, spool (octets exacts, plusieurs travaux, déconnexion), CUPS (faux `lpadmin`/`lpstat`), protocole agent, lien serveur avec un faux agent, relais complet agent → serveur → imprimante (2,5 Mo) dans un processus.
- `ci/printers-test.sh` : client IronRDP (5 travaux dont 512 Kio) puis `cupsd` privé avec le vrai backend : `lp -o raw` octet pour octet, travail texte filtré par CUPS, socket et file retirés après la déconnexion. Le backend refuse bien un socket hors d'un répertoire `rdsktp-printers`.
- xfreerdp et Remmina (conteneur, file CUPS qui enregistre) : 5 travaux dont 256 Kio, octets identiques.
- mstsc (VM Windows 11, `cargo xtask test --client`, 16/16) : imprimante « Generic / Text Only » sur un port TCP/IP local ; le travail de 100019 octets envoyé par l'hôte arrive identique. Un port fichier n'est pas redirigé par mstsc, et le pilote doit être ajouté (`Add-PrinterDriver`) sous Windows 11.
- Découvertes : le `Create` doit avoir un chemin de longueur 0 ; FreeRDP répond à un `Create` échoué sans l'octet « Information ».
- **Mode `--login` (VM Debian 13)** : `cargo xtask vm-test --session login --keep`, puis `ci/clients-login-test.sh` (xfreerdp et Remmina : dossier monté et lu/écrit, 5 travaux par le socket, 300 000 octets par la file `dev-Recorder` avec `lp -o raw`, file retirée à la déconnexion) et `ci/mstsc-login-test.sh` (mstsc, 14/14 contrôles dont lecteurs, imprimante de 100019 octets et presse-papiers de fichiers de 128 Mio ; vérifications côté hôte par SSH).
- Découvertes du test login : (1) le serveur lisait l'hôte des lecteurs et des imprimantes à la construction du canal, avant la validation du mot de passe : sans NLA, il voyait la session de la connexion précédente, ou aucune ; l'hôte est maintenant résolu à l'annonce de l'appareil. (2) Sous Debian 13, `lpadmin -m drv:///…` a besoin de `cups-driverd`, qui n'est que dans le paquet `cups` (le .deb le recommande, avec `cups-filters`) ; l'utilisateur doit être dans le groupe `lpadmin`. (3) Le .deb installe le backend en `/usr/lib/cups/backend/rdsktp` (root, 0700).
