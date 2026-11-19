# Watts

**Régler la limite de puissance d'une carte graphique NVIDIA sous Linux**, en un clic, et la garder au démarrage.

Moins de watts, c'est moins de chaleur dans le boîtier et moins de bruit, surtout l'été ou dans un petit boîtier (mini-ITX). Une RTX bridée à 80 % perd en général seulement quelques pourcents d'images par seconde.

![Watts](docs/screenshot.png)

## Fonctionnalités

- **En direct** : consommation et température de la carte, température du processeur, fréquence, ventilateur, avec le **maximum vu** depuis l'ouverture de l'app. Laisse Watts ouvert pendant une partie pour voir les pics.
- **Curseur** entre le minimum et le maximum autorisés par la carte, plus 4 réglages tout prêts calculés à partir de la puissance d'origine :

  | Réglage | Part de l'origine | Exemple RTX 4070 (200 W) | Perte estimée, carte à fond |
  |---|---|---|---|
  | Canicule | 70 % | 140 W | ≈ −9 % |
  | **Équilibré (recommandé)** | **80 %** | **160 W** | **≈ −4 %** |
  | Léger | 90 % | 180 W | ≈ −1,5 % |
  | D'origine | 100 % | 200 W | 0 % |

  Avec le DLSS ou un plafond de fps, la carte tourne souvent sous la limite : la perte réelle est encore plus faible.
- **Appliquer maintenant** : jusqu'au prochain redémarrage.
- **Appliquer et garder au démarrage** : crée un petit service systemd qui réapplique la limite **au démarrage et après chaque sortie de veille** (sinon le pilote revient à la valeur d'origine).
- **Retirer le réglage permanent** : supprime le service et remet la carte à sa puissance d'origine.
- Plusieurs cartes NVIDIA : un menu permet de choisir la carte, chacune a son propre réglage.

## Compatibilité

- **Bazzite** (images `-nvidia`), et en général toute distribution Linux x86_64 avec le **pilote NVIDIA propriétaire** (`nvidia-smi`), **systemd** et **pkexec** (polkit). C'est le cas de Fedora, Bazzite, Nobara, CachyOS, Ubuntu, Mint, etc.
- **Toutes les cartes NVIDIA de bureau dont le pilote autorise le réglage** : en général les GeForce GTX 900 et plus récentes, RTX 20/30/40/50. Testé sur une RTX 4070.
- **PC portables** : la plupart des GPU NVIDIA de portables bloquent ce réglage. Watts l'indique et n'affiche alors que les mesures.
- Cartes AMD / Intel : non prises en charge.

## Installation

1. Télécharge `Watts-x.y.z-linux-x86_64.AppImage` depuis les [Releases](https://github.com/VGioM/Watts/releases/latest).
2. Rends-la exécutable (clic droit → Propriétés → « Exécutable », ou `chmod +x Watts-*.AppImage`), puis lance-la.
   Sur Bazzite, tu peux aussi l'ouvrir avec **Gear Lever** pour l'ajouter au menu des applications.

Le mot de passe administrateur est demandé (fenêtre polkit) à chaque fois qu'un réglage est appliqué. La lecture des mesures n'en demande pas.

## Ce que l'app modifie sur le système

Seulement quand tu cliques sur **« Appliquer et garder au démarrage »** : un fichier par carte,

```
/etc/systemd/system/watts-gpu-power-<id>.service
```

```ini
[Service]
Type=oneshot
ExecStart=/usr/bin/nvidia-smi -i GPU-<uuid> -pl 160

[Install]
WantedBy=graphical.target suspend.target hibernate.target hybrid-sleep.target suspend-then-hibernate.target
```

Sur Bazzite, `/etc` est modifiable et conservé entre les mises à jour : le réglage survit donc aux mises à jour du système.

Pour tout enlever sans l'app :

```bash
sudo systemctl disable watts-gpu-power-<id>.service
sudo rm /etc/systemd/system/watts-gpu-power-<id>.service
sudo systemctl daemon-reload
sudo nvidia-smi -pl <puissance d'origine>   # ou redémarrer
```

Vérifier la limite actuelle : `nvidia-smi -q -d POWER`.

## Questions

**Est-ce que ça peut abîmer la carte ?** Non. On ne fait que **baisser** la puissance maximale, dans la plage que le pilote NVIDIA autorise. Ce n'est ni de l'overclocking ni de l'undervolting manuel.

**La limite est revenue à l'origine après un redémarrage ?** Ouvre Watts : s'il signale que la limite actuelle ne correspond pas au réglage permanent, clique sur « Réappliquer ». Si ça recommence, ouvre une *issue* avec la sortie de `systemctl status watts-gpu-power-*.service`.

**Et le processeur ?** Watts affiche sa température pour surveiller, mais ne le règle pas. Pour limiter un processeur Intel, cherche les réglages PL1/PL2 (« Package Power Limit ») dans le BIOS ; pour AMD, PPT / « Eco Mode ».

## Compiler

Rust stable, puis :

```bash
cargo run --release          # lancer
./build-appimage.sh          # construire dist/Watts-<version>-linux-x86_64.AppImage
```

`build-appimage.sh` a besoin de `mksquashfs` (squashfs-tools) et d'un runtime AppImage de type 2 (variable `APPIMAGE_RUNTIME`). Pour une AppImage compatible avec des systèmes plus anciens, compile dans un conteneur Fedora (distrobox) plutôt que sur un système très récent.

Interface : [egui](https://github.com/emilk/egui). Les actions administrateur passent par `pkexec` et des scripts shell qui valident leurs arguments (watts entiers, UUID de carte, nom du service).
