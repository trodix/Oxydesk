# ADR 007 — Audio sortant (M6)

## Décision
- Port `AudioProvider::open(AudioParams) -> AudioSource` (core) : la source est ouverte **au format négocié** avec le client (PCM 16 bits, 48 kHz ou 44,1 kHz, stéréo) ; `AudioSource::read_exact` bloque jusqu'à remplir un paquet.
- Adaptateurs (`crates/audio`) : `ToneProvider` (sinus 440 Hz cadencé en temps réel, mode `--test-mode`) et `PulseCapture` (capture du moniteur d'un puits nul PipeWire `rdsktp`, créé au démarrage de la session et mis par défaut).
- `PulseCapture` v1 passe par les outils de l'API PulseAudio de `pipewire-pulse` (`pactl`, `parec`, paquet `pulseaudio-utils`) plutôt que par une liaison native libpipewire : plus simple à vérifier. Une capture native (crate `pipewire`, déjà utilisé pour l'écran) reste possible derrière le même port.
- `rdsktp-rdp` : `SoundFactory` (RDPSND d'IronRDP, `with_sound_factory`) ; à la négociation, un thread lit des paquets de 20 ms et les envoie en `ServerEvent::Rdpsnd(Wave)`. PCM uniquement ; Opus non fait (pas de besoin client avéré).
- `--audio tone|pulse|off` (défaut : `tone` en `--test-mode`, `pulse` en session KWin).

## Constats
- `ironrdp-rdpsnd` côté client envoie l'intersection des formats dans un ordre de `HashSet` : le `format_no` des paquets n'est pas relisible avec plusieurs formats. Le client de test n'en propose donc qu'un (48 kHz). Les clients réels (FreeRDP, mstsc) n'ont pas ce défaut.
- Paquets non consommés = périmés : le scénario audio vide d'abord la file et se base sur l'instant de réception.
- Aucun chemin d'horloge partagée avec la vidéo : pas de synchronisation lèvres (lip-sync) en M6.

## Oracle
Scénario `audio_tone` : ≥ 1 s de PCM continu (reçu / temps réel > 0,9) dont ≥ 50 % de l'énergie est à 440 Hz (Goertzel). Contrôles négatifs : tonalité à 1 kHz (mock, test unitaire) et session KWin sans lecture (silence → échec, vérifié sur la VM).

## Vérifié (VM Debian 13)
`--session synthetic` et `--session kwin` : 7/7, `audio_tone` ratio 1,00, continuité ≈ 1,0 (kwin : testapp `--tone` → `paplay` → puits `rdsktp` → `parec` → RDPSND).

## Test manuel
xfreerdp 3.31.1 (`/sound:sys:pulse`) avec Firefox ESR sur YouTube dans la session KWin : son entendu, validé par l'utilisateur.

## Non vérifié / suite
Latence audio mesurée ; KRDC/mstsc audibles ; VMware de production (PipeWire/pipewire-pulse et `pulseaudio-utils` présents ?) ; Opus ; entrée micro (RDPEAI) ; volume ; resynchronisation après rafale.
