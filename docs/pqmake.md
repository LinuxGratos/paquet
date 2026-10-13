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
│       ├── .footprint       <- fichiers attendus dans le paquet
│       ├── Pkgfile
│       └── nano.conf        <- fichier fourni avec la recette
├── extra/                   <- ensemble « extra »
│   └── salut/
│       ├── Pkgfile
│       ├── post-install     <- script lancé après l'installation
│       └── salut.sh
└── styles/                  <- styles de construction (build=meson...)
    └── meson
```

Le nom du répertoire est le nom de la recette ; le répertoire au-dessus est
son ensemble, qui devient le dossier de ses archives dans le dépôt
(`<dépôt>/base/`, `<dépôt>/extra/`). On peut aussi lancer `pqmake` dans un seul
ensemble (`recettes/base/`) : il contient alors directement les recettes.
Deux recettes ne peuvent pas porter le même nom, même dans deux ensembles.

### Le `Pkgfile`

Le `Pkgfile` est un script bash, à la manière des recettes CRUX : des
variables `name`, `version`, `release` et `source`, et une fonction `build()`
qui installe le paquet dans `$PKG`.

À la différence de CRUX, la description et les dépendances sont des
variables (`description`, `makedepends`, `run`) : les commentaires d'en-tête
`# Description:` et `# Depends on:` ne sont pas lus. Une recette CRUX reprise
telle quelle se construit, mais son paquet n'aura ni description ni
dépendances déclarées (seules les dépendances détectées automatiquement).

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
| `build` | style de construction, pour une recette sans fonction `build()` (voir plus bas) |
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

Une recette sans fonction `build()` utilise un style de construction (voir
[Les styles de construction](#les-styles-de-construction)).

Variables disponibles pendant la construction :

| Variable | Contenu |
|---|---|
| `$SRC` | répertoire où les sources sont déballées |
| `$PKG` | racine du paquet à remplir |
| `$PKGMK_ROOT` | copie du répertoire de la recette |
| `$PKGMK_ARCH` | architecture (`uname -m`) |
| `$MAKEFLAGS`, `$CFLAGS`, `$CXXFLAGS` | réglages de compilation |

### Les styles de construction

Beaucoup de recettes se construisent de la même façon (`./configure && make`,
`meson`, `cmake`...). Plutôt que de recopier la même fonction `build()`, une
recette peut désigner un style :

```bash
description="Moniteur système"
name=htop
version=3.5.1
source=(https://github.com/htop-dev/htop/releases/download/$version/htop-$version.tar.xz)
build=meson
```

Un style est un fichier de commandes bash, qui sert de corps à la fonction
`build()` de la recette, avec les mêmes variables (`$name`, `$version`,
`$SRC`, `$PKG`...) et le même `set -e`. Par exemple `styles/meson` :

```bash
meson setup --prefix=/usr \
            --libdir=/usr/lib \
            "$name-$version" \
            build

ninja -C build
DESTDIR="$PKG" ninja -C build install
```

Le style `<nom>` est cherché, dans cet ordre :

1. dans `<recettes>/styles/<nom>`, versionné avec les recettes ;
2. dans `STYLES_DIR/<nom>` (par défaut `/etc/pqmake/styles/<nom>`), pour des
   styles partagés par plusieurs arbres de recettes.

Une recette sans `build()` ni `build=` utilise le style `default`. S'il n'y a
pas de fichier `default` (ni `autotools` pour `build=autotools`), c'est la
construction autotools intégrée :

```bash
cd $name-$version
./configure --prefix=/usr
make
make DESTDIR=$PKG install
```

Un style introuvable est signalé dès le plan de construction (`pqmake -n`).
Une fonction `build()` dans le `Pkgfile` passe toujours avant `build=`.

#### Les styles fournis

Le dépôt de `paquet` fournit des styles prêts à l'emploi, dans
[`styles/`](../styles/). Le paquet de `paquet` les installe dans
`/etc/pqmake/styles/` :

```bash
mkdir -p $PKG/etc/pqmake/styles
cp -P $PKGMK_ROOT/styles/* $PKG/etc/pqmake/styles/    # -P garde les liens kde5 et plasma
```

| `build=` | Construction |
|---|---|
| `autotools` | `./configure --prefix=/usr --sysconfdir=/etc --localstatedir=/var --disable-static`, `make`, `make install` |
| `autoreconf` | comme `autotools`, après `autoreconf -fiv` |
| `autogen` | `./autogen.sh`, puis `./configure --prefix=/usr --disable-static`, `make`, `make install` |
| `xorg` | comme `autotools`, depuis `${_name}-$version` |
| `meson` | `meson setup --prefix=/usr --libdir=/usr/lib`, `ninja` |
| `cmake`, `kde5` | `cmake` en `Release`, `lib` comme répertoire des bibliothèques, sans les tests |
| `kde6`, `plasma` | comme `cmake`, avec `-DQT_MAJOR_VERSION=6` |
| `python3` | `python3 setup.py build` puis `install --root=$PKG`, depuis `${_name}-$version` |
| `pip3` | une roue (*wheel*) construite avec `pip3`, depuis `${_name}-$version` |

Les styles `python3`, `pip3` et `xorg` entrent dans `${_name}-$version` quand
la recette définit `_name` (le nom du projet amont, s'il diffère du nom du
paquet), sinon dans `$name-$version`. `pip3` met d'abord à jour `pip`,
`wheel` et `setuptools` dans le système de construction : il a besoin du
réseau pendant la construction.

Comme tout fichier de `/etc`, un style modifié par l'administrateur n'est pas
écrasé par une mise à jour de `paquet`. Un style du même nom dans
`<recettes>/styles/` passe avant celui de `/etc/pqmake/styles/`.

### Les scripts

Une recette peut fournir des scripts lancés par `paquet` autour de
l'installation et de la suppression de son paquet : des fichiers
`pre-install`, `post-install`, `pre-remove` et `post-remove` dans son
répertoire, à côté du `Pkgfile`, comme dans CRUX. `pqmake` les range dans
l'archive du paquet principal (pas dans les sous-paquets) :

```text
==> Packaging serveur 2.0-1
  with its post-install script
  /srv/great-os/stable/x86_64/extra/serveur-2.0-1.tar.zst
```

Leurs arguments, leur moment d'exécution et ce qui se passe s'ils échouent
sont décrits dans [Scripts et hooks](scripts.md).

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
| `STYLES_DIR` | styles de construction partagés | `/etc/pqmake/styles` |
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
| `-u`, `--update-footprint` | réécrit le footprint des recettes nommées à partir de leurs archives, sans construire (voir plus bas) |
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

### Le footprint

Le footprint d'un paquet est la liste de ses fichiers, avec leur type, leurs
droits et leur propriétaire, comme dans CRUX. Il est gardé dans le
répertoire de la recette et versionné avec elle : `.footprint` pour le
paquet principal, `.footprint.<groupe>` pour chaque sous-paquet.

```text
drwxr-xr-x	root/root	usr/
drwxr-xr-x	root/root	usr/bin/
-rwxr-xr-x	root/root	usr/bin/nano
lrwxrwxrwx	root/root	usr/bin/rnano -> nano
```

Après chaque construction, `pqmake` compare les nouvelles archives au
footprint et montre les différences. Elles ne font pas échouer la
construction : c'est au packageur de décider s'il faut corriger la recette et
reconstruire, ou accepter les changements.

```text
==> Packaging nano 8.3-1
  /srv/great-os/stable/x86_64/base/nano-8.3-1.tar.zst
==> Footprint of nano changed (base/nano/.footprint):
    CHANGED  -rwsr-xr-x root/root usr/bin/nano  (was -rwxr-xr-x: setuid added)
    MISSING  lrwxrwxrwx root/root usr/bin/rnano -> nano
    NEW      -rw-r--r-- root/root usr/share/nano/extra/zig.nanorc
==> nano.doc is no longer produced (base/nano/.footprint.doc)
    If these changes are expected: pqmake --update-footprint nano
...
==> 1 recipe(s) built
==> Footprint changed: nano (see above)
```

| Ligne | Sens |
|---|---|
| `NEW` | fichier nouveau dans le paquet |
| `MISSING` | fichier attendu qui n'y est plus |
| `CHANGED` | même chemin, mais type, droits, propriétaire ou cible de lien différents ; `(was ...)` donne l'ancienne valeur |

Les bits setuid, setgid et sticky sont signalés en clair (`setuid added`,
`setuid removed`, ou `(setuid)` sur un nouveau fichier) : un setuid perdu
casse un programme comme `su`, un setuid inattendu est un risque.

Ce que le footprint permet de repérer :
- une dépendance optionnelle détectée (ou perdue) par `configure` sur la
  machine de construction, qui ajoute ou retire des fichiers ;
- une nouvelle version qui ne fournit plus un programme, ou installe ailleurs
  (`lib64` au lieu de `lib`) ;
- un changement de droits ou de propriétaire ;
- en relecture, dans git, ce qu'une mise à jour change dans le paquet.

Pour accepter les changements, sans reconstruire :

```console
$ pqmake --update-footprint nano
  base/nano/.footprint updated
  base/nano/.footprint.doc removed
```

Un footprint absent est créé à la première construction (`new footprint:
base/nano/.footprint`). En construction chroot, il appartient au
propriétaire du `Pkgfile`, pas à root.

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
