# PowerCap

**Limiter la puissance d'une carte graphique NVIDIA sous Linux, et voir ce que consomme le PC.**

Moins de watts, c'est moins de chaleur dans le boîtier et moins de bruit, surtout l'été ou dans un petit boîtier (mini-ITX). Une RTX bridée à 80 % ne perd en général que quelques pourcents d'images par seconde.

![PowerCap](docs/screenshot.png)

## Fonctionnalités

### Consommation en direct
- **Carte graphique** : consommation mesurée par le pilote NVIDIA.
- **Processeur (estimé)** : calculé d'après sa charge et sa fréquence, et plafonné par ses limites PL1/PL2. Le compteur d'énergie exact du processeur n'est lisible qu'en administrateur, d'où l'estimation.
- **PC complet (estimé)** : carte + processeur + environ 35 W pour le reste (carte mère, mémoire, SSD, ventilateurs), avec une alimentation à environ 90 % de rendement. C'est ce que le PC tire **à la prise**, écran non compris.
- **Températures** de la carte et du processeur, et fréquences.
- Pour chaque mesure, le **maximum vu** depuis l'ouverture de l'app : laisse PowerCap ouvert pendant une partie pour voir les pics.

### Limites du processeur (lecture seule)
- **PL1** (puissance en continu), **PL2** (pointes) et **durée des pointes**, lues dans `/sys/class/powercap` telles que le BIOS les a réglées.
- Conseil pour garder le PC au frais : PL2 ≈ 1,4 × PL1. Par exemple 90 W pour un i5-13400F, au lieu de 148 W.
- Ces limites se règlent dans le BIOS : chez ASUS, mode avancé (F7), puis *Ai Tweaker*, puis *Internal CPU Power Management*. Désactive aussi *MultiCore Enhancement*.
- Processeurs AMD : leurs limites (PPT) ne sont pas lisibles de cette façon. Seules la température et l'estimation sont affichées.

### Limite de la carte graphique
- **Curseur** entre le minimum et le maximum autorisés par la carte, et 4 réglages tout prêts calculés à partir de sa puissance d'origine :

  | Réglage | Part de l'origine | Exemple RTX 4070 (200 W) | Perte estimée, carte à fond |
  |---|---|---|---|
  | Canicule | 70 % | 140 W | ≈ −9 % |
  | **Équilibré (recommandé)** | **80 %** | **160 W** | **≈ −4 %** |
  | Léger | 90 % | 180 W | ≈ −1,5 % |
  | D'origine | 100 % | 200 W | 0 % |

  Avec le DLSS ou un plafond de fps, la carte tourne souvent sous la limite : la perte réelle est encore plus faible.
- **Appliquer maintenant** : la limite vaut jusqu'au prochain redémarrage.
- **Appliquer et garder au démarrage** : crée un petit service systemd qui réapplique la limite **au démarrage et après chaque sortie de veille**. Sans lui, le pilote revient à la valeur d'origine.
- **Retirer le réglage permanent** : supprime le service et remet la carte à sa puissance d'origine.
- Avec plusieurs cartes NVIDIA, un menu permet de choisir la carte. Chacune a son propre réglage.

### Mises à jour intégrées
Au lancement, au plus une fois par jour, PowerCap regarde s'il existe une nouvelle version sur GitHub et propose de l'installer. L'AppImage est téléchargée, vérifiée (empreinte SHA-256), puis remplacée, et l'app se relance. Le bouton « Rechercher les mises à jour » est en bas de la fenêtre, et la recherche automatique peut être désactivée.

## Compatibilité

- **Bazzite** (images `-nvidia`), et en général toute distribution Linux x86_64 avec le **pilote NVIDIA propriétaire** (`nvidia-smi`), **systemd** et **pkexec** (polkit) : Fedora, Nobara, CachyOS, Ubuntu, Mint, etc.
- **Cartes NVIDIA de bureau dont le pilote autorise le réglage** : en général les GeForce GTX 900 et plus récentes, et les RTX 20/30/40/50. Testé sur une RTX 4070.
- **PC portables** : la plupart des GPU NVIDIA de portables bloquent ce réglage. PowerCap le signale et affiche seulement les mesures.
- Cartes AMD / Intel : la limite de la carte n'est pas prise en charge.

## Installation

1. Télécharge `PowerCap-x.y.z-linux-x86_64.AppImage` depuis les [Releases](https://github.com/VGioM/PowerCap/releases/latest).
2. Rends-la exécutable (clic droit, Propriétés, « Exécutable », ou `chmod +x PowerCap-*.AppImage`), puis lance-la.
   Sur Bazzite, tu peux aussi l'ouvrir avec **Gear Lever** pour l'ajouter au menu des applications.

Le mot de passe administrateur est demandé (fenêtre polkit) chaque fois qu'un réglage est appliqué. La lecture des mesures n'en demande pas.

> PowerCap s'appelait **Watts** jusqu'à la version 1.0.1. Les installations de Watts se mettent à jour d'elles-mêmes vers PowerCap, et les réglages déjà faits sont repris.

## Ce que l'app modifie sur le système

Seulement quand tu cliques sur **« Appliquer et garder au démarrage »** : un fichier par carte.

```
/etc/systemd/system/powercap-gpu-<id>.service
```

```ini
[Service]
Type=oneshot
ExecStart=/usr/bin/nvidia-smi -i GPU-<uuid> -pl 160

[Install]
WantedBy=graphical.target suspend.target hibernate.target hybrid-sleep.target suspend-then-hibernate.target
```

Sur Bazzite, `/etc` est modifiable et conservé entre les mises à jour : le réglage survit donc aux mises à jour du système. Les préférences de mise à jour sont dans `~/.config/PowerCap/update.json`.

Pour tout enlever sans l'app :

```bash
sudo systemctl disable powercap-gpu-<id>.service
sudo rm /etc/systemd/system/powercap-gpu-<id>.service
sudo systemctl daemon-reload
sudo nvidia-smi -pl <puissance d'origine>   # ou redémarrer
```

Pour vérifier la limite actuelle : `nvidia-smi -q -d POWER`.

## Questions

**Est-ce que ça peut abîmer la carte ?** Non. On ne fait que **baisser** la puissance maximale, dans la plage que le pilote NVIDIA autorise. Ce n'est ni de l'overclocking ni de l'undervolting manuel.

**La limite est revenue à l'origine après un redémarrage ?** Ouvre PowerCap : s'il signale que la limite actuelle ne correspond pas au réglage permanent, clique sur « Réappliquer ». Si ça recommence, ouvre une *issue* avec la sortie de `systemctl status powercap-gpu-*.service`.

**Les watts du processeur et du PC sont-ils exacts ?** Non, ce sont des estimations, à environ 15–20 % près. Pour une mesure exacte de la consommation à la prise, il faut un wattmètre.

## Compiler

Avec Rust stable :

```bash
cargo run --release          # lancer
./build-appimage.sh          # construire dist/PowerCap-<version>-linux-x86_64.AppImage
```

`build-appimage.sh` a besoin de `mksquashfs` (squashfs-tools) et d'un runtime AppImage de type 2 (variable `APPIMAGE_RUNTIME`). Pour une AppImage compatible avec des systèmes plus anciens, compile dans un conteneur Fedora (distrobox) plutôt que sur un système très récent.

Pour publier une nouvelle version : augmente `version` dans `Cargo.toml` (1.0.2, puis 1.0.3…), construis l'AppImage, puis crée une release GitHub `vX.Y.Z` qui contient `PowerCap-X.Y.Z-linux-x86_64.AppImage`. Les installations existantes la proposeront d'elles-mêmes.

L'interface utilise [egui](https://github.com/emilk/egui). Les actions administrateur passent par `pkexec` et des scripts shell qui valident leurs arguments (watts entiers, UUID de carte, nom du service).

## Licence

[MIT](LICENSE) : tout le monde peut reprendre, modifier et améliorer PowerCap, tant que la mention de copyright est conservée.
