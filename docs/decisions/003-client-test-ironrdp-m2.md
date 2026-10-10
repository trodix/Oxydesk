# 003 — Backend IronRDP du client de test (M2)

Statut : accepté (M2).

## Décision
`bins/rdp-test-client` (feature `ironrdp`) pilote la connexion avec les crates de bas niveau
(`ironrdp-connector`, `-blocking`, `-session`, `-egfx`, `-dvc`, `-cliprdr`, `-input`) dans un thread
worker. Il n'utilise **pas** `ironrdp-client`.

## Pourquoi pas `ironrdp-client` 0.1.0 (lu dans le source, pas seulement la doc)
- Bibliothèque seule (le GUI est `ironrdp-viewer`), sortie par `mpsc::Sender<RdpOutputEvent>` borné.
- **Aucun EGFX** : seuls DisplayControl et Echo sont attachés en DVC (`rdp.rs`). Notre serveur n'émet que de
  l'EGFX H.264 : ce client ne recevrait aucune image.
- **Pas de point d'injection CLIPRDR** : la factory est construite en interne depuis `ClipboardType`
  (`Enable | Disable | Stub`) ; aucun backend en mémoire possible.
- `ironrdp-tls` (rustls, certificat non vérifié) pourrait remplacer notre petit code TLS ; gain faible, reporté.

## Constats techniques
- Le décodeur EGFX client est à fournir (`H264Decoder`) ; le nôtre utilise libopenh264 chargée à l'exécution et
  accepte Annex-B (ce que le serveur envoie) ou AVC à préfixe de longueur.
- rustls : `ring` et `aws-lc-rs` coexistent dans l'arbre ; le provider doit être choisi explicitement (serveur
  `install_default`, client `builder_with_provider`), sinon panique à l'exécution.
- Mode TLS seul par défaut ; CredSSP/NLA activé côté client si `RDP_TEST_PASSWORD` est défini.
- Le certificat serveur n'est jamais vérifié : usage tests locaux uniquement.

## Résultats mesurés (synthétique, libopenh264 2.6)
- Local 1280x720 et VM Debian 13 1920x1080 : 6/6 checks, key→pixel p95 ≈ 35 ms, ~34 fps.
