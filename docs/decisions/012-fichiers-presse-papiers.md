# ADR 012 : copier-coller de fichiers (CLIPRDR), gros fichiers compris

## Contexte
Le presse-papiers échangeait du texte et des images. Les fichiers doivent passer dans les deux sens, y compris au-delà de 1 Gio, sans que le serveur ni l'agent n'aient de quoi les stocker ni les tenir en mémoire.

## Décision
- **Rien n'est copié à l'avance.** Une liste de fichiers (`FileList` du crate `core` : entrées `FileEntry` + `FileSource::read(index, offset, len)`) circule, et le contenu se lit **par plages** au moment où une application colle. Côté RDP, c'est le mécanisme normal de CLIPRDR (`FileGroupDescriptorW` + `FileContentsRequest` en `RANGE`, `CB_HUGE_FILE_SUPPORT_ENABLED` pour les positions au-delà de 2 Gio, verrous `CAN_LOCK_CLIPDATA`).
- **RDP -> bureau** : `RemoteSource` (`crates/rdp/src/clipfiles.rs`) demande des blocs de 512 Kio au client et garde le dernier. L'adaptateur Wayland (`crates/clipboard/src/fuse.rs`) monte un système de fichiers FUSE **en lecture seule** (`fuser`, `fusermount3` ; paquet `fuse3`) dont les fichiers sont lus à la demande par la liste ; la sélection Wayland est un `text/uri-list` (avec `x-special/gnome-copied-files` et `application/x-kde-cutselection`) qui pointe dedans. Un gestionnaire de fichiers copie donc en flux depuis le client.
- **Bureau -> RDP** : l'adaptateur lit la sélection `text/uri-list` d'un gestionnaire de fichiers (`local_files.rs`), parcourt les dossiers, et `LocalSource` lit les plages sur le disque. Le thread `cliprdr-files` répond aux `FileContentsRequest` (taille ou plage) pour que la lecture du disque ne bloque pas la connexion.
- **Mode `--login`** : l'agent détient le presse-papiers Wayland, le serveur détient la connexion RDP. Les listes passent par `FilesSet` / `Files`, et les plages par `FileRead` / `FileData` dans les deux sens (`FileExchange` du crate `core`, un identifiant de liste par copie, les deux dernières listes restent lisibles pour qu'une lecture en cours survive à une nouvelle copie).
- **Garde-fous** : fichiers annoncés seulement si `STREAM_FILECLIP_ENABLED` est négocié ; noms validés (`FileEntry::is_valid` : relatifs, sans `..`, ≤ 259 unités UTF-16) des deux côtés ; une lecture d'une liste remplacée échoue tout de suite (génération) au lieu de partir sur une liste que le pair a oubliée ; délai de 60 s par plage ; ce qui est en vol échoue à la déconnexion.
- **Patch `third_party/ironrdp-server`** : une erreur renvoyée par le canal presse-papiers (une plage refusée par la validation de la bibliothèque, par exemple) ne ferme plus la connexion ; elle est journalisée.

## Test
- Unitaires : conversion descripteurs <-> entrées, lecture par blocs (`clipfiles`), `FileExchange`, messages de l'agent, lien agent (`files_flow_both_ways_by_range`), FUSE (lecture à des positions au-delà de 4 Gio sans stockage), URI-list, agent réel (`bins/rdsktp-agent/tests/files.rs`).
- Client de test : `clipboard_files` (7 entrées : fichier vide, tailles impaires, dossiers imbriqués, nom accentué, bloc exact et bloc+1) et `clipboard_files_large` (`--large-file-mib N`, un fichier généré à la volée, vérifié octet par octet en flux, sans stockage). Le fichier fait l'aller-retour client -> serveur -> bureau -> serveur -> client.

## Limites
- Lecture seule côté Linux : les fichiers collés dans le bureau ne sont pas modifiables à la source.
- Les liens symboliques vers des fichiers sont suivis ; ceux vers des dossiers sont ignorés (boucles) ; les fichiers spéciaux (tubes, sockets, périphériques) aussi.
- Un déplacement (couper) est traité comme une copie.
- Non essayé : mstsc, xfreerdp et Remmina avec de vrais fichiers (`FileGroupDescriptorW` suit la spécification ; les clients diffèrent surtout sur les dossiers et les verrous).
