<div align="center">

<img src="assets/logo.svg" alt="MyStream" width="128" height="128">

# MyStream

**Partage ton écran avec tes potes, pas les ressources de ton PC.**

Léger · Pair-à-pair · Sans serveur · Open source

[![Licence MIT](https://img.shields.io/badge/licence-MIT-5865F2)](LICENSE)
![Windows](https://img.shields.io/badge/Windows-10%20%2F%2011-9B5CF6)
![Rust](https://img.shields.io/badge/fait%20en-Rust-3BA55D)

*Peer-to-peer screen sharing between friends that barely touches your GPU.*

</div>

---

## 🎮 Pourquoi MyStream ?

Tu lances un partage d'écran pendant une partie et ton jeu se met à ramer. La plupart des apps de discussion font tourner un navigateur complet pour streamer, et elles ajoutent des copies et des traitements graphiques au passage.

MyStream fait une seule chose, bien : **prendre ton écran et l'envoyer à tes amis en dérangeant le moins possible ton PC.** L'image reste sur la carte graphique du début à la fin. L'encodage est confié à la petite puce vidéo dédiée que toutes les cartes récentes possèdent. Le moteur 3D, celui dont tes jeux ont besoin, reste presque au repos.

Et quand ton écran ne bouge pas ? MyStream n'encode rien et n'envoie rien. Zéro.

## ✨ Ce que ça fait

| | |
| --- | --- |
| 🎙️ **Salons vocaux** | Parle avec tes potes sans délai : détection de voix ou appuyer-pour-parler, suppression du bruit |
| 🏠 **Serveurs** | Une bande de potes = un serveur, avec ses salons. Un ami peut être dans plusieurs serveurs |
| 🖥️ **Écran ou fenêtre** | Partage un écran entier ou une seule appli, curseur inclus |
| 🔊 **Le son** | Le son de ton PC, sans écho (MyStream ne capte pas son propre son) |
| 🤝 **Codes amis** | Envoie ton code `ms-…` une fois sur Discord ou WhatsApp, il est valable pour toujours |
| 🔐 **Tes règles** | Pour chaque ami : *Demander*, *Toujours autoriser* ou *Bloqué* |
| 👥 **Groupes** | « Copine », « Potes coop »… Choisis qui voit chaque partage : un ami, un groupe ou tout un serveur |
| 📸 **Captures** | Envoie une capture HD à un groupe, ou garde l'image du stream que tu regardes |
| ⚡ **3 qualités** | Éco 720p30 · Standard 1080p30 · Fluide 1080p60 |
| 🌍 **Sans serveur** | Connexion directe entre vous, chiffrée de bout en bout |

## 📊 Combien ça consomme ?

Estimations pour un écran qui bouge en permanence (le pire cas). Elles s'appuient sur nos mesures et varient selon la génération de la carte graphique.

| Qualité | Moteur 3D (jeux) | Puce vidéo dédiée | Processeur | Débit envoyé |
| --- | --- | --- | --- | --- |
| Éco 720p30 | < 1 % | ~ 3–6 % | < 1 % | ~ 1–2,5 Mb/s |
| Standard 1080p30 | < 1 % | ~ 6–12 % | < 1 % | ~ 2–5 Mb/s |
| Fluide 1080p60 | ~ 1 % | ~ 10–20 % | < 1 % | ~ 4–9 Mb/s |
| Écran immobile | 0 % | 0 % | ~ 0 % | ~ 0 |

- **Mémoire** : environ 90 Mo au repos, ~130 Mo dans un salon vocal, 200 à 300 Mo pendant un partage ou un visionnage.
- **Vocal** : ~5 % d'un cœur de processeur en parlant (suppression du bruit comprise), ~40 kb/s par personne. Latence ≈ 80 ms + le trajet réseau.
- **Côté spectateur** : environ 1 % du moteur 3D. Le décodage se fait sur la puce vidéo.
- **Plusieurs spectateurs** : l'image est encodée une seule fois. Seul le débit envoyé est multiplié par le nombre de spectateurs.

> 💡 Dans le Gestionnaire des tâches, la colonne « GPU » affiche le moteur le plus chargé. Pour voir ce qui touche vraiment tes jeux, ouvre *Performance → GPU* et regarde le graphe **3D**.

## 🚀 Démarrer en 30 secondes

1. Télécharge **`MyStream-Setup.exe`** dans les [**Releases**](../../releases) et lance-le. Il installe tout : raccourcis, et autorisation dans le pare-feu Windows (connexions directes, plus rapides).
   *Version portable* : `mystream.exe` seul marche aussi, sans installation. Accepte alors la demande du pare-feu au premier lancement.
2. Lance MyStream.
3. Clique sur **📋 Copier** à côté de ton code ami et envoie-le à tes potes.
4. Ils collent ton code dans *Ajouter un ami*. Tu acceptes.
5. Clique sur **▶ Démarrer le partage**. Tes amis voient un bouton rouge **Regarder** à côté de ton nom.

**Parler** : clique sur **+** à gauche pour créer un serveur, invite tes amis, puis clique sur un salon vocal. Dans le salon, **Partager mon écran** diffuse à tous les membres, sans demande d'autorisation.

**Astuces** : double-clic sur l'image = plein écran, `Échap` pour sortir. Clic droit sur un ami = ses droits.

**Prérequis** : Windows 10 (version 2004 ou plus récente) ou Windows 11, et une carte graphique de moins de 10 ans environ. NVIDIA, AMD et Intel sont tous pris en charge.

> 🧩 **Windows « N », vieux PC, machine virtuelle ?** Rien à installer. Sans encodeur vidéo matériel ou sans les composants multimédia de Windows, MyStream passe automatiquement sur son **moteur vidéo intégré** (OpenH264). Il utilise un peu de processeur à la place de la carte graphique (~2 % d'un PC récent), et il est limité à 720p30.

> 🔄 **Mises à jour automatiques** : au démarrage, MyStream regarde s'il existe une version plus récente sur GitHub. Si oui, un bandeau te propose de mettre à jour en un clic. Le fichier est vérifié (SHA-256), remplacé, et l'app redémarre toute seule.

## 🧠 Comment ça marche

```text
   TOI                                                 TON AMI
   ───                                                 ───────
   Capture Windows (l'image reste sur le GPU)          Réception
     → conversion couleur sur la puce vidéo              → décodage sur la puce vidéo
     → encodage H.264 matériel                           → affichage
     → envoi chiffré, en direct ou via relais  ──────▶
   Son du PC → Opus (léger, sur le processeur) ──────▶  → haut-parleurs
```

- **Pas de serveur MyStream.** Ton identité est une clé cryptographique créée sur ton PC. Tes amis te retrouvent grâce à l'infrastructure publique d'[iroh](https://iroh.computer). La connexion est directe dès que possible ; sinon elle passe par un relais qui ne voit que des données chiffrées.
- **Une image = un flux réseau indépendant.** Une image en retard ne bloque pas les suivantes. Si un spectateur décroche, il repart de l'image complète suivante, sans ralentir les autres.
- **La voix va directement de PC à PC.** Chaque participant envoie sa voix (Opus, 20 ms par paquet) à chacun des autres, sans serveur au milieu. Rien n'est enregistré.
- **Un serveur n'est hébergé nulle part.** C'est juste un nom, une liste de membres et des noms de salons, que chaque membre garde chez lui. Les copies se mettent à jour quand deux membres se croisent. Les réglages du créateur sont signés avec sa clé : personne d'autre ne peut les modifier. Pas de messages, pas d'historique.
- **L'interface dort.** Elle ne se redessine que lorsqu'il se passe quelque chose.

## 🗂️ Où sont mes fichiers ?

| Quoi | Où |
| --- | --- |
| Captures envoyées et reçues | `Images\MyStream\` |
| Amis, groupes, serveurs, réglages | `%APPDATA%\MyStream\data.json` |
| Ton identité | `%APPDATA%\MyStream\secret.key` |

> ⚠️ `secret.key` **est** ton identité. Ne le partage jamais. Si tu le perds, tes amis devront t'ajouter à nouveau.

## 🛠️ Pour les développeurs

**Compiler** : il faut [Rust](https://rustup.rs), les Visual Studio Build Tools (C++) et CMake. [NASM](https://nasm.us) est conseillé : sans lui, le moteur vidéo intégré est compilé sans ses optimisations. L'installeur se construit avec [Inno Setup](https://jrsoftware.org/isinfo.php) (`installer/mystream.iss`).

```powershell
$env:CMAKE_POLICY_VERSION_MINIMUM = "3.5"   # seulement avec CMake 4.x
cargo build --release
.\target\release\mystream.exe
```

**Commandes utiles**

| Commande | Rôle |
| --- | --- |
| `mystream selftest` | Test complet : trois identités locales via le vrai réseau (amis, partage, visionnage, capture, serveur, vocal) |
| `mystream bench [s] [fps] [hauteur] [kbps] [force]` | Capture + encodage seuls, avec statistiques |
| `mystream loopback` | Capture → encodage → décodage, enregistre `loopback.png` |
| `mystream audiotest` | Capture le son 5 s et le rejoue |
| `mystream --profile <nom>` | Lance une identité séparée (plusieurs instances sur un PC) |

Variables de test : `MYSTREAM_AUTOSTART=1` partage le premier écran au démarrage, `MYSTREAM_AUTOWATCH=1` regarde le premier ami en direct, `MYSTREAM_AUTOJOIN=1` rejoint le premier salon vocal, `MYSTREAM_VOICE_TEST=1` laisse le micro toujours ouvert, `MYSTREAM_FORCE_SOFTWARE=1` simule un Windows « N » (moteur vidéo intégré).

**Organisation du code**

```text
src/
├── main.rs          point d'entrée, commandes de dev
├── ui/              interface (egui) : mod.rs (écrans), servers.rs (serveurs, vocal), theme.rs (design system)
├── net.rs           réseau pair-à-pair (iroh) : amis, droits, diffusion, serveurs, vocal
├── store.rs         données locales et identité
├── selftest.rs      test de bout en bout
├── update.rs        mise à jour automatique (GitHub Releases)
└── media/
    ├── capture.rs   capture Windows.Graphics.Capture
    ├── d3d.rs       Direct3D 11 et conversion couleur sur GPU
    ├── pipeline.rs  capture → conversion → encodage
    ├── encoder.rs   encodeur H.264 matériel (Media Foundation)
    ├── decoder.rs   décodeur H.264 matériel
    ├── soft.rs      encodeur/décodeur OpenH264 intégré (secours)
    ├── audio.rs     capture et lecture du son, Opus
    └── voice.rs     micro, suppression du bruit, détection de voix, mixage des voix
```

## 🗺️ Et après ?

### v2 — une interface toute neuve ✅ (0.2)

- [x] Design soigné : thème sombre, polices et icônes natives de Windows, animations discrètes
- [x] Liste d'amis claire : qui est en direct, en ligne ou hors ligne, avec avatars
- [x] Sélecteur de source avec miniatures des écrans et fenêtres
- [x] Barre de contrôle flottante pendant le visionnage (volume, capture, plein écran)
- [x] Assistant de premier lancement (pseudo, code ami, premier partage)
- [x] Fenêtres claires pour ajouter un ami, accepter un spectateur, retirer un ami
- [x] Page Paramètres (pseudo, code, volume, captures, mises à jour)

### v3 — vocal et serveurs ✅ (0.3)

- [x] Salons vocaux pair-à-pair, sans serveur et sans enregistrement
- [x] Détection de voix ou appuyer-pour-parler (touche ou bouton de souris, même en jeu)
- [x] Suppression du bruit (RNNoise), test du micro, volume par personne
- [x] Serveurs : plusieurs bandes de potes, plusieurs salons, invitations
- [x] Partage d'écran à tout un serveur, bouton « Regarder » dans le salon

### Ensuite

- [ ] Notifications Windows natives (« Léa partage son écran »)
- [ ] Réduction dans la zone de notification
- [ ] Aperçu miniature du partage de tes amis
- [ ] Thème clair

### Plus tard

- [ ] Linux et macOS
- [ ] Relais auto-hébergeable, réglable dans l'app
- [ ] Choix du micro et de la sortie audio
- [ ] Annulation d'écho (pour ceux qui n'ont pas de casque)
- [ ] AV1 / HEVC sur les cartes compatibles
- [ ] Son d'une seule application quand on partage une fenêtre

## 🤝 Contribuer

Les issues et pull requests sont les bienvenues. Avant de proposer un changement, lance `mystream selftest`. Si tu touches au pipeline vidéo, vérifie la consommation avec `mystream bench 15 30 1080 5000 force` et le graphe **3D** du Gestionnaire des tâches.

## 📄 Licence

[MIT](LICENSE). Utilise-le, modifie-le, partage-le.

<div align="center">
<img src="assets/logo.svg" alt="" width="32" height="32">
<br>
<sub>Fait pour créer des souvenirs avec ses potes, pas pour faire chauffer sa carte graphique.</sub>
</div>
