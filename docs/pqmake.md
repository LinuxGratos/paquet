# Construire des paquets avec pqmake

`pqmake` construit des paquets à partir de leurs recettes, dépendances
d'abord, et dépose les archives dans un dépôt prêt à être indexé par
`genpqrepo` (voir [Générer et publier un dépôt](depot.md)).

## Les recettes

Une recette est un répertoire qui contient un `Pkgfile`, rangé par
ensemble :

```text
recettes/
├── base/                    <- ensemble « base »
│   ├── libsalut/
│   │   └── Pkgfile
│   └── nano/
│       ├── Pkgfile
│       └── nano.conf        <- fichier fourni avec la recette
└── extra/                   <- ensemble « extra »
    └── salut/
        ├── Pkgfile
        └── salut.sh
```

Le nom du répertoire est le nom de la recette ; le répertoire au-dessus est
son ensemble, qui devient le dossier de ses archives dans le dépôt
(`<dépôt>/base/`, `<dépôt>/extra/`). On peut aussi lancer `pqmake` dans un seul
ensemble (`recettes/base/`) : il contient alors directement les recettes.
Deux recettes ne peuvent pas porter le même nom, même dans deux ensembles.

### Le `Pkgfile`

Le `Pkgfile` est un script bash, dans la syntaxe de NuTyX :

```bash
description="Dit bonjour"
url="https://exemple.org/salut"

name=salut
version=0.3
release=2

makedepends=(libsalut)
run=(libsalut)
source=(https://exemple.org/salut/salut-$version.tar.gz
        salut.conf)

build() {
    cd salut-$version
    ./configure --prefix=/usr --sysconfdir=/etc
    make
    make DESTDIR=$PKG install
    install -Dm644 ../salut.conf $PKG/etc/salut.conf
}
```

| Variable | Rôle |
|---|---|
| `name` | nom du paquet ; doit être celui du répertoire |
| `version` | version ; un `-` y devient `_` (`1.3-rc1` donne `1.3_rc1`) |
| `release` | numéro de construction, à augmenter quand la recette change sans changer de version (défaut : `1`) |
| `description` | description, reprise dans l'index et la recherche |
| `makedepends` | paquets nécessaires pour construire |
| `run` | dépendances à l'exécution, en plus de celles détectées automatiquement |
| `run_devel` | dépendances du sous-paquet `.devel` |
| `source` | sources : URL ou fichiers du répertoire de la recette |
| `PKGMK_GROUPS` | sous-paquets à séparer (voir plus bas) |
| `PKGMK_IGNORE_RUNTIMEDEPS=yes` | ne pas détecter les dépendances à l'exécution |

Noms et versions finissent dans des noms de fichiers : un nom n'utilise que
des lettres, des chiffres et `. _ + -` (et commence par une lettre ou un
chiffre), une version ou une release que des lettres, des chiffres et
`. _ + ~`.

Une dépendance peut viser un sous-paquet (`libsalut.devel`) et porter une
contrainte de version (`libsalut>=1.2`) ; la contrainte est conservée dans
l'index mais n'est pas encore vérifiée.

### Les étapes de construction

Les fonctions `prepare()`, `build()` et `package()`, si elles existent, sont
lancées dans cet ordre, chacune depuis le répertoire des sources (`$SRC`),
avec `set -e` : la première commande qui échoue arrête la construction.
`build()` (ou `package()`) installe le paquet dans `$PKG`, jamais dans `/`.

Sans fonction `build()`, la construction par défaut est celle des projets
autotools :

```bash
cd $name-$version
./configure --prefix=/usr
make
make DESTDIR=$PKG install
```

Variables disponibles pendant la construction :

| Variable | Contenu |
|---|---|
| `$SRC` | répertoire où les sources sont déballées |
| `$PKG` | racine du paquet à remplir |
| `$PKGMK_ROOT` | copie du répertoire de la recette |
| `$PKGMK_ARCH` | architecture (`uname -m`) |
| `$MAKEFLAGS`, `$CFLAGS`, `$CXXFLAGS` | réglages de compilation |

### Les sources

- Une URL (`https://...`) est téléchargée une seule fois, dans
  `SOURCES_DIR/<recette>/`, puis réutilisée.
- Un nom simple est un fichier du répertoire de la recette.
- Les archives (`.tar`, `.tar.*`, `.tgz`, `.tbz2`, `.txz`, `.zip`) sont
  déballées dans `$SRC` ; les autres fichiers y sont copiés tels quels.

### Les sous-paquets

Une recette peut produire, en plus du paquet principal, des sous-paquets
`<nom>.<groupe>`, chacun avec sa propre archive. Par défaut, `pqmake` sépare
les groupes `devel man doc service sources` ; une recette choisit les siens
avec `PKGMK_GROUPS=(devel man)`, ou n'en sépare aucun avec `PKGMK_GROUPS=()`.

| Groupe | Fichiers déplacés dans `<nom>.<groupe>` | Dépend de |
|---|---|---|
| `devel` | `usr/include`, `usr/lib/pkgconfig`, `usr/share/pkgconfig`, `usr/lib/cmake`, `usr/share/aclocal`, et dans `usr/lib` : `*.a`, `*.la` et les liens `*.so` | `<nom>` et `run_devel` |
| `doc` | `usr/share/doc`, `usr/share/gtk-doc`, `usr/share/info` | |
| `man` | `usr/share/man` | |
| `service` | `etc/rc.d`, `lib/systemd/system`, `usr/lib/systemd/system` | `<nom>` |
| `sources` | `usr/src` | |
| `locale` | `usr/share/locale` | |

Un groupe sans fichier ne produit pas d'archive.

### Les dépendances détectées

Pour chaque paquet produit, `pqmake` lit les bibliothèques partagées dont ont
besoin ses exécutables et ses `*.so*` (les entrées `NEEDED` des fichiers ELF),
et cherche quel paquet fournit chacune dans un répertoire de bibliothèques
(`lib`, `lib64`, `usr/lib`...) : d'abord les paquets de la recette elle-même,
puis ceux installés dans le chroot de construction, puis ceux du dépôt. Ces
paquets s'ajoutent à `run` dans le champ `Depends` de l'archive. Les
bibliothèques que personne ne fournit sont signalées et ignorées.

## Construire

### Réglages

Les réglages viennent de `~/.config/pqmake` (ou du fichier indiqué par
`$PQMAKE_CONFIG`), au format `CLÉ=valeur` :

```sh
RECIPES_DIR=~/recettes
REPO_DIR=/srv/great-os/stable/x86_64
WORK_DIR=~/pqmake/travail
SOURCES_DIR=~/pqmake/sources
LOG_DIR=~/pqmake/journaux
MAKEFLAGS=-j8
CFLAGS="-O2 -pipe"
```

| Réglage | Rôle | Par défaut |
|---|---|---|
| `RECIPES_DIR` | recettes | répertoire courant |
| `REPO_DIR` | dépôt qui reçoit les archives | `~/pqmake/repo/stable/<arch>` |
| `WORK_DIR` | répertoires de construction | `~/pqmake/work` |
| `SOURCES_DIR` | sources téléchargées | `~/pqmake/sources` |
| `LOG_DIR` | journaux de construction | `~/pqmake/logs` |
| `MAKEFLAGS` | options de `make` | `-j<nombre de processeurs>` |
| `CFLAGS`, `CXXFLAGS` | options de compilation | `-O2 -pipe` (`CXXFLAGS` = `CFLAGS`) |
| `PKGMK_GROUPS` | groupes séparés par défaut | `devel man doc service sources` |
| `CHROOT` | système de construction (voir plus bas) | aucun : construction sur l'hôte |

`-R <dir>` et `-o <dir>` remplacent `RECIPES_DIR` et `REPO_DIR` pour une
commande.

### Le plan de construction

`pqmake` part des recettes demandées, ajoute toutes celles dont elles ont
besoin (à la construction et à l'exécution), et les ordonne : une recette est
construite après ses `makedepends` et tout ce dont ceux-ci ont besoin à
l'exécution. `-n` affiche ce plan sans rien construire :

```console
$ pqmake -n salut
Build plan: 2 recipe(s), 2 to build
  build  libsalut 1.2-1
  build  salut 0.3-2
```

Une recette dont l'archive existe déjà dans le dépôt, pour sa version et sa
release, n'est pas reconstruite (`skip ... (up to date)`) : pour publier une
nouvelle construction d'une même version, on augmente `release`. `-f` force la
reconstruction des recettes nommées.

Les dépendances sans recette sont supposées fournies par le système de
construction et listées à la fin du plan. Un cycle (`a` a besoin de `b` pour
être construit, et `b` de `a`) est signalé et arrête tout ; un cycle entre
dépendances à l'exécution seulement est accepté.

### Lancer la construction

```console
$ pqmake salut
...
==> Building libsalut 1.2-1
==> build()
==> Packaging libsalut 1.2-1
  /srv/great-os/stable/x86_64/base/libsalut.devel-1.2-1.tar.zst
  /srv/great-os/stable/x86_64/base/libsalut.doc-1.2-1.tar.zst
  /srv/great-os/stable/x86_64/base/libsalut-1.2-1.tar.zst
==> Building salut 0.3-2
...
==> 2 recipe(s) built
```

| Option | Effet |
|---|---|
| `-a`, `--all` | construit toutes les recettes |
| `-n`, `--dry-run` | affiche seulement le plan |
| `-f`, `--force` | reconstruit les recettes nommées même si leur archive existe |
| `-k`, `--keep-going` | après un échec, continue avec les recettes qui n'en dépendent pas |
| `-q`, `--quiet` | n'écrit la sortie des constructions que dans les journaux (les 20 dernières lignes sont affichées en cas d'échec) |
| `--index` | régénère l'index du dépôt à la fin (signé si `PQ_SIGN_KEY` est définie) |
| `--keep-work` | garde le répertoire de travail des constructions réussies |
| `-c`, `--chroot <rootfs>` | construit dans un overlay de `<rootfs>` |

La sortie de chaque construction va dans
`LOG_DIR/<nom>-<version>-<release>.log`. Après un échec, le répertoire de
travail (`WORK_DIR/<nom>`) est gardé pour l'enquête :

```text
Error: build of salut failed (log: ~/pqmake/journaux/salut-0.3-2.log, work: ~/pqmake/travail/salut)
```

Sur l'hôte, `pqmake` refuse de tourner en root : une erreur dans `build()`
(un `$PKG` oublié) installerait directement dans le système. Les fichiers
appartiennent tout de même à root dans les archives.

### Construire en chroot

Construire sur l'hôte suppose que ses outils et bibliothèques sont ceux du
système cible, et que les `makedepends` y sont installés à la main. Avec
`--chroot <rootfs>` (ou `CHROOT=` dans les réglages), chaque recette est
construite dans un système propre :

```sh
pqmake --chroot /srv/rootfs-construction -k --index -a    # en root
```

`<rootfs>` est un système Great-OS minimal avec les outils de construction
(`bash`, compilateur, `make`...). Pour chaque recette, `pqmake` :

1. monte un overlay jetable de `<rootfs>` sous `WORK_DIR/<nom>/root`, avec
   `/proc`, `/dev` et le dépôt en lecture seule (`/pqmake/mirror`) ;
2. y installe les `makedepends` avec `paquet`, depuis le dépôt en cours de
   construction (un nom simple amène aussi son sous-paquet `.devel` s'il
   existe ; `paquet` ajoute leurs dépendances) ;
3. y lance la construction, en root, dans un environnement propre ;
4. démonte tout. `<rootfs>` n'est jamais modifié, même après un Ctrl-C.

Il faut :
- être root, et avoir `mount`, `umount` et `chroot` ;
- que `WORK_DIR` et `REPO_DIR` soient en dehors de `<rootfs>` ;
- que le programme `paquet` copié dans le chroot puisse s'y exécuter : c'est
  celui placé à côté de `pqmake` (ou `$PQMAKE_PAQUET`). Il doit être lié à la
  même bibliothèque C que `<rootfs>`, ou compilé statiquement.

L'index que `paquet` utilise dans le chroot est généré à la volée depuis les
archives locales, sans vérification de signature : il vient de la machine de
construction elle-même.

Dans les archives, les fichiers gardent les propriétaires fixés par
`build()` (qui tourne en root).
