# paquet

Gestionnaire de paquets de Great-OS, en Rust.

```sh
paquet --sync          # récupère l'index du dépôt et vérifie sa signature
paquet -s editeur      # cherche un paquet
paquet -i nano         # l'installe, avec ses dépendances
paquet -u              # met tout à jour
paquet -r nano         # le supprime
paquet autoremove      # supprime les dépendances devenues inutiles
```

`pq` est un alias de `paquet` : `pq -i nano`.

| Programme | Rôle |
|---|---|
| `paquet` (`pq`) | Installe, met à jour et supprime les paquets d'un système. |
| `genpqrepo` | Génère l'index `PQREPO` d'un miroir à partir des archives, et le signe. |
| `pqmake` | Construit les paquets à partir de leurs recettes (`Pkgfile`), dépendances d'abord. |

## Documentation

- [Utilisation de paquet](docs/utilisation.md) : configurer le dépôt, installer, mettre à jour, supprimer, réparer.
- [Construire des paquets avec pqmake](docs/pqmake.md) : écrire une recette, construire, construire en chroot.
- [Générer et publier un dépôt](docs/depot.md) : organisation du miroir, index `PQREPO`, publication.
- [Signer un dépôt](docs/signature.md) : clés, signature, installation des clés, rotation.

Le [parcours complet](docs/README.md#le-parcours-complet), de la recette au
système installé, tient en dix commandes.

## Compatibilité

C'est la reprise des scripts `paquet.sh`, `genpqrepo.sh` et `pqmake.sh`. Les
formats sont les mêmes (index, état local, listes de fichiers, journaux de
transaction, clés et signatures, archives) : un système, un miroir ou un
arbre de recettes passe d'une version à l'autre sans conversion.

## Construction

```sh
cargo build --release
install -m 755 target/release/{paquet,pq,genpqrepo,pqmake} /usr/bin/
```

Il faut Rust 1.87 ou plus récent et un compilateur C (pour zstd et la
cryptographie). Les programmes n'ont besoin ni de `curl`, ni de `zstd`, ni
d'`openssl`, ni de `readelf` : téléchargement, archives, signatures et
analyse ELF sont intégrés. `pqmake` utilise encore `bash` (les recettes sont
des scripts bash), `tar` et `unzip` pour déballer les sources, et `mount`,
`umount` et `chroot` pour les constructions en chroot.

## paquet

```text
paquet [OPTION] [--chroot <racine>]

  -S, --sync [--nosign]          récupère l'index, vérifie sa signature et met à jour l'état
  -i, --install <paquet>         installe un paquet et ses dépendances manquantes
  -l, --list                     liste les paquets
  -s, --search <terme>           cherche par nom, description, dossier ou ensemble
  -r, --remove <paquet>          supprime un paquet dont aucun paquet installé ne dépend
  -u, --upgrade [paquet]         met à jour un paquet, ou tous ceux marqués U (dépendances d'abord)
  -o, --orphans                  liste les dépendances devenues inutiles
  autoremove, --autoremove       les supprime
  --mark-auto <paquet>           marque un paquet comme installé automatiquement
  --mark-explicit <paquet>       marque un paquet comme installé explicitement
  --cache-info                   affiche le cache et sa taille
  --purge-old-cache              supprime du cache les anciennes versions
  -C, --cache                    affiche la taille du cache et propose de le vider
  --recover                      annule une opération interrompue par un plantage
  --chroot <racine>              gère le système installé sous <racine>
```

États : `N` non installé, `I` installé explicitement, `A` installé
automatiquement (dépendance) ; drapeaux : `U` mise à jour disponible, `X` n'est
plus dans le dépôt.

### Configuration

`/etc/pq.conf` (lu, jamais exécuté) :

```sh
MIRROR="https://miroir.exemple.org/great-os"
BRANCH="stable"
```

Les paquets sont pris dans `$MIRROR/$BRANCH/$(uname -m)/`. Les URL `http://`,
`https://` et `file://` sont acceptées ; les variables de proxy habituelles
(`https_proxy`, `no_proxy`...) sont respectées.

Les clés publiques de confiance sont dans `/etc/pq/keys/*.pub`. `--sync`
refuse un index sans signature valide (sauf avec `--nosign`, déconseillé), et
garde alors l'index précédent. Les variables `PQ_CONFIG` et `PQ_KEYS_DIR`
remplacent ces deux chemins.

### Fichiers

| Chemin | Contenu |
|---|---|
| `/var/lib/pq/PQREPO` | index du dépôt, tel que récupéré |
| `/var/lib/pq/PQREPOSTATE` | état de chaque paquet connu |
| `/var/lib/pq/files/<paquet>` | fichiers installés par un paquet |
| `/var/lib/pq/cache/` | archives téléchargées |
| `/var/lib/pq/transaction/` | journaux des opérations en cours |

### Sûreté

- Chaque archive est vérifiée (SHA-256 de l'index signé) avant d'être utilisée.
- Une installation qui écraserait les fichiers d'un autre paquet est refusée.
- Les fichiers de `/etc` déjà présents ne sont jamais écrasés : la nouvelle
  version est posée à côté, en `<fichier>.pqnew`. `/etc` et `/var/lib/pq` ne
  sont jamais vidés par une suppression.
- Installations et mises à jour sont transactionnelles : après une erreur ou
  un Ctrl-C, le système est remis dans son état précédent ; après un
  plantage, la commande suivante qui modifie le système (ou `--recover`)
  annule l'opération inachevée.
- Une seule instance modifie un système à la fois (verrou
  `/var/lib/pq/lock`).

## genpqrepo

```sh
genpqrepo --genkey depot              # crée depot.key (secrète) et depot.pub
genpqrepo -k depot.key /srv/miroir/stable/x86_64
genpqrepo -k depot.key /srv/miroir/stable/x86_64 base   # ne rafraîchit que base/
```

Le répertoire contient les archives `<nom>-<version>-<release>.tar.zst`, dans
des sous-dossiers (`base/`, `extra/`...). Chaque archive contient un fichier
`.META` :

```text
Description: Éditeur de texte
Groups:
Set: base
Depends: ncurses, libc
```

L'index `PQREPO` (ou `$OUTPUT_FILE`) reçoit une ligne par paquet ; si
plusieurs versions d'un même paquet sont présentes, seule la plus récente
(ordre de `sort -V`) est publiée. Avec une clé (`-k` ou `$PQ_SIGN_KEY`),
l'index est signé (`PQREPO.sig`, Ed25519). Les clés sont au format PEM
d'OpenSSL : une clé créée par `openssl genpkey -algorithm ed25519` convient,
et `openssl pkeyutl -verify -rawin` vérifie les signatures.

## pqmake

```sh
pqmake -n nano                 # affiche le plan de construction
pqmake nano                    # construit nano et ce dont il a besoin
pqmake -a -k --index           # construit tout, continue après un échec, met à jour PQREPO
pqmake --chroot /srv/rootfs nano
```

Les recettes sont rangées en `<recettes>/<ensemble>/<nom>/Pkgfile`, à la
manière des recettes CRUX (`name`, `version`, `release`, `source`, fonction
`build`), avec en plus des variables pour la description et les dépendances
(`description`, `makedepends`, `run`, `run_devel`). Les archives
vont dans `<dépôt>/<ensemble>/`. Une recette dont l'archive existe déjà pour
sa version et sa release n'est pas reconstruite (sauf `-f`).

- Les groupes (`PKGMK_GROUPS` : `devel`, `doc`, `man`, `service`, `sources`,
  `locale`) deviennent des sous-paquets `<nom>.<groupe>`.
- Les dépendances d'exécution sont détectées en lisant les bibliothèques
  dont ont besoin les fichiers ELF, et en cherchant quel paquet les fournit
  (sauf `PKGMK_IGNORE_RUNTIMEDEPS=yes`).
- Sans `--chroot`, les recettes sont construites sur l'hôte, par un
  utilisateur ordinaire. Avec `--chroot <rootfs>` (root), chaque recette est
  construite dans un overlay jetable de `<rootfs>`, où `paquet` installe
  d'abord ses `makedepends` depuis le dépôt en cours de construction. Le
  programme `paquet` copié dans le chroot est celui placé à côté de `pqmake`
  (ou `$PQMAKE_PAQUET`) : il doit pouvoir s'y exécuter, donc être lié à la
  même bibliothèque C, ou statiquement.

Les réglages viennent de `~/.config/pqmake` (ou `$PQMAKE_CONFIG`) :
`RECIPES_DIR`, `REPO_DIR`, `WORK_DIR`, `SOURCES_DIR`, `LOG_DIR`, `MAKEFLAGS`,
`CFLAGS`, `CXXFLAGS`, `PKGMK_GROUPS`, `CHROOT`.

## Différences avec les scripts bash

- Avec `--chroot`, l'index et le cache de `paquet` sont aussi pris sous la
  racine gérée (`<racine>/var/lib/pq/PQREPO` et `cache/`), au lieu de ceux du
  système hôte : gérer un chroot ne touche plus à l'index de l'hôte.
- Une archive déjà dans le cache avec la bonne somme SHA-256 n'est pas
  téléchargée de nouveau.
- `genpqrepo` accepte les paquets dont la version et la release sont égales
  (`foo-1-1.tar.zst`), que le script ignorait.
- Les liens symboliques absolus (ou passant par `..`) d'une archive sont créés après les autres
  fichiers, pour qu'aucun fichier ne puisse être écrit à travers eux.
- `paquet --help` fonctionne sans `/etc/pq.conf`.

## Tests

```sh
cargo test
```

Les tests de bout en bout installent dans un répertoire temporaire, depuis
un miroir `file://`. `PQ_TEST_CRASH` (`kill:<étape>`, `int:<étape>`,
`fail:<étape>`) simule un plantage, un Ctrl-C ou une erreur pendant une
opération, et `PQ_TEST_NONROOT=1` permet de lancer les commandes qui
modifient le système sans être root : ces deux variables ne servent qu'aux
tests.

## Licence

MIT
