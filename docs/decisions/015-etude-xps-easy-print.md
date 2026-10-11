# ADR 015 : modes d'impression par imprimante (AUTO, RAW, XPS), chemin PostScript/PDF → XPS

## Contexte
L'ADR 014 envoie au client des travaux **bruts** : le langage (PostScript, PCL, texte) était déduit du nom du pilote Windows, avec PostScript par défaut. C'est une supposition qu'une imprimante Windows n'est pas tenue de satisfaire : un pilote GDI seul, « Microsoft Print to PDF » ou « XPS Document Writer » refuse un travail brut.

Le protocole connaît la voie XPS ([MS-RDPEPC]) : le client annonce `RDPDR_PRINTER_ANNOUNCE_FLAG_XPSFORMAT` (0x10) ; le serveur envoie `DR_PRN_USING_XPS` (composant `RDPDR_CTYP_PRN` 0x5052, paquet 0x5543, `PrinterId`, `Flags` ignorés ; ce n'est pas un IRP, il n'y a pas de réponse) **avant** le premier `Write` ; les travaux sont alors des documents XPS que le pilote Windows du client rend pour l'imprimante. « Easy Print » complet ([MS-RDPEXPS] : canaux dynamiques `TSVCTKT` et `XPSRD`, tickets d'impression et interface du pilote) est un étage au-dessus, non nécessaire au transport d'un travail.

## Décision

### Trois modes, choisis par imprimante
`--printer-mode SPEC` (défaut `auto`) ; `SPEC` est une liste séparée par des virgules d'entrées `MODE` (défaut) ou `MOTIF=MODE` (`*` remplace n'importe quelle suite, insensible à la casse ; la première règle qui correspond l'emporte). Exemple : `--printer-mode "xps,HP*=raw:pcl,Zebra=raw:text"`.

| Mode | Règle |
|---|---|
| `raw` | Travaux bruts. **Exige un langage explicitement connu** : celui que le nom du pilote énonce (`PostScript`, mot `PS`, `Imagesetter`, `LaserWriter`, `PCL`, `Text Only`), ou celui donné dans la configuration (`raw:ps`, `raw:pcl`, `raw:text`). Refusé pour un pilote qui produit des documents (PDF, XPS, OneNote, fax). |
| `xps` | Travaux XPS. Exige que le client ait annoncé l'indicateur XPS ; sinon l'imprimante n'est pas redirigée (message explicite). |
| `auto` | Langage énoncé par le nom du pilote et pilote non « document » → `raw` (comportement validé jusqu'ici, conservé) ; sinon, si le client annonce XPS → `xps` ; sinon **aucune devinette** : l'imprimante n'est pas redirigée et le journal indique quoi configurer (`raw:ps`, `xps`…). |

Le format est décidé **une fois, à l'annonce** (fonction pure `resolve_format`, `crates/core/src/printer.rs`). En mode `xps`, `DR_PRN_USING_XPS` part avant que l'imprimante soit offerte au bureau, donc avant tout `Write`.

**Pas de repli après l'envoi d'un travail.** Un travail refusé ou échoué par le client n'est jamais renvoyé dans l'autre format (double impression possible). Pour un travail XPS, le document est converti **entièrement avant** de démarrer le travail chez le client : un échec de conversion ne lui envoie rien.

### Chaîne XPS
- Port `XpsConverter` (domaine) ; adaptateur `GhostscriptXps` (`crates/spool/src/xps.rs`) : `gs -dSAFER -sDEVICE=xpswrite`, délai de 120 s, détection au démarrage (`gs -h` doit citer `xpswrite`). Sans lui, les imprimantes XPS ne sont pas servies (avertissement) ; les imprimantes brutes ne sont pas affectées.
- File CUPS d'une imprimante XPS : modèle PostScript générique (`drv:///sample.drv/generic.ppd`) ; le **socket** de l'imprimante lit le document (PostScript ou PDF, `gs` détecte), le convertit dans le répertoire privé (0700) puis l'envoie par blocs. Un script peut donc aussi écrire un PDF ou un PostScript directement dans le socket. Document limité à 512 Mio.
- Mode `--login` : même décision côté serveur (`--printer-mode` du serveur) ; le format voyage dans `ToAgent::PrinterAdded.format` (0 PostScript, 1 PCL, 2 texte, **3 XPS**) et l'agent fait la conversion dans la session de l'utilisateur (`ghostscript` est recommandé par le `.deb` et installé par `vm/vm.sh deploy`).

### CUPS système, ACL et association à la session
Le CUPS du système n'est pas modifié ; les droits existants sont conservés : file `<utilisateur>-<imprimante>`, `lpadmin -u allow:<utilisateur>`, `printer-is-shared=false`, socket `0600` dans un répertoire `0700` du runtime de l'utilisateur, backend `root:0700` qui refuse tout socket qui n'appartient pas à l'utilisateur du travail. En plus :
- une file est liée à **sa connexion RDP** : un spool par connexion, supprimé à la déconnexion, et son emplacement (`-L`) porte l'identifiant de session `<pid>-<n>` ;
- correction : une imprimante du même nom qu'une précédente arrête d'abord l'ancienne (avant, la création de la nouvelle file était suivie de la suppression de l'ancienne, qui avait le même nom de file).

## Niveaux de preuve
| | Implémenté | Vérifié avec un pilote logiciel | Validé sur imprimante physique |
|---|---|---|---|
| Choix du mode (3 modes, règles par motif, refus explicite) | oui | tests unitaires (table de décision) | non |
| `DR_PRN_USING_XPS` avant tout `Write`, une fois, seulement en mode xps | oui | test de bout en bout du serveur contre le client de test (ordre des paquets) ; mstsc (VM Windows 11) | non |
| PostScript → XPS (`gs xpswrite`) | oui | tests de conversion (2 pages) ; `gs` 10.06 sur l'hôte | non |
| mstsc rend le XPS | oui | « Microsoft PS Class Driver » : le PostScript produit contient 2 pages | non |
| Échec de conversion : aucun travail, pas de repli | oui | test unitaire | non |
| Brut (texte, PostScript, PCL) | oui (ADR 014) | « Generic / Text Only » avec mstsc ; xfreerdp et Remmina avec un PostScript | non |
| Mode login (VM Debian 13) | oui | voir « Résultats » | non |
| Imprimante GDI seule | oui (mode xps) | **non** (aucun pilote GDI seul dans la VM) | **non** |
| Easy Print complet (MS-RDPEXPS) | **non** | non | non |

L'acceptation d'un XPS par mstsc avec un pilote logiciel est encourageante mais ne démontre ni la compatibilité avec des imprimantes physiques, ni un Easy Print complet.

## Réglages (bac, recto-verso, couleur)
Sans MS-RDPEXPS, aucun ticket d'impression du client n'est négocié et rien ne passe par `TSVCTKT`/`XPSRD`. Ce qui part est le contenu des pages. **On ne promet donc pas** que les choix de bac, de recto-verso ou de couleur faits dans la session Linux (options CUPS) arrivent à l'imprimante : le pilote Windows applique ses réglages par défaut. Ce qui est réellement appliqué est ce que CUPS et `gs` mettent dans le document : contenu, format de page, orientation, ordre des pages, copies que CUPS expanse. Les options de finition (recto-verso, bac, couleur/niveaux de gris) sont **à vérifier sur une imprimante physique** (protocole ci-dessous) ; tant que ce n'est pas fait, elles sont considérées comme non transmises. En mode brut, ce que le PPD générique de la file met dans le PostScript ou le PCL va à l'imprimante, sans que le pilote Windows n'intervienne.

## Dépendances et environnements
- `gs` avec `xpswrite` : vérifié sur l'hôte de développement (Ghostscript 10.06.0) ; sur la VM Debian 13 voir « Résultats ». Paquet `ghostscript` recommandé par le `.deb`.
- FreeRDP (xfreerdp, Remmina) n'annonce pas l'indicateur XPS (`xps=false` dans le journal du serveur) : pour eux le mode `xps` est refusé et `auto` choisit le brut si le nom du pilote énonce un langage (FreeRDP annonce `MS Publisher Imagesetter`, PostScript). Un pilote FreeRDP de nom quelconque doit être configuré (`raw:ps`).
- Changement par rapport à l'ADR 014 : un pilote dont le nom n'énonce aucun langage n'est plus pris pour PostScript ; avec mstsc il passe en XPS (`auto`), avec FreeRDP il faut le configurer.
- Double rendu en XPS (CUPS/`gs`, puis le pilote Windows) ; la fidélité sur de vrais documents (polices, transparence) n'est pas mesurée en dehors du protocole ci-dessous.

## Protocole de validation sur imprimante physique
À faire avec un poste Windows (mstsc) et au moins deux imprimantes : une PostScript ou PCL (mode `raw`) et une **GDI seule** (mode `xps`, aucun PostScript/PCL). Pour chacune, depuis une session Linux (LibreOffice et `lp`) :
1. **Texte** : page de texte avec accents (é è ç ñ œ), symboles et chiffres.
2. **Polices** : normal, gras, italique, tailles 6 à 48 pt, une police non latine (cyrillique, CJK), polices embarquées d'un PDF.
3. **Images** : JPEG couleur, PNG avec transparence, dégradé, image en niveaux de gris, ligne de 0,1 pt (finesse).
4. **Plusieurs pages** : 5 pages avec une page en paysage au milieu ; vérifier l'ordre, aucune page manquante ni en double ; un travail de 50 pages.
5. **Finition demandée côté Linux** (recto-verso, bac, couleur/niveaux de gris, copies 3) : noter ce qui est appliqué (attendu : copies oui, le reste à constater).
6. **Robustesse** : déconnexion du client pendant un travail ; imprimante hors ligne ; deux travaux successifs.
Critère de réussite : aucune page manquante ni double impression, texte lisible et correctement encodé, rendu des images conforme. Consigner pour chaque ligne : imprimante, mode, résultat (réussi / échec / écart avec description).

## Résultats (tests exécutés)
- Tests unitaires et d'intégration (`cargo test --workspace`, clippy, fmt, `check-deps`) : verts. Ils couvrent la table de décision des modes, l'analyse de `--printer-mode`, le convertisseur `gs` réel (2 pages, ignoré sans `xpswrite`), l'absence de `begin()` après un échec de conversion, l'ordre `DR_PRN_USING_XPS` avant tout travail, une imprimante non décidable non redirigée, le format 3 du protocole agent et le remplacement d'une file de même nom.
- mstsc, hôte (`cargo xtask test --client`) : brut 16/16 ; XPS (`WIN_PRINTERS=2`) 16/16, le « Microsoft PS Class Driver » rend le XPS en PostScript de 2 pages. « Microsoft Print To PDF » est redirigée en XPS par `auto` (pilote « document »).
- mstsc, mode login (VM Debian 13, Ghostscript 10.05.1 avec `xpswrite`) : brut 14/14 ; XPS 14/14 avec le travail envoyé par `lp` sur la file CUPS de l'utilisateur (3 pages rendues). `resize_request_survives` peut échouer à partir de la deuxième session mstsc d'un même démarrage de la VM (indépendant de l'impression, absent après un `vm-test` neuf) : lancer `ci/mstsc-login-test.sh` une fois par `vm-test`.
- xfreerdp et Remmina (hôte, `ci/clients-test.sh`, et mode login, `ci/clients-login-test.sh`) : verts pour les imprimantes (brut, `auto`) ; `ci/freerdp-login-test.sh` vert. Ils n'annoncent pas l'indicateur XPS.
- Test `clipboard_files_large` (mstsc) : échec intermittent mesuré aussi sur le commit précédent (le serveur reçoit `CB_RESPONSE_FAIL` sur la première plage du fichier de 128 Mio fraîchement écrit) ; le script invité exclut maintenant `C:\rdsktp` de l'analyse de Defender et attend le déverrouillage du fichier, ce qui l'a stabilisé.
