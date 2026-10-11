# CPAL 0.15.3 — hôte PS Vita

Branche `vita-0.15.3`, depuis le tag amont `v0.15.3` (commit
`7b21251afde5ff8f2b1b1a2a299d714ee5b10f04`). Implémentation sous la licence
Apache-2.0 de CPAL. Aucun code de VitaForge, siglus_rs ou OpenNOW-vita copié.
Les trois déclarations FFI et les valeurs des enums suivent les en-têtes
[VitaSDK](https://github.com/vitasdk/vita-headers/blob/master/include/psp2/audioout.h).

## Contrat

- `target_os = "vita"` sélectionne l'hôte `Vita`, y compris `default_host()`.
- Un périphérique logique de sortie BGM ; aucun périphérique d'entrée, aucune
  configuration d'entrée, construction d'un stream d'entrée refusée sans panic.
- Sortie exclusivement stéréo entrelacée à **48 000 Hz**, formats CPAL **F32**
  (défaut) et **I16**. `BufferSize::Default` et `Fixed(512)` sont acceptés ;
  toute autre fréquence, taille, disposition ou représentation est refusée.
- Le port reçoit toujours du PCM I16. F32 est borné à [-1, 1], multiplié par
  32768, puis converti avec saturation ; NaN et infinis deviennent du silence.
  I16 est transmis directement. Les buffers sont remis à zéro avant le callback.
- Un worker Rust avec pile de **128 Kio** ouvre
  `sceAudioOutOpenPort(BGM, 512, 48000, STEREO)`. La création attend le résultat
  de l'ouverture et remonte son erreur à l'appelant ; aucun callback ne précède
  `play()`. Une erreur de création du thread est également remontée.
- Le worker appelle le callback CPAL puis `sceAudioOutOutput`, qui est bloquant
  et cadence la production. Aucun sommeil ni allocation de buffer par bloc.
  Les buffers sont alloués une fois sur le tas du worker.
- `play()`/`pause()` utilisent un drapeau atomique ; un worker arrêté en pause
  attend avec `park()` et est réveillé par `unpark()`. La pause conserve le port.
  Un bloc déjà soumis peut encore jouer ; une pause concurrente du callback peut
  consommer puis écarter un bloc. Il n'y a pas de promesse de pause instantanée.
- Une erreur d'output appelle le callback d'erreur et termine le worker.
  `play()`/`pause()` refusent ensuite un stream dont le worker est terminé.
  Le drop demande l'arrêt, réveille et joint le worker ; celui-ci draine le port
  avec `sceAudioOutOutput(port, NULL)` puis appelle `sceAudioOutReleasePort`.
  Si le backend est détruit depuis son propre callback, il ne se joint pas
  lui-même : le worker termine et libère le port à la sortie du callback.

## Rodio / Looking Glass

Le lockfile de Looking Glass sur la branche de départ `vita`, commit
`25c2c790221f8fda21ec6458d1da7d4a1a1d12d7`, contient rodio **0.20.1** et CPAL
**0.15.3**. `Playback::open` conserve le `OutputStream` de
`OutputStream::try_default()` pendant la lecture. Sans périphérique, rodio
renvoie `NoDevice` ; le jeu attrape cette erreur et continue avec le statut
« No audio output available ».

Le jeu fournit un mixeur F32 stéréo à **44 100 Hz**. Rodio construit son mixeur
de sortie à la fréquence du périphérique et adapte les sources avec
`UniformSourceIterator` : cet hôte annonce 48 000 Hz, sans modification de
`src/audio.rs` ni des sources du jeu. L'ouverture effective peut encore échouer
sur matériel (par exemple port BGM déjà occupé), avec le même repli du jeu.

## Latence attendue et limites

512 trames / 48 000 Hz = **10,67 ms par bloc**. Quelques blocs de latence sont
plausibles, mais la latence de bout en bout reste à mesurer : files matérielles,
ordonnancement, temps du mixeur et rééchantillonnage s'y ajoutent. La pile de
128 Kio n'est pas mesurée avec les cas les plus lourds du mixeur Looking Glass.

Les timestamps du callback utilisent `Instant` depuis la création du stream ;
le timestamp de lecture estime un bloc d'avance, sans horloge audio matérielle.
Pas de capture, énumération matérielle, hotplug, reprise automatique après
erreur, changement de configuration, ni contrôle de volume propre à l'hôte.
Un seul port BGM peut être disponible : plusieurs streams simultanés ne sont
pas garantis, même si chacun tente une ouverture et remonte son résultat.

Le paramètre optionnel `timeout` de construction n'est pas utilisé. Les appels
natifs d'ouverture/output/drain ne sont pas annulables ici : si un appel ne
revient jamais, le drop peut attendre. Les erreurs de drain/release ne peuvent
pas être retournées par le destructeur. La libération RAII fonctionne lors
d'un déroulage Rust, mais un panic avec `panic = "abort"` termine le processus.
Les callbacks doivent revenir rapidement et ne pas attendre une action du
thread qui détruit le stream.

## Compilation et validation

Compilation de la bibliothèque seule réussie le 2026-10-11 avec la toolchain
VitaSDK native, Rust nightly-2026-10-11 et la commande :

```sh
export VITASDK="$HOME/vitasdk"
export PATH="$HOME/.cargo/bin:$VITASDK/bin:$PATH"
RUSTFLAGS="-C target-feature=-neon -L native=$VITASDK/arm-vita-eabi/lib" \
  cargo +nightly-2026-10-11 build --lib --locked \
  --target armv7-sony-vita-newlibeabihf -Z build-std=std,panic_unwind
```

Le `Cargo.lock` local généré reste ignoré, comme en amont. La recherche de la
lib statique est explicite pour cette compilation directe ; cargo-vita fournit
les chemins VitaSDK pour le build du jeu. Aucune nouvelle dépendance ajoutée.

**Compilation seule : aucun test console ou émulateur, aucun son entendu.**
La sortie F32/I16, le rééchantillonnage en fonctionnement, la cadence réelle,
les underruns, les pauses, les erreurs, la destruction et les interactions BGM
avec le système restent à tester sur matériel. Aucun asset de jeu nécessaire
pour compiler et aucun asset inclus dans ce fork.
