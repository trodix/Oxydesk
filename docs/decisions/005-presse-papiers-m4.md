# ADR 005 : presse-papiers réel (M4)

## Décision
- Adaptateur `rdsktp-clipboard` (feature `wayland`) du port `ClipboardBackend`, via `zwlr_data_control_manager_v1`.
  KWin 6.3 l'expose à un client non privilégié (constaté au spike, ADR 004) : pas d'autorisation `.desktop` nécessaire.
- Une connexion Wayland et un thread dédiés ; `set_text` / `poll_text` ne bloquent pas ; dernier texte gagnant.
- **Reconnaissance de notre propre sélection** par un type MIME privé propre à l'instance
  (`application/x-rdsktp-instance-<pid>-<nonce>`), jamais par comparaison de texte : une autre application
  qui recopie le même texte est un vrai changement (sinon l'oracle d'écho serait masqué).
- Lecture d'une sélection distante : tube + `receive`, lecture dans un thread (plafond 32 Mio), UTF-8 lossy.
- Réception RDP -> bureau : `set_selection` avec les MIME texte usuels. Bureau -> RDP : `poll_text`.
- **Fins de ligne** : CF_UNICODETEXT est en CRLF, Linux en LF. Conversion dans `TextClipboard` (crate `rdp`, protocole),
  pas dans l'adaptateur Wayland. Le client de test, qui partage `TextClipboard`, se comporte donc comme un client Windows.

## Oracle de test
`rdsktp-testapp --clipboard-echo` recopie toute sélection texte reçue (comme une application). Le check
`clipboard_roundtrip` (court Unicode) et `clipboard_roundtrip_large` (64 Kio+, avec retours à la ligne) passent
donc par : client RDP -> rdsktp -> sélection Wayland -> testapp -> sélection -> rdsktp -> client RDP.
Contrôle négatif : sans `--clipboard-echo`, les deux checks échouent (« aucun texte reçu »).

## Limites connues
- Texte seul (CF_UNICODETEXT) ; pas d'images ni de fichiers.
- Pas de sélection primaire (clic milieu).
- La sélection déjà présente au démarrage est transmise au premier client.
- Le thread Wayland n'est pas arrêté explicitement (il se termine avec le processus).
- Non testé avec Klipper.

## Test manuel
xfreerdp 3.31 (+clipboard) vers la VM, Plasma + KWrite : copier-coller hôte -> VM et VM -> hôte validés
par l'utilisateur, avec accents et `€`. Non essayés manuellement : emoji, retours à la ligne, texte > 64 Kio
(couverts seulement par les checks automatisés avec l'agent d'écho).
