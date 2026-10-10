# ADR 006 — Multi-écran (M5)

## Décision
- N écrans = N sorties `kwin_wayland --virtual --output-count N` (même taille, côte à côte : Virtual-0, Virtual-1…), **un flux de capture et un encodeur H.264 par sortie**, **une surface EGFX par sortie** mappée à l'origine de la sortie.
- Le bureau RDP est le rectangle englobant (`Layout::desktop_size`) ; `Display::size()` le renvoie. Pour N > 1, `resize_with_monitors` émet un `ResetGraphics` portant la liste des moniteurs (le premier `ResetGraphics` automatique d'ironrdp-egfx 0.3.0 n'en porte aucun), puis les surfaces sont créées.
- Index logique d'une sortie = rang gauche→droite (position `wl_output.geometry`), pas l'ordre d'annonce du registre.
- La souris reste en coordonnées absolues du bureau : les sorties étant jointives à partir de (0,0), elles coïncident avec les coordonnées globales de KWin.

## Constats
- Le serveur IronRDP 0.13 n'annonce qu'un `DesktopSize` ; le multi-écran passe donc par EGFX (surfaces + moniteurs du ResetGraphics), sans patch amont.
- `ironrdp-egfx` côté **client** : un seul `H264Decoder`, qui ne reçoit pas l'identifiant de surface, alors que chaque surface a son flux P. Deux flux entrelacés dans un décodeur donnent « PDU error ». Contournement dans le client de test (`SurfaceTap` + `PerSurfaceDecoder`). FreeRDP/mstsc ont un décodeur par surface (non vérifié ici).
- `--output-count` de KWin impose des sorties identiques : tailles hétérogènes non couvertes.

## Vérifié (VM Debian 13, virgl)
`xtask vm-test --session kwin --outputs 2` : 7/7 (bureau 3840x1080, blocs d'identité aux bonnes abscisses, key→pixel p95 ≈ 71 ms, ~43 fps). `--outputs 3` : 7/7 (p95 ≈ 104 ms). 1 écran et synthétique : inchangés.

## Test manuel
xfreerdp 3.31.1 (`/size:3840x1080 /smart-sizing`) : 2 écrans affichés côte à côte, puis Plasma sur 2 écrans sans la testapp, souris entre les écrans validée à l'œil par l'utilisateur.

## Non vérifié / suite
xfreerdp `/multimon`, KRDC, mstsc ; souris sur l'écran 2 sans oracle automatique ; Display Control (disposition pilotée par le client) ; tailles hétérogènes ; offsets négatifs ; budget CPU partagé entre encodeurs.
