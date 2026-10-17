# ADR 016 — Reconnexion automatique après coupure réseau

Date : 2026-10-17. Complète l'ADR 008 (mode login : la session appartient à l'agent et survit aux déconnexions).

## Problème
Après une coupure réseau, la session KWin était intacte mais le client devait retaper son mot de passe, et pouvait même ne pas
être reçu : le serveur traite une connexion à la fois et une connexion coupée sans FIN restait « ouverte » (≈ 15 min de
retransmissions) en tenant l'écoute.

## Décision
1. **Cookie Auto-Reconnect (MS-RDPBCGR 2.2.4.2 et 5.5).** Après chaque connexion acceptée, le serveur envoie dans un
   Save Session Info PDU (Logon Info Extended) un identifiant et 16 octets aléatoires. Sur une erreur réseau, mstsc et FreeRDP
   (`+auto-reconnect`) le rejouent seuls dans le Client Info, avec la preuve `HMAC-MD5(secret, 32 octets nuls)` (TLS : pas de
   client random).
2. **Registre en mémoire** (`crates/rdp/src/reconnect.rs`) : un cookie par utilisateur (un nouveau login remplace
   l'ancien), à usage unique (un nouveau est émis à la reprise), lié à l'adresse du client, valable `reconnect-timeout`
   (défaut 10 s, 0 = désactivé) **à compter de la perte** et non du login. Une déconnexion voulue (fin de connexion sans erreur)
   l'oublie. Comparaison de la preuve en temps constant (`hmac::verify_slice`).
3. **Attache sans mot de passe** : `SessionBroker::attach` → requête `Attach` à `rdsktp-authd`, qui ne l'accorde que pour un
   compte **qu'il a lui-même authentifié par PAM depuis son démarrage**, au-dessus de l'uid minimal, dont l'agent tourne ; il ne
   démarre jamais de session. Un `rdsktp` compromis ne peut donc pas rejoindre une session qu'il n'a pas vu s'ouvrir.
4. **Un client qui réessaie sans cookie valable** envoie un mot de passe vide : refusé sans compter comme échec de login (sinon
   les ~20 essais de mstsc bloqueraient son adresse).
5. **Libérer l'écoute vite** : keepalive TCP et `TCP_USER_TIMEOUT` sur les connexions acceptées (`dead-peer-timeout`, défaut 8 s)
   pour que la coupure soit constatée avant la fin de la fenêtre.
6. **Audio** : les paquets produits pour l'ancienne connexion pendant qu'elle était figée sont jetés à l'acceptation de la suivante,
   et un paquet audio envoyé avant la négociation RDPSND est ignoré au lieu de terminer la connexion (symptôme observé : la reprise
   était acceptée puis fermée aussitôt, « client format not yet received »).

Les deux réglages sont dans `/etc/rdsktp/rdsktp.conf` (`reconnect-timeout`, `dead-peer-timeout`) et en options.

## Limites
- Les fenêtres sont conservées, mais la disposition des écrans est celle du client qui revient.
- L'adresse du client doit rester la même ; sinon le mot de passe est redemandé.
- Le patch de `third_party/ironrdp-server` (cookie, keepalive, audio) n'a pas de test unitaire dans le workspace : validé par la
  coupure réelle décrite ci-dessous.

## Vérification
Unitaires : registre (fenêtre, usage unique, adresse, preuve, remplacement, déconnexion voulue), validateur (cookie → reprise,
fin voulue, pas de session à rejoindre, mots de passe vides, désactivation), décision d'attache d'`authd`, keepalive.
Réel (VM Debian, FreeRDP `+auto-reconnect`) : le flux du client est coupé dans la VM par `iptables` et réinitialisé côté client ;
le serveur constate la perte après ≈ 8 s, FreeRDP se reconnecte seul, `authd` journalise « session rejoined », le même
`kwin_wayland` continue (même PID) et les trames reprennent. **mstsc** (VM Windows 11, contrôle ActiveX) : même coupure (reset du flux par un proxy + paquets de l’ancien flux jetés dans la VM) ; mstsc se reconnecte seul, sans mot de passe, `kwin_wayland` inchangé (`ci/mstsc-reconnect-test.sh`).
